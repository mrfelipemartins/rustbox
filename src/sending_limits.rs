use crate::{
    config::Config,
    db::Backend,
    error::{Error, Result},
};
use serde::Serialize;

pub const MICROSOFT_MAX_MIME_BYTES: usize = 200 * 1024 * 1024;
pub const MICROSOFT_MAX_ENCODED_REQUEST_BYTES: usize = 4 * 1024 * 1024;
pub const MICROSOFT_UPLOAD_CHUNK_BYTES: usize = 3_276_800;
pub const GOOGLE_MAX_MIME_BYTES: usize = 25 * 1024 * 1024;
pub const GOOGLE_MAX_ENCODED_REQUEST_BYTES: usize = 35_000_000;
pub const GOOGLE_MAX_THREAD_ID_BYTES: usize = 256;

#[derive(Clone, Debug, Serialize)]
pub struct SendingLimits {
    pub backend: &'static str,
    pub limit_scope: &'static str,
    pub configured_max_mime_bytes: usize,
    pub supported_path_max_mime_bytes: Option<usize>,
    pub max_mime_bytes: usize,
    pub max_encoded_request_bytes: Option<usize>,
    pub request_encoding: &'static str,
    pub encoded_request_limit_scope: &'static str,
    pub max_draft_mime_bytes: Option<usize>,
    pub max_upload_chunk_bytes: Option<usize>,
    pub max_attachment_bytes: Option<usize>,
    pub max_thread_id_bytes: Option<usize>,
    pub large_attachment_upload_supported: bool,
    pub provider_policy_may_be_lower: bool,
}

impl SendingLimits {
    pub fn for_backend(backend: Backend, config: &Config) -> Self {
        Self::with_configured_limit(backend, config.max_message_bytes)
    }

    fn with_configured_limit(backend: Backend, configured_max_mime_bytes: usize) -> Self {
        let (supported_path_max_mime_bytes, max_encoded_request_bytes, request_encoding) =
            match backend {
                Backend::Microsoft => (
                    Some(MICROSOFT_MAX_MIME_BYTES),
                    Some(MICROSOFT_MAX_ENCODED_REQUEST_BYTES),
                    "graph_draft_and_attachment_upload",
                ),
                Backend::Google => (
                    Some(GOOGLE_MAX_MIME_BYTES),
                    Some(GOOGLE_MAX_ENCODED_REQUEST_BYTES),
                    "base64url_mime_json",
                ),
                Backend::Imap => (None, None, "smtp_mime"),
            };
        Self {
            backend: backend.as_str(),
            limit_scope: "rustbox_supported_send_path",
            configured_max_mime_bytes,
            supported_path_max_mime_bytes,
            max_mime_bytes: supported_path_max_mime_bytes
                .map_or(configured_max_mime_bytes, |limit| {
                    limit.min(configured_max_mime_bytes)
                }),
            max_encoded_request_bytes,
            request_encoding,
            encoded_request_limit_scope: match backend {
                Backend::Microsoft => "per_control_request",
                Backend::Google => "entire_send_request",
                Backend::Imap => "not_applicable",
            },
            max_draft_mime_bytes: (backend == Backend::Microsoft)
                .then_some(crate::graph_message::MAX_DRAFT_MIME_BYTES),
            max_upload_chunk_bytes: (backend == Backend::Microsoft)
                .then_some(MICROSOFT_UPLOAD_CHUNK_BYTES),
            max_attachment_bytes: (backend == Backend::Microsoft)
                .then_some(crate::graph_message::MAX_ATTACHMENT_BYTES),
            max_thread_id_bytes: (backend == Backend::Google).then_some(GOOGLE_MAX_THREAD_ID_BYTES),
            large_attachment_upload_supported: backend == Backend::Microsoft,
            provider_policy_may_be_lower: true,
        }
    }

