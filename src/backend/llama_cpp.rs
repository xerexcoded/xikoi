use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::BytesMut;
use futures_util::StreamExt;
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{ChatBackend, ChatEventReceiver};
use crate::{
    domain::{
        BackendErrorKind, BackendFailure, BackendInfo, ChatEvent, ChatRequest, ModelSummary,
        SelectedModel, Usage,
    },
    http_limits::{
        CONNECT_TIMEOUT, LimitedBodyError, MAX_ERROR_BODY_BYTES, MAX_JSON_BODY_BYTES,
        REQUEST_TIMEOUT, read_limited_body,
    },
};

const CHAT_EVENT_CHANNEL_CAPACITY: usize = 256;
const MAX_SSE_LINE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct LlamaCppBackend {
    base_url: Url,
    client: Client,
    request_timeout: Duration,
}

impl LlamaCppBackend {
    pub fn new(base_url: Url) -> Result<Self, BackendFailure> {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("xikoi/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| unavailable(format!("could not create llama.cpp client: {error}")))?;
        Ok(Self {
            base_url: normalize_base_url(base_url),
            client,
            request_timeout: REQUEST_TIMEOUT,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, BackendFailure> {
        self.base_url
            .join(path.trim_start_matches('/'))
            .map_err(|error| protocol(format!("invalid llama.cpp endpoint URL: {error}")))
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, BackendFailure> {
        let response = self
            .client
            .get(self.endpoint(path)?)
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(|error| self.transport_failure(&error))?;
        self.decode_json(response).await
    }

    async fn decode_json<T: DeserializeOwned>(
        &self,
        response: Response,
    ) -> Result<T, BackendFailure> {
        let status = response.status();
        let limit = if status.is_success() {
            MAX_JSON_BODY_BYTES
        } else {
            MAX_ERROR_BODY_BYTES
        };
        let body = read_limited_body(response, limit)
            .await
            .map_err(|error| self.limited_body_failure(error))?;
        if !status.is_success() {
            return Err(status_failure(status, &body));
        }
        serde_json::from_slice(&body)
            .map_err(|error| protocol(format!("llama.cpp returned invalid JSON: {error}")))
    }

    fn limited_body_failure(&self, error: LimitedBodyError) -> BackendFailure {
        match error {
            LimitedBodyError::TooLarge { limit } => protocol(format!(
                "llama.cpp returned a response larger than {limit} bytes"
            )),
            LimitedBodyError::Transport(error) => self.transport_failure(&error),
        }
    }

    fn transport_failure(&self, error: &reqwest::Error) -> BackendFailure {
        let kind = if error.is_connect() || error.is_timeout() {
            BackendErrorKind::Unavailable
        } else {
            BackendErrorKind::Server
        };
        BackendFailure {
            kind,
            message: format!(
                "llama.cpp is unavailable at {}: {error}",
                self.base_url.as_str().trim_end_matches('/')
            ),
        }
    }
}

#[async_trait]
impl ChatBackend for LlamaCppBackend {
    async fn probe(&self) -> Result<BackendInfo, BackendFailure> {
        let health: HealthResponse = self.get_json("health").await?;
        if !matches!(health.status.as_str(), "ok" | "ready") {
            return Err(unavailable(format!(
                "llama.cpp server is not ready: {}",
                health.status
            )));
        }
        Ok(BackendInfo {
            version: "llama.cpp".to_owned(),
            endpoint: self.base_url.as_str().trim_end_matches('/').to_owned(),
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelSummary>, BackendFailure> {
        let response: ModelsResponse = self.get_json("v1/models").await?;
        Ok(response
            .data
            .into_iter()
            .filter(|model| !model.id.trim().is_empty())
            .map(|model| ModelSummary {
                name: model.id,
                capabilities: vec!["completion".to_owned()],
                ..ModelSummary::default()
            })
            .collect())
    }

    async fn running_models(&self) -> Result<Vec<SelectedModel>, BackendFailure> {
        Ok(Vec::new())
    }

    async fn show_model(&self, model: &str) -> Result<SelectedModel, BackendFailure> {
        let summary = self
            .list_models()
            .await?
            .into_iter()
            .find(|candidate| candidate.name == model)
            .ok_or_else(|| BackendFailure {
                kind: BackendErrorKind::MissingModel,
                message: format!("llama.cpp does not expose model `{model}`"),
            })?;
        Ok(SelectedModel {
            summary,
            ..SelectedModel::default()
        })
    }

    async fn stream_chat(
        &self,
        request: ChatRequest,
        cancellation: CancellationToken,
    ) -> Result<ChatEventReceiver, BackendFailure> {
        let (events, receiver) = mpsc::channel(CHAT_EVENT_CHANNEL_CAPACITY);
        let payload = ChatPayload::from(&request);
        let started = Instant::now();
        let dispatch = self
            .client
            .post(self.endpoint("v1/chat/completions")?)
            .json(&payload)
            .send();
        let response = tokio::select! {
            () = cancellation.cancelled() => {
                let _ = events.send(ChatEvent::Cancelled).await;
                return Ok(receiver);
            }
            response = dispatch => response.map_err(|error| self.transport_failure(&error))?,
        };
        if !response.status().is_success() {
            let status = response.status();
            let body = read_limited_body(response, MAX_ERROR_BODY_BYTES)
                .await
                .map_err(|error| self.limited_body_failure(error))?;
            return Err(status_failure(status, &body));
        }
        tokio::spawn(consume_sse(response, events, cancellation, started));
        Ok(receiver)
    }
}

async fn consume_sse(
    response: Response,
    events: mpsc::Sender<ChatEvent>,
    cancellation: CancellationToken,
    started: Instant,
) {
    let mut bytes = response.bytes_stream();
    let mut framer = SseFramer::default();
    let mut usage = Usage::default();
    let mut finish_reason = None;
    let mut emitted_content = false;

    loop {
        let next = tokio::select! {
            () = cancellation.cancelled() => {
                let _ = events.send(ChatEvent::Cancelled).await;
                return;
            }
            next = bytes.next() => next,
        };
        match next {
            Some(Ok(chunk)) => {
                let lines = match framer.push(&chunk) {
                    Ok(lines) => lines,
                    Err(error) => {
                        let _ = events.send(ChatEvent::Failed(error)).await;
                        return;
                    }
                };
                for line in lines {
                    match consume_sse_line(
                        &line,
                        &events,
                        &cancellation,
                        started,
                        &mut usage,
                        &mut finish_reason,
                        &mut emitted_content,
                    )
                    .await
                    {
                        Ok(LineResult::Continue) => {}
                        Ok(LineResult::Done) => return,
                        Err(error) => {
                            let _ = events.send(ChatEvent::Failed(error)).await;
                            return;
                        }
                    }
                }
            }
            Some(Err(error)) => {
                let _ = events
                    .send(ChatEvent::Failed(unavailable(format!(
                        "llama.cpp stream failed: {error}"
                    ))))
                    .await;
                return;
            }
            None => {
                match framer.finish() {
                    Ok(Some(line)) => {
                        match consume_sse_line(
                            &line,
                            &events,
                            &cancellation,
                            started,
                            &mut usage,
                            &mut finish_reason,
                            &mut emitted_content,
                        )
                        .await
                        {
                            Ok(LineResult::Done) => return,
                            Ok(LineResult::Continue) => {}
                            Err(error) => {
                                let _ = events.send(ChatEvent::Failed(error)).await;
                                return;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = events.send(ChatEvent::Failed(error)).await;
                        return;
                    }
                }
                let _ = events
                    .send(ChatEvent::Failed(protocol(
                        "llama.cpp closed the stream before [DONE]".to_owned(),
                    )))
                    .await;
                return;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn consume_sse_line(
    line: &[u8],
    events: &mpsc::Sender<ChatEvent>,
    cancellation: &CancellationToken,
    started: Instant,
    usage: &mut Usage,
    finish_reason: &mut Option<String>,
    emitted_content: &mut bool,
) -> Result<LineResult, BackendFailure> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() || line.starts_with(b":") || line.starts_with(b"event:") {
        return Ok(LineResult::Continue);
    }
    let Some(data) = line.strip_prefix(b"data:") else {
        return Ok(LineResult::Continue);
    };
    let data = data.strip_prefix(b" ").unwrap_or(data);
    if data == b"[DONE]" {
        if cancellation.is_cancelled() {
            let _ = events.send(ChatEvent::Cancelled).await;
        } else {
            let _ = events
                .send(ChatEvent::Finished {
                    reason: finish_reason.take(),
                    usage: usage.clone(),
                })
                .await;
        }
        return Ok(LineResult::Done);
    }
    let chunk: ChatChunk = serde_json::from_slice(data)
        .map_err(|error| protocol(format!("invalid llama.cpp SSE JSON: {error}")))?;
    if let Some(wire_usage) = chunk.usage {
        usage.prompt_eval_count = wire_usage.prompt_tokens;
        usage.eval_count = wire_usage.completion_tokens;
    }
    for choice in chunk.choices {
        if let Some(reason) = choice.finish_reason {
            *finish_reason = Some(reason);
        }
        if let Some(thinking) = choice
            .delta
            .reasoning_content
            .filter(|value| !value.is_empty())
            && events
                .send(ChatEvent::ThinkingDelta(thinking))
                .await
                .is_err()
        {
            return Ok(LineResult::Done);
        }
        if let Some(content) = choice.delta.content.filter(|value| !value.is_empty()) {
            if !*emitted_content {
                usage.time_to_first_token_ms =
                    Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
                *emitted_content = true;
            }
            if events.send(ChatEvent::ContentDelta(content)).await.is_err() {
                return Ok(LineResult::Done);
            }
        }
    }
    Ok(LineResult::Continue)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineResult {
    Continue,
    Done,
}

#[derive(Default)]
struct SseFramer {
    buffer: BytesMut,
}

impl SseFramer {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, BackendFailure> {
        let mut offset = 0;
        let mut lines = Vec::new();
        while let Some(relative) = chunk[offset..].iter().position(|byte| *byte == b'\n') {
            let end = offset + relative;
            let line_len = self.buffer.len().saturating_add(end.saturating_sub(offset));
            if line_len > MAX_SSE_LINE_BYTES {
                return Err(protocol("llama.cpp SSE line exceeded 16 MiB".to_owned()));
            }
            self.buffer.extend_from_slice(&chunk[offset..end]);
            lines.push(self.buffer.split().to_vec());
            offset = end + 1;
        }
        if offset < chunk.len() {
            self.buffer.extend_from_slice(&chunk[offset..]);
            if self.buffer.len() > MAX_SSE_LINE_BYTES {
                return Err(protocol("llama.cpp SSE line exceeded 16 MiB".to_owned()));
            }
        }
        Ok(lines)
    }

    fn finish(&mut self) -> Result<Option<Vec<u8>>, BackendFailure> {
        if self.buffer.is_empty() {
            return Ok(None);
        }
        if self.buffer.len() > MAX_SSE_LINE_BYTES {
            return Err(protocol("llama.cpp SSE line exceeded 16 MiB".to_owned()));
        }
        Ok(Some(self.buffer.split().to_vec()))
    }
}

#[derive(Debug, Deserialize)]
struct HealthResponse {
    status: String,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<WireModel>,
}

#[derive(Debug, Deserialize)]
struct WireModel {
    id: String,
}

#[derive(Debug, Serialize)]
struct ChatPayload<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    stream: bool,
    stream_options: StreamOptions,
}

impl<'a> From<&'a ChatRequest> for ChatPayload<'a> {
    fn from(request: &'a ChatRequest) -> Self {
        Self {
            model: &request.model,
            messages: request
                .messages
                .iter()
                .map(|message| WireMessage {
                    role: message.role.as_ollama_str(),
                    content: &message.content,
                })
                .collect(),
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<WireChoice>,
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    #[serde(default)]
    delta: WireDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

fn normalize_base_url(mut url: Url) -> Url {
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url
}

fn unavailable(message: String) -> BackendFailure {
    BackendFailure {
        kind: BackendErrorKind::Unavailable,
        message,
    }
}

fn protocol(message: String) -> BackendFailure {
    BackendFailure {
        kind: BackendErrorKind::Protocol,
        message,
    }
}

fn status_failure(status: StatusCode, body: &[u8]) -> BackendFailure {
    let kind = match status.as_u16() {
        400 | 422 => BackendErrorKind::InvalidRequest,
        404 => BackendErrorKind::MissingModel,
        429 | 503 => BackendErrorKind::Busy,
        _ if status.is_server_error() => BackendErrorKind::Server,
        _ => BackendErrorKind::Protocol,
    };
    let body = String::from_utf8_lossy(body);
    BackendFailure {
        kind,
        message: format!("llama.cpp returned HTTP {status}: {}", body.trim()),
    }
}

#[cfg(test)]
mod tests {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;
    use crate::domain::{ChatMessage, GenerationOptions};

    #[tokio::test]
    async fn lists_models_and_streams_openai_sse() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "qwen.gguf"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":1}}\n\n",
                    "data: [DONE]\n\n"
                ),
                "text/event-stream",
            ))
            .mount(&server)
            .await;

        let backend = LlamaCppBackend::new(Url::parse(&server.uri()).unwrap()).unwrap();
        assert_eq!(backend.probe().await.unwrap().version, "llama.cpp");
        assert_eq!(backend.list_models().await.unwrap()[0].name, "qwen.gguf");
        let mut events = backend
            .stream_chat(
                ChatRequest {
                    model: "qwen.gguf".to_owned(),
                    messages: vec![ChatMessage::user("hi")],
                    options: GenerationOptions::default(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            events.recv().await,
            Some(ChatEvent::ContentDelta("hello".to_owned()))
        );
        let Some(ChatEvent::Finished { usage, .. }) = events.recv().await else {
            panic!("expected finished event");
        };
        assert_eq!(usage.prompt_eval_count, Some(4));
        assert_eq!(usage.eval_count, Some(1));
    }

    #[test]
    fn sse_framer_rejects_a_newline_terminated_record_over_the_limit() {
        let mut record = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
        record.push(b'\n');
        let error = SseFramer::default()
            .push(&record)
            .expect_err("a 16 MiB-plus data record with a trailing newline must be rejected");
        assert_eq!(error.kind, BackendErrorKind::Protocol);
        assert!(error.message.contains("16 MiB"));
    }

    #[test]
    fn sse_framer_rejects_an_unterminated_final_record_over_the_limit() {
        let mut framer = SseFramer {
            buffer: BytesMut::from(vec![b'x'; MAX_SSE_LINE_BYTES + 1].as_slice()),
        };
        let error = framer
            .finish()
            .expect_err("the final unterminated record must honor the same size limit");
        assert_eq!(error.kind, BackendErrorKind::Protocol);
    }

    #[tokio::test]
    async fn probe_times_out_when_the_body_stalls() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "status": "ok" }))
                    .set_delay(Duration::from_secs(30)),
            )
            .mount(&server)
            .await;

        let mut backend = LlamaCppBackend::new(Url::parse(&server.uri()).unwrap()).unwrap();
        backend.request_timeout = Duration::from_millis(150);
        let started = Instant::now();
        let error = backend.probe().await.expect_err("stalled health must fail");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(error.kind, BackendErrorKind::Unavailable);
    }

    #[tokio::test]
    async fn error_bodies_are_capped_before_buffering() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_raw(vec![b'x'; MAX_ERROR_BODY_BYTES + 8], "text/plain"),
            )
            .mount(&server)
            .await;

        let backend = LlamaCppBackend::new(Url::parse(&server.uri()).unwrap()).unwrap();
        let error = backend
            .probe()
            .await
            .expect_err("oversized error must fail");
        assert_eq!(error.kind, BackendErrorKind::Protocol);
        assert!(error.message.contains("bytes"));
    }
}
