//! OpenAI-compatible client for the local MLX-Audio sidecar.

use std::time::Duration;

use bytes::Bytes;
use reqwest::{Client, Response, StatusCode, multipart};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use url::Url;

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
            .map_err(SpeechFailure::Transport)?;
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
            .map_err(SpeechFailure::Transport)?;
        ensure_success(response).await?;
        Ok(SpeechServerInfo {
            endpoint: self.base_url.to_string(),
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
            .map_err(SpeechFailure::Transport)?;
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
            response = dispatch => response.map_err(SpeechFailure::Transport)?,
        };
        let response = ensure_success(response).await?;
        let parsed = response
            .json::<TranscriptionResponse>()
            .await
            .map_err(SpeechFailure::Transport)?;
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
            response = dispatch => response.map_err(SpeechFailure::Transport)?,
        };
        ensure_success(response).await
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
    let body = response.bytes().await.map_err(SpeechFailure::Transport)?;
    Err(SpeechFailure::Server {
        status,
        message: bounded_body(&body),
    })
}

fn bounded_body(body: &Bytes) -> String {
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
}