    pub fn validate_mime(&self, raw: &[u8]) -> Result<()> {
        if raw.len() > self.max_mime_bytes {
            return Err(Error::SendLimitExceeded {
                backend: self.backend,
                maximum_mime_bytes: self.max_mime_bytes,
                actual_mime_bytes: raw.len(),
            });
        }
        Ok(())
    }
}

pub fn validate(backend: Backend, config: &Config, raw: &[u8]) -> Result<()> {
    SendingLimits::for_backend(backend, config).validate_mime(raw)
}

pub fn validate_google_thread(thread: Option<&str>) -> Result<()> {
    if thread.is_some_and(|id| id.is_empty() || id.len() > GOOGLE_MAX_THREAD_ID_BYTES) {
        return Err(Error::Invalid(
            "Gmail thread ID must contain 1 to 256 bytes".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;

    #[test]
    fn graph_uses_drafts_and_uploads_instead_of_a_whole_message_http_cap() {
        let limits = SendingLimits::with_configured_limit(Backend::Microsoft, 25 * 1024 * 1024);
        let raw = vec![b'x'; 4_000_001];
        limits.validate_mime(&raw).unwrap();
        assert_eq!(limits.max_mime_bytes, 25 * 1024 * 1024);
        assert_eq!(
            limits.supported_path_max_mime_bytes,
            Some(200 * 1024 * 1024)
        );
        assert_eq!(limits.max_draft_mime_bytes, Some(3_000_000));
        assert_eq!(limits.max_attachment_bytes, Some(150_000_000));
        assert_eq!(limits.max_upload_chunk_bytes, Some(3_276_800));
        assert!(limits.large_attachment_upload_supported);
        assert_eq!(limits.request_encoding, "graph_draft_and_attachment_upload");
        assert_eq!(limits.encoded_request_limit_scope, "per_control_request");
        let maximum = SendingLimits::with_configured_limit(Backend::Microsoft, usize::MAX);
        assert_eq!(maximum.max_mime_bytes, MICROSOFT_MAX_MIME_BYTES);
    }

    #[test]
    fn lower_configuration_and_smtp_contract_are_explicit() {
        for backend in [Backend::Microsoft, Backend::Google, Backend::Imap] {
            let limits = SendingLimits::with_configured_limit(backend, 1024);
            assert_eq!(limits.max_mime_bytes, 1024);
            assert!(limits.validate_mime(&[0; 1024]).is_ok());
            assert!(limits.validate_mime(&[0; 1025]).is_err());
            assert_eq!(
                limits.large_attachment_upload_supported,
                backend == Backend::Microsoft
            );
            assert_eq!(
                limits.max_attachment_bytes.is_some(),
                backend == Backend::Microsoft
            );
        }
        let smtp = SendingLimits::with_configured_limit(Backend::Imap, 1234);
        assert!(smtp.supported_path_max_mime_bytes.is_none());
        assert!(smtp.max_encoded_request_bytes.is_none());
    }

    #[test]
    fn gmail_json_encoding_fits_supported_path_limit() {
        let limits = SendingLimits::with_configured_limit(Backend::Google, usize::MAX);
        let raw = vec![b'x'; GOOGLE_MAX_MIME_BYTES];
        limits.validate_mime(&raw).unwrap();
        let thread = "\0".repeat(GOOGLE_MAX_THREAD_ID_BYTES);
        validate_google_thread(Some(&thread)).unwrap();
        let body = json!({"raw":URL_SAFE_NO_PAD.encode(&raw),"threadId":thread});
        let encoded = serde_json::to_vec(&body).unwrap();
        assert!(encoded.len() < GOOGLE_MAX_ENCODED_REQUEST_BYTES);
        assert!(
            limits
                .validate_mime(&vec![0; GOOGLE_MAX_MIME_BYTES + 1])
                .is_err()
        );
        assert!(validate_google_thread(Some(&"x".repeat(GOOGLE_MAX_THREAD_ID_BYTES + 1))).is_err());
    }
}
