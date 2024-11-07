use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::object_client::{
    ObjectClientResult, PutObjectError, PutObjectParams, PutObjectRequest, PutObjectResult, PutObjectSingleParams,
};
use async_trait::async_trait;
use futures::channel::oneshot::{self, Receiver};
use mountpoint_s3_crt::http::request_response::{Header, Headers, HeadersError};
use mountpoint_s3_crt::io::stream::InputStream;
use mountpoint_s3_crt::s3::client::{ChecksumConfig, RequestType, UploadReview};
use tracing::error;

use super::{
    emit_throughput_metric, ETag, PutObjectTrailingChecksums, S3CrtClient, S3CrtClientInner, S3HttpRequest, S3Message,
    S3Operation, S3RequestError,
};

const ETAG_HEADER_NAME: &str = "ETag";
const SSE_TYPE_HEADER_NAME: &str = "x-amz-server-side-encryption";
const SSE_KEY_ID_HEADER_NAME: &str = "x-amz-server-side-encryption-aws-kms-key-id";

impl S3CrtClient {
    pub(super) async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        params: &PutObjectParams,
    ) -> ObjectClientResult<S3PutObjectRequest, PutObjectError, S3RequestError> {
        let span = request_span!(self.inner, "put_object", bucket, key);
        let mut message = self.new_put_request(
            bucket,
            key,
            params.storage_class.as_deref(),
            params.server_side_encryption.as_deref(),
            params.ssekms_key_id.as_deref(),
        )?;

        let checksum_config = match params.trailing_checksums {
            PutObjectTrailingChecksums::Enabled => Some(ChecksumConfig::trailing_crc32c()),
            PutObjectTrailingChecksums::ReviewOnly => Some(ChecksumConfig::upload_review_crc32c()),
            PutObjectTrailingChecksums::Disabled => None,
        };
        message.set_checksum_config(checksum_config);

        for (name, value) in &params.object_metadata {
            message
                .set_header(&Header::new(format!("x-amz-meta-{}", name), value))
                .map_err(S3RequestError::construction_failure)?
        }
        for (name, value) in &params.custom_headers {
            message
                .inner
                .add_header(&Header::new(name, value))
                .map_err(S3RequestError::construction_failure)?;
        }

        let review_callback = ReviewCallbackBox::default();
        let callback = review_callback.clone();

        let (on_headers, response_headers) = response_headers_handler();

        let mut options = S3CrtClientInner::new_meta_request_options(message, S3Operation::PutObject);
        options.send_using_async_writes(true);
        options.on_upload_review(move |review| callback.invoke(review));
        options.part_size(self.inner.write_part_size as u64);

        // Before the first write, we need to await for the multi-part upload to be created, so we can report errors.
        // To do so, we need to detect one of two events (whichever comes first):
        // * a CreateMultipartUpload request completes successfully (potentially after a number of retries),
        // * the meta-request fails.
        let (mpu_created_sender, mpu_created) = oneshot::channel();
        let on_mpu_created_sender = Arc::new(Mutex::new(Some(mpu_created_sender)));
        let on_error_sender = on_mpu_created_sender.clone();

        let body = self.inner.make_simple_http_request_from_options(
            options,
            span,
            move |metrics| {
                if metrics.request_type() == RequestType::CreateMultipartUpload && !metrics.error().is_err() {
                    // Signal that a CreateMultipartUpload completed successfully (unless the meta-request had already failed).
                    if let Some(sender) = on_mpu_created_sender.lock().unwrap().take() {
                        _ = sender.send(Ok(()));
                    }
                }
            },
            move |result| {
                // Signal that the meta-request failed (unless a CreateMultipartUpload had already completed successfully).
                if let Some(sender) = on_error_sender.lock().unwrap().take() {
                    _ = sender.send(Err(result.crt_error.into()));
                }
                None
            },
            on_headers,
        )?;

        Ok(S3PutObjectRequest {
            body,
            review_callback,
            start_time: Instant::now(),
            total_bytes: 0,
            response_headers,
            state: S3PutObjectRequestState::CreatingMPU(mpu_created),
        })
    }

    pub(super) async fn put_object_single<'a>(
        &self,
        bucket: &str,
        key: &str,
        params: &PutObjectSingleParams,
        contents: impl AsRef<[u8]> + Send + 'a,
    ) -> ObjectClientResult<PutObjectResult, PutObjectError, S3RequestError> {
        let span = request_span!(self.inner, "put_object_single", bucket, key);
        let start_time = Instant::now();

        let (on_headers, response_headers) = response_headers_handler();
        let slice = contents.as_ref();
        let content_length = slice.len();
        let body = {
            let mut message = self.new_put_request(
                bucket,
                key,
                params.storage_class.as_deref(),
                params.server_side_encryption.as_deref(),
                params.ssekms_key_id.as_deref(),
            )?;
            message
                .set_content_length_header(content_length)
                .map_err(S3RequestError::construction_failure)?;
            if let Some(checksum) = &params.checksum {
                message
                    .set_checksum_header(checksum)
                    .map_err(S3RequestError::construction_failure)?;
            }
            for (name, value) in &params.object_metadata {
                message
                    .set_header(&Header::new(format!("x-amz-meta-{}", name), value))
                    .map_err(S3RequestError::construction_failure)?
            }
            for (name, value) in &params.custom_headers {
                message
                    .inner
                    .add_header(&Header::new(name, value))
                    .map_err(S3RequestError::construction_failure)?;
            }

            let body_input_stream =
                InputStream::new_from_slice(&self.inner.allocator, slice).map_err(S3RequestError::CrtError)?;
            message.set_body_stream(Some(body_input_stream));

            let options = S3CrtClientInner::new_meta_request_options(message, S3Operation::PutObjectSingle);
            self.inner
                .make_simple_http_request_from_options(options, span, |_| {}, |_| None, on_headers)?
        };

        body.await?;

        let elapsed = start_time.elapsed();
        emit_throughput_metric(content_length as u64, elapsed, "put_object_single");

        Ok(extract_result(response_headers.await.expect(
            "headers should be available since the request completed successfully",
        ))?)
    }

    fn new_put_request(
        &self,
        bucket: &str,
        key: &str,
        storage_class: Option<&str>,
        server_side_encryption: Option<&str>,
        ssekms_key_id: Option<&str>,
    ) -> Result<S3Message<'_>, S3RequestError> {
        let mut message = self
            .inner
            .new_request_template("PUT", bucket)
            .map_err(S3RequestError::construction_failure)?;

        let key = format!("/{key}");
        message
            .set_request_path(&key)
            .map_err(S3RequestError::construction_failure)?;

        if let Some(storage_class) = storage_class {
            message
                .set_header(&Header::new("x-amz-storage-class", storage_class))
                .map_err(S3RequestError::construction_failure)?;
        }

        if let Some(sse) = server_side_encryption {
            message
                .set_header(&Header::new(SSE_TYPE_HEADER_NAME, sse))
                .map_err(S3RequestError::construction_failure)?;
        }
        if let Some(key_id) = ssekms_key_id {
            message
                .set_header(&Header::new(SSE_KEY_ID_HEADER_NAME, key_id))
                .map_err(S3RequestError::construction_failure)?;
        }

        Ok(message)
    }
}

