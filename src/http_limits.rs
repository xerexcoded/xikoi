//! Shared HTTP body and deadline limits for local service clients.

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use reqwest::Response;

/// TCP connect budget for Ollama, llama.cpp, and similar local servers.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Total budget for non-streaming health, listing, and metadata calls.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// Maximum JSON or transcription payload that may be buffered.
pub const MAX_JSON_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Maximum error payload that may be buffered for diagnostics.
pub const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum LimitedBodyError {
    TooLarge { limit: usize },
    Transport(reqwest::Error),
}

impl LimitedBodyError {
    pub fn message(&self) -> String {
        match self {
            Self::TooLarge { limit } => {
                format!("HTTP response exceeded {limit} bytes")
            }
            Self::Transport(error) => error.to_string(),
        }
    }
}

#[must_use]
pub fn content_length_exceeds(content_length: Option<u64>, limit: usize) -> bool {
    content_length.is_some_and(|length| length > limit as u64)
}

/// Read a response body while rejecting oversized `Content-Length` values and
/// bounding unknown-length streams before they are fully materialized.
pub async fn read_limited_body(
    response: Response,
    limit: usize,
) -> Result<Bytes, LimitedBodyError> {
    if content_length_exceeds(response.content_length(), limit) {
        return Err(LimitedBodyError::TooLarge { limit });
    }

    let mut stream = response.bytes_stream();
    let mut body = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(LimitedBodyError::Transport)?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(LimitedBodyError::TooLarge { limit });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_content_length_is_rejected_before_allocation() {
        assert!(content_length_exceeds(
            Some(MAX_JSON_BODY_BYTES as u64 + 1),
            MAX_JSON_BODY_BYTES
        ));
        assert!(!content_length_exceeds(
            Some(MAX_JSON_BODY_BYTES as u64),
            MAX_JSON_BODY_BYTES
        ));
        assert!(!content_length_exceeds(None, MAX_JSON_BODY_BYTES));
    }
}
