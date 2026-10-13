//! OpenAI-compatible client for the local MLX-Audio sidecar.

use std::time::Duration;

use reqwest::{Client, Response, StatusCode, multipart};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::http_limits::{
    CancellableBodyError, LimitedBodyError, MAX_ERROR_BODY_BYTES, MAX_JSON_BODY_BYTES,
    read_limited_body, read_limited_body_cancellable,
};

const REQUEST_TIMEOUT: Duration = Duration::from_mins(30);

#[derive(Clone, Debug)]
pub struct MlxAudioClient {
    client: Client,
    base_url: Url,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpeechServerInfo {
    pub endpoint: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptionRequest {
    pub model: String,
    pub wav: Vec<u8>,
    pub language: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SynthesisRequest {
    pub model: String,
    pub input: String,
    pub voice: String,
    pub response_format: &'static str,
    pub stream: bool,
    pub streaming_interval: f32,
}

impl SynthesisRequest {
    pub fn qwen(
        model: impl Into<String>,
        input: impl Into<String>,
        voice: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            input: input.into(),
            voice: voice.into(),
            response_format: "pcm",
            stream: true,
            streaming_interval: 0.5,
        }
    }
}

#[derive(Debug, Deserialize)]
struct TranscriptionResponse {
    text: String,
}

impl MlxAudioClient {
    pub fn new(base_url: Url) -> Result<Self, SpeechFailure> {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(safe_transport)?;
        Ok(Self { client, base_url })
    }

    pub async fn probe(&self) -> Result<SpeechServerInfo, SpeechFailure> {
        let endpoint = self.endpoint("")?;
        let response = self
            .client
            .get(endpoint)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map_err(safe_transport)?;
        ensure_success(response).await?;
        Ok(SpeechServerInfo {
            endpoint: crate::config::redact_endpoint(&self.base_url),
        })
    }

    pub async fn transcribe(
        &self,
        request: TranscriptionRequest,
        cancellation: &CancellationToken,
    ) -> Result<String, SpeechFailure> {
        if cancellation.is_cancelled() {
            return Err(SpeechFailure::Cancelled);
        }

        let audio = multipart::Part::bytes(request.wav)
            .file_name("xikoi-capture.wav")
            .mime_str("audio/wav")
            .map_err(safe_transport)?;
        let mut form = multipart::Form::new()
            .text("model", request.model)
            .text("response_format", "json")
            .text("stream", "false")
            .part("file", audio);
        if let Some(language) = request.language {
            form = form.text("language", language);
        }

        let dispatch = self
            .client
            .post(self.endpoint("v1/audio/transcriptions")?)
            .multipart(form)
            .send();
        let response = tokio::select! {
            () = cancellation.cancelled() => return Err(SpeechFailure::Cancelled),
            response = dispatch => response.map_err(safe_transport)?,
        };
        if cancellation.is_cancelled() {
            return Err(SpeechFailure::Cancelled);
        }
        let response = ensure_success_cancellable(response, cancellation).await?;
        if cancellation.is_cancelled() {
            return Err(SpeechFailure::Cancelled);
        }
        let body = read_limited_body_cancellable(
            response,
            MAX_JSON_BODY_BYTES,
            cancellation,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(speech_cancellable_body_failure)?;
        if cancellation.is_cancelled() {
            return Err(SpeechFailure::Cancelled);
        }
        let parsed = serde_json::from_slice::<TranscriptionResponse>(&body).map_err(|error| {
            SpeechFailure::Protocol(format!("MLX-Audio returned invalid JSON: {error}"))
        })?;
        let text = parsed.text.trim().to_owned();
        if text.is_empty() {
            return Err(SpeechFailure::Protocol(
                "MLX-Audio returned an empty transcription".to_owned(),
            ));
        }
        Ok(text)
    }

    pub async fn synthesize(
        &self,
        request: &SynthesisRequest,
        cancellation: &CancellationToken,
    ) -> Result<Response, SpeechFailure> {
        if cancellation.is_cancelled() {
            return Err(SpeechFailure::Cancelled);
        }
        let dispatch = self
            .client
            .post(self.endpoint("v1/audio/speech")?)
            .json(request)
            .send();
        let response = tokio::select! {
            () = cancellation.cancelled() => return Err(SpeechFailure::Cancelled),
            response = dispatch => response.map_err(safe_transport)?,
        };
        ensure_success_cancellable(response, cancellation).await
    }

    fn endpoint(&self, path: &str) -> Result<Url, SpeechFailure> {
        self.base_url
            .join(path)
            .map_err(|error| SpeechFailure::Protocol(error.to_string()))
    }
}

async fn ensure_success(response: Response) -> Result<Response, SpeechFailure> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = read_limited_body(response, MAX_ERROR_BODY_BYTES)
        .await
        .map_err(speech_body_failure)?;
    Err(SpeechFailure::Server {
        status,
        message: bounded_body(&body),
    })
}

async fn ensure_success_cancellable(
    response: Response,
    cancellation: &CancellationToken,
) -> Result<Response, SpeechFailure> {
    if cancellation.is_cancelled() {
        return Err(SpeechFailure::Cancelled);
    }
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = read_limited_body_cancellable(
        response,
        MAX_ERROR_BODY_BYTES,
        cancellation,
        REQUEST_TIMEOUT,
    )
    .await
    .map_err(speech_cancellable_body_failure)?;
    Err(SpeechFailure::Server {
        status,
        message: bounded_body(&body),
    })
}

fn speech_body_failure(error: LimitedBodyError) -> SpeechFailure {
    match error {
        LimitedBodyError::TooLarge { limit } => SpeechFailure::Protocol(format!(
            "MLX-Audio returned a response larger than {limit} bytes"
        )),
        LimitedBodyError::Transport(error) => safe_transport(error),
    }
}

fn safe_transport(error: reqwest::Error) -> SpeechFailure {
    SpeechFailure::Transport(error.without_url())
}

fn speech_cancellable_body_failure(error: CancellableBodyError) -> SpeechFailure {
    match error {
        CancellableBodyError::Cancelled => SpeechFailure::Cancelled,
        CancellableBodyError::TimedOut => SpeechFailure::Timeout,
        CancellableBodyError::Body(error) => speech_body_failure(error),
    }
}

fn bounded_body(body: &[u8]) -> String {
    const LIMIT: usize = 512;
    let text = String::from_utf8_lossy(body);
    let mut chars = text.chars();
    let result = chars.by_ref().take(LIMIT).collect::<String>();
    if chars.next().is_some() {
        format!("{result}…")
    } else if result.trim().is_empty() {
        "empty response".to_owned()
    } else {
        result
    }
}

#[derive(Debug, Error)]
pub enum SpeechFailure {
    #[error("speech request was cancelled")]
    Cancelled,
    #[error("could not reach MLX-Audio: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("MLX-Audio response body timed out")]
    Timeout,
    #[error("MLX-Audio protocol error: {0}")]
    Protocol(String),
    #[error("MLX-Audio returned HTTP {status}: {message}")]
    Server { status: StatusCode, message: String },
}

#[cfg(test)]
mod tests {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    #[tokio::test]
    async fn probes_and_transcribes_through_openai_compatible_routes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": "MLX Audio"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "text": " local speech "
            })))
            .mount(&server)
            .await;

        let client = MlxAudioClient::new(Url::parse(&server.uri()).unwrap()).unwrap();
        assert!(client.probe().await.is_ok());
        let text = client
            .transcribe(
                TranscriptionRequest {
                    model: "qwen-asr".to_owned(),
                    wav: b"not-a-real-wav".to_vec(),
                    language: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(text, "local speech");
    }

    #[test]
    fn qwen_synthesis_requests_stream_raw_pcm() {
        let request = SynthesisRequest::qwen("qwen-tts", "hello", "Vivian");
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["response_format"], "pcm");
        assert_eq!(value["stream"], true);
        assert_eq!(value["streaming_interval"], 0.5);
    }

    #[tokio::test]
    async fn cancelled_transcription_does_not_return_late_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(400))
                    .set_body_json(serde_json::json!({ "text": "late text" })),
            )
            .mount(&server)
            .await;

        let client = MlxAudioClient::new(Url::parse(&server.uri()).unwrap()).unwrap();
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            client
                .transcribe(
                    TranscriptionRequest {
                        model: "qwen-asr".to_owned(),
                        wav: b"not-a-real-wav".to_vec(),
                        language: None,
                    },
                    &cancellation,
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let result = task.await.unwrap();
        assert!(matches!(result, Err(SpeechFailure::Cancelled)));
    }

    #[tokio::test]
    async fn transcription_rejects_oversized_error_bodies() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_raw(vec![b'x'; MAX_ERROR_BODY_BYTES + 8], "text/plain"),
            )
            .mount(&server)
            .await;

        let client = MlxAudioClient::new(Url::parse(&server.uri()).unwrap()).unwrap();
        let error = client
            .transcribe(
                TranscriptionRequest {
                    model: "qwen-asr".to_owned(),
                    wav: b"not-a-real-wav".to_vec(),
                    language: None,
                },
                &CancellationToken::new(),
            )
            .await
            .expect_err("oversized transcription error must fail");
        assert!(matches!(error, SpeechFailure::Protocol(message) if message.contains("bytes")));
    }
}