type ReviewCallback = dyn FnOnce(UploadReview) -> bool + Send;

/// Holder for the upload review callback.
/// Used to set the callback when initiating the PutObject request on the CRT client,
/// but redirects to the actual callback the user can specify at completion time.
#[derive(Clone, Default)]
struct ReviewCallbackBox {
    callback: Arc<Mutex<Option<Box<ReviewCallback>>>>,
}

impl std::fmt::Debug for ReviewCallbackBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewCallbackBox").finish()
    }
}

impl ReviewCallbackBox {
    fn set(&mut self, callback: impl FnOnce(UploadReview) -> bool + Send + 'static) {
        let previous = self.callback.lock().unwrap().replace(Box::new(callback));
        assert!(previous.is_none(), "review callback set twice");
    }

    fn invoke(self, review: UploadReview) -> bool {
        let mut callback = self.callback.lock().unwrap();
        let Some(callback) = callback.take() else {
            error!("review callback was either never set or invoked twice");
            return false;
        };

        (callback)(review)
    }
}

/// An in-progress streaming PutObject request to S3.
///
/// You can write to or complete the upload using the [`PutObjectRequest`] implementation on this
/// object.
#[derive(Debug)]
pub struct S3PutObjectRequest {
    body: S3HttpRequest<Vec<u8>, PutObjectError>,
    review_callback: ReviewCallbackBox,
    start_time: Instant,
    total_bytes: u64,
    /// Future for the headers of the CompleteMultipartUpload response.
    /// Guaranteed to be available after the request finishes successfully.
    response_headers: Receiver<Headers>,
    state: S3PutObjectRequestState,
}

/// Internal state for a [S3PutObjectRequest].
#[derive(Debug)]
enum S3PutObjectRequestState {
    /// Initial state indicating that CreateMultipartUpload may still be in progress. To be awaited on first
    /// write so errors can be reported early. The signal indicates that CreateMultipartUpload completed
    /// successfully, or that the MPU failed.
    CreatingMPU(oneshot::Receiver<Result<(), S3RequestError>>),
    /// A write operation is in progress or was interrupted before completion.
    PendingWrite,
    /// Idle state between write calls.
    Idle,
}

fn get_etag(response_headers: &Headers) -> Result<ETag, HeadersError> {
    Ok(response_headers.get_as_string(ETAG_HEADER_NAME)?.into())
}

fn extract_result(response_headers: Headers) -> Result<PutObjectResult, S3RequestError> {
    fn extract_result_headers_err(response_headers: Headers) -> Result<PutObjectResult, HeadersError> {
        Ok(PutObjectResult {
            etag: get_etag(&response_headers)?,
            sse_type: response_headers.get_as_optional_string(SSE_TYPE_HEADER_NAME)?,
            sse_kms_key_id: response_headers.get_as_optional_string(SSE_KEY_ID_HEADER_NAME)?,
        })
    }
    extract_result_headers_err(response_headers).map_err(|e| S3RequestError::InternalError(Box::new(e)))
}

/// Creates `on_headers` callback that will send the response headers to the matching `Receiver`.
fn response_headers_handler() -> (impl FnMut(&Headers, i32), Receiver<Headers>) {
    let (response_headers_sender, response_headers) = oneshot::channel();
    // The callback signature (`FnMut`) allows for it to be invoked multiple times,
    // but for PUT requests it will only be called once (on CompleteMultipartUpload
    // or on PutObject).
    // Wrapping the `oneshot::Sender` in an `Option` allows it to be consumed
    // on the first (and only!) invocation.
    let mut response_headers_sender = Some(response_headers_sender);
    let on_headers = move |headers: &Headers, _: i32| {
        if let Some(sender) = response_headers_sender.take() {
            let _ = sender.send(headers.clone());
        }
    };
    (on_headers, response_headers)
}

#[cfg_attr(not(docsrs), async_trait)]
impl PutObjectRequest for S3PutObjectRequest {
    type ClientError = S3RequestError;

    async fn write(&mut self, slice: &[u8]) -> ObjectClientResult<(), PutObjectError, Self::ClientError> {
        // Writing to the meta request may require multiple calls. Set the internal
        // state to `PendingWrite` until we are done.
        match std::mem::replace(&mut self.state, S3PutObjectRequestState::PendingWrite) {
            S3PutObjectRequestState::CreatingMPU(create_mpu) => {
                // On first write, check the pending CreateMultipartUpload so we can report errors.
                // Wait for CreateMultipartUpload to complete successfully, or the MPU to fail.
                create_mpu.await.unwrap()?;
            }
            S3PutObjectRequestState::PendingWrite => {
                // Fail if a previous write was not completed.
                return Err(S3RequestError::RequestCanceled.into());
            }
            S3PutObjectRequestState::Idle => {}
        }

        let meta_request = &mut self.body.meta_request;
        let mut slice = slice;
        while !slice.is_empty() {
            // Write will fail if the request has already finished (because of an error).
            let remaining = meta_request
                .write(slice, false)
                .await
                .map_err(S3RequestError::CrtError)?;
            self.total_bytes += (slice.len() - remaining.len()) as u64;
            slice = remaining;
        }
        // Write completed with no errors, we can reset to `Idle`.
        self.state = S3PutObjectRequestState::Idle;
        Ok(())
    }

    async fn complete(self) -> ObjectClientResult<PutObjectResult, PutObjectError, Self::ClientError> {
        self.review_and_complete(|_| true).await
    }

    async fn review_and_complete(
        mut self,
        review_callback: impl FnOnce(UploadReview) -> bool + Send + 'static,
    ) -> ObjectClientResult<PutObjectResult, PutObjectError, Self::ClientError> {
        // No need to check for `CreatingMPU`: errors will be reported on completing the upload.
        if matches!(self.state, S3PutObjectRequestState::PendingWrite) {
            // Fail if a previous write was not completed.
            return Err(S3RequestError::RequestCanceled.into());
        }

        self.review_callback.set(review_callback);

        // Write will fail if the request has already finished (because of an error).
        _ = self
            .body
            .meta_request
            .write(&[], true)
            .await
            .map_err(S3RequestError::CrtError)?;

        // Now wait for the request to finish.
        let _ = self.body.await?;

        let elapsed = self.start_time.elapsed();
        emit_throughput_metric(self.total_bytes, elapsed, "put_object");

        Ok(extract_result(self.response_headers.await.expect(
            "headers should be available since the request completed successfully",
        ))?)
    }
}

impl S3PutObjectRequest {
    /// The number of bytes written to this request so far.
    // TODO: consider exposing on the `PutObjectRequest` trait.
    pub fn bytes_written(&self) -> u64 {
        self.total_bytes
    }
}
