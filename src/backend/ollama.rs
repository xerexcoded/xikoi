use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{ChatBackend, ChatEventReceiver};
use crate::{
    domain::{
        BackendErrorKind, BackendFailure, BackendInfo, ChatEvent, ChatRequest, ModelDetails,
        ModelSummary, SelectedModel, Usage,
    },
    http_limits::{
        CONNECT_TIMEOUT, LimitedBodyError, MAX_ERROR_BODY_BYTES, MAX_JSON_BODY_BYTES,
        REQUEST_TIMEOUT, read_limited_body,
    },
};

const MAX_NDJSON_LINE_BYTES: usize = 16 * 1024 * 1024;
const CHAT_EVENT_CHANNEL_CAPACITY: usize = 256;

#[derive(Clone, Debug)]
pub struct OllamaBackend {
    base_url: Url,
    client: Client,
    request_timeout: Duration,
}

impl OllamaBackend {
    pub fn new(base_url: Url) -> Result<Self, BackendFailure> {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("xikoi/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| BackendFailure {
                kind: BackendErrorKind::Unavailable,
                message: format!("could not create the Ollama HTTP client: {error}"),
            })?;
        Ok(Self::with_client(base_url, client))
    }

    pub fn with_client(base_url: Url, client: Client) -> Self {
        Self {
            base_url: normalize_base_url(base_url),
            client,
            request_timeout: REQUEST_TIMEOUT,
        }
    }

    fn endpoint(&self, path: &str) -> Result<Url, BackendFailure> {
        self.base_url
            .join(path.trim_start_matches('/'))
            .map_err(|error| protocol_failure(format!("invalid Ollama endpoint URL: {error}")))
    }

    async fn get_json<T>(&self, path: &str) -> Result<T, BackendFailure>
    where
        T: DeserializeOwned,
    {
        let url = self.endpoint(path)?;
        let response = self
            .client
            .get(url)
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(|error| self.transport_failure(&error))?;
        self.decode_json(response).await
    }

    async fn post_json<B, T>(&self, path: &str, body: &B) -> Result<T, BackendFailure>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let url = self.endpoint(path)?;
        let response = self
            .client
            .post(url)
            .timeout(self.request_timeout)
            .json(body)
            .send()
            .await
            .map_err(|error| self.transport_failure(&error))?;
        self.decode_json(response).await
    }

    async fn decode_json<T>(&self, response: Response) -> Result<T, BackendFailure>
    where
        T: DeserializeOwned,
    {
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
            .map_err(|error| protocol_failure(format!("Ollama returned invalid JSON: {error}")))
    }

    fn limited_body_failure(&self, error: LimitedBodyError) -> BackendFailure {
        match error {
            LimitedBodyError::TooLarge { limit } => protocol_failure(format!(
                "Ollama returned a response larger than {limit} bytes"
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
                "Ollama is unavailable at {}: {error}",
                self.base_url.as_str().trim_end_matches('/')
            ),
        }
    }

    pub async fn pull_model<F>(&self, model: &str, mut on_progress: F) -> Result<(), BackendFailure>
    where
        F: FnMut(&PullProgress),
    {
        let response = self
            .client
            .post(self.endpoint("api/pull")?)
            .json(&PullRequest {
                model,
                stream: true,
            })
            .send()
            .await
            .map_err(|error| self.transport_failure(&error))?;
        let status = response.status();
        if !status.is_success() {
            let body = read_limited_body(response, MAX_ERROR_BODY_BYTES)
                .await
                .map_err(|error| self.limited_body_failure(error))?;
            return Err(status_failure(status, &body));
        }

        let mut stream = response.bytes_stream();
        let mut framer = NdjsonFramer::default();
        let mut completed = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| self.transport_failure(&error))?;
            for line in framer.push(&chunk)? {
                completed |= consume_pull_record(&line, &mut on_progress)?;
            }
        }
        if let Some(line) = framer.finish()? {
            completed |= consume_pull_record(&line, &mut on_progress)?;
        }
        if completed {
            Ok(())
        } else {
            Err(protocol_failure(
                "the Ollama pull response ended before status=success".to_owned(),
            ))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullProgress {
    pub status: String,
    pub completed: Option<u64>,
    pub total: Option<u64>,
}

impl PullProgress {
    #[allow(clippy::cast_precision_loss)]
    pub fn fraction(&self) -> Option<f64> {
        let completed = self.completed?;
        let total = self.total?;
        if total == 0 {
            return None;
        }
        Some((completed as f64 / total as f64).clamp(0.0, 1.0))
    }
}

#[derive(Serialize)]
struct PullRequest<'a> {
    model: &'a str,
    stream: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct PullWireChunk {
    status: String,
    completed: Option<u64>,
    total: Option<u64>,
    error: Option<String>,
}

fn consume_pull_record<F>(line: &[u8], on_progress: &mut F) -> Result<bool, BackendFailure>
where
    F: FnMut(&PullProgress),
{
    let chunk: PullWireChunk = serde_json::from_slice(line)
        .map_err(|error| protocol_failure(format!("invalid Ollama pull record: {error}")))?;
    if let Some(error) = chunk.error {
        return Err(wire_error_failure(None, error));
    }
    if chunk.status.trim().is_empty() {
        return Err(protocol_failure(
            "Ollama returned a pull record without status".to_owned(),
        ));
    }
    let progress = PullProgress {
        status: chunk.status,
        completed: chunk.completed,
        total: chunk.total,
    };
    let complete = progress.status.eq_ignore_ascii_case("success");
    on_progress(&progress);
    Ok(complete)
}

#[async_trait]
impl ChatBackend for OllamaBackend {
    async fn probe(&self) -> Result<BackendInfo, BackendFailure> {
        let response: VersionResponse = self.get_json("api/version").await?;
        if response.version.trim().is_empty() {
            return Err(protocol_failure(
                "Ollama returned an empty version string".to_owned(),
            ));
        }
        Ok(BackendInfo {
            version: response.version,
            endpoint: self.base_url.as_str().trim_end_matches('/').to_owned(),
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelSummary>, BackendFailure> {
        let response: ListModelsResponse = self.get_json("api/tags").await?;
        Ok(response
            .models
            .into_iter()
            .filter_map(WireModel::into_summary)
            .collect())
    }

    async fn running_models(&self) -> Result<Vec<SelectedModel>, BackendFailure> {
        let response: ListModelsResponse = self.get_json("api/ps").await?;
        Ok(response
            .models
            .into_iter()
            .filter_map(WireModel::into_selected)
            .collect())
    }

    async fn show_model(&self, model: &str) -> Result<SelectedModel, BackendFailure> {
        let response: ShowModelResponse = self
            .post_json(
                "api/show",
                &ShowModelRequest {
                    model,
                    verbose: false,
                },
            )
            .await?;

        let maximum_context = maximum_context(&response.model_info);
        Ok(SelectedModel {
            summary: ModelSummary {
                name: response
                    .name
                    .or(response.model)
                    .unwrap_or_else(|| model.to_owned()),
                size: response.size.unwrap_or_default(),
                digest: response.digest,
                details: response.details.into(),
                capabilities: response.capabilities,
                remote_model: response.remote_model,
                remote_host: response.remote_host,
            },
            maximum_context,
            ..SelectedModel::default()
        })
    }

    async fn stream_chat(
        &self,
        request: ChatRequest,
        cancellation: CancellationToken,
    ) -> Result<ChatEventReceiver, BackendFailure> {
        let (events, receiver) = mpsc::channel(CHAT_EVENT_CHANNEL_CAPACITY);
        let started_at = Instant::now();
        let url = self.endpoint("api/chat")?;
        let wire_request = ChatWireRequest::from(&request);
        let send = self.client.post(url).json(&wire_request).send();

        let response = tokio::select! {
            () = cancellation.cancelled() => {
                let _ = events.send(ChatEvent::Cancelled).await;
                return Ok(receiver);
            }
            result = send => {
                result.map_err(|error| self.transport_failure(&error))?
            }
        };

        let status = response.status();
        if !status.is_success() {
            let body = read_limited_body(response, MAX_ERROR_BODY_BYTES)
                .await
                .map_err(|error| self.limited_body_failure(error))?;
            return Err(status_failure(status, &body));
        }

        tokio::spawn(consume_chat_stream(
            response,
            events,
            cancellation,
            started_at,
        ));
        Ok(receiver)
    }
}

async fn consume_chat_stream(
    response: Response,
    events: mpsc::Sender<ChatEvent>,
    cancellation: CancellationToken,
    started_at: Instant,
) {
    let mut stream = response.bytes_stream();
    let mut framer = NdjsonFramer::default();
    let mut state = ChatStreamState::new(events, started_at);

    loop {
        let next = tokio::select! {
            () = cancellation.cancelled() => {
                state.emit_terminal(ChatEvent::Cancelled).await;
                return;
            }
            next = stream.next() => next,
        };

        match next {
            Some(Ok(bytes)) => match framer.push(&bytes) {
                Ok(lines) => {
                    for line in lines {
                        match state.consume_line(&line, &cancellation).await {
                            Ok(ConsumeLine::Done | ConsumeLine::ReceiverClosed) => return,
                            Ok(ConsumeLine::Continue) => {}
                            Ok(ConsumeLine::Cancelled) => {
                                state.emit_terminal(ChatEvent::Cancelled).await;
                                return;
                            }
                            Err(failure) => {
                                state.emit_terminal(ChatEvent::Failed(failure)).await;
                                return;
                            }
                        }
                    }
                }
                Err(failure) => {
                    state.emit_terminal(ChatEvent::Failed(failure)).await;
                    return;
                }
            },
            Some(Err(error)) => {
                state
                    .emit_terminal(ChatEvent::Failed(BackendFailure {
                        kind: BackendErrorKind::Unavailable,
                        message: format!("the Ollama response stream failed: {error}"),
                    }))
                    .await;
                return;
            }
            None => {
                match framer.finish() {
                    Ok(Some(line)) => match state.consume_line(&line, &cancellation).await {
                        Ok(ConsumeLine::Done | ConsumeLine::ReceiverClosed) => return,
                        Ok(ConsumeLine::Continue) => {}
                        Ok(ConsumeLine::Cancelled) => {
                            state.emit_terminal(ChatEvent::Cancelled).await;
                            return;
                        }
                        Err(failure) => {
                            state.emit_terminal(ChatEvent::Failed(failure)).await;
                            return;
                        }
                    },
                    Ok(None) => {}
                    Err(failure) => {
                        state.emit_terminal(ChatEvent::Failed(failure)).await;
                        return;
                    }
                }
                state
                    .emit_terminal(ChatEvent::Failed(protocol_failure(
                        "the Ollama response ended before done=true".to_owned(),
                    )))
                    .await;
                return;
            }
        }
    }
}

struct ChatStreamState {
    events: mpsc::Sender<ChatEvent>,
    started_at: Instant,
    time_to_first_token_ms: Option<u64>,
}

enum ConsumeLine {
    Continue,
    Done,
    Cancelled,
    ReceiverClosed,
}

impl ChatStreamState {
    const fn new(events: mpsc::Sender<ChatEvent>, started_at: Instant) -> Self {
        Self {
            events,
            started_at,
            time_to_first_token_ms: None,
        }
    }

    async fn consume_line(
        &mut self,
        line: &[u8],
        cancellation: &CancellationToken,
    ) -> Result<ConsumeLine, BackendFailure> {
        let chunk: ChatWireChunk = serde_json::from_slice(line)
            .map_err(|error| protocol_failure(format!("invalid Ollama stream record: {error}")))?;

        if let Some(message) = chunk.error {
            return Err(wire_error_failure(chunk.status, message));
        }
        if chunk.done.is_none() && chunk.message.is_none() {
            return Err(protocol_failure(
                "Ollama returned an unrecognized stream record".to_owned(),
            ));
        }

        if let Some(message) = chunk.message {
            if !message.thinking.is_empty() {
                let outcome = self
                    .emit(ChatEvent::ThinkingDelta(message.thinking), cancellation)
                    .await;
                if !matches!(outcome, ConsumeLine::Continue) {
                    return Ok(outcome);
                }
            }
            if !message.content.is_empty() {
                if self.time_to_first_token_ms.is_none() {
                    self.time_to_first_token_ms = Some(elapsed_millis(self.started_at));
                }
                let outcome = self
                    .emit(ChatEvent::ContentDelta(message.content), cancellation)
                    .await;
                if !matches!(outcome, ConsumeLine::Continue) {
                    return Ok(outcome);
                }
            }
        }

        if chunk.done == Some(true) {
            let outcome = self
                .emit(
                    ChatEvent::Finished {
                        reason: chunk.done_reason,
                        usage: Usage {
                            prompt_eval_count: chunk.prompt_eval_count,
                            prompt_eval_duration_ns: chunk.prompt_eval_duration,
                            eval_count: chunk.eval_count,
                            eval_duration_ns: chunk.eval_duration,
                            load_duration_ns: chunk.load_duration,
                            total_duration_ns: chunk.total_duration,
                            time_to_first_token_ms: self.time_to_first_token_ms,
                        },
                    },
                    cancellation,
                )
                .await;
            return Ok(match outcome {
                ConsumeLine::Continue => ConsumeLine::Done,
                other => other,
            });
        }
        Ok(ConsumeLine::Continue)
    }

    async fn emit(&self, event: ChatEvent, cancellation: &CancellationToken) -> ConsumeLine {
        tokio::select! {
            () = cancellation.cancelled() => ConsumeLine::Cancelled,
            result = self.events.send(event) => {
                if result.is_ok() {
                    ConsumeLine::Continue
                } else {
                    ConsumeLine::ReceiverClosed
                }
            }
        }
    }

    async fn emit_terminal(&self, event: ChatEvent) {
        let _ = self.events.send(event).await;
    }
}

#[derive(Default)]
struct NdjsonFramer {
    buffer: BytesMut,
}

impl NdjsonFramer {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Bytes>, BackendFailure> {
        let mut offset = 0;
        let mut lines = Vec::new();

        while let Some(relative) = bytes[offset..].iter().position(|byte| *byte == b'\n') {
            let end = offset + relative;
            let line_len = self.buffer.len().saturating_add(end.saturating_sub(offset));
            if line_len > MAX_NDJSON_LINE_BYTES {
                return Err(protocol_failure(format!(
                    "Ollama returned a stream record larger than {MAX_NDJSON_LINE_BYTES} bytes"
                )));
            }
            self.buffer.extend_from_slice(&bytes[offset..end]);
            if self.buffer.last() == Some(&b'\r') {
                self.buffer.truncate(self.buffer.len() - 1);
            }
            if !self.buffer.is_empty() {
                lines.push(self.buffer.split().freeze());
            }
            offset = end + 1;
        }

        if offset < bytes.len() {
            self.buffer.extend_from_slice(&bytes[offset..]);
            if self.buffer.len() > MAX_NDJSON_LINE_BYTES {
                return Err(protocol_failure(format!(
                    "Ollama returned a stream record larger than {MAX_NDJSON_LINE_BYTES} bytes"
                )));
            }
        }
        Ok(lines)
    }

    fn finish(&mut self) -> Result<Option<Bytes>, BackendFailure> {
        if self.buffer.is_empty() {
            return Ok(None);
        }
        if self.buffer.len() > MAX_NDJSON_LINE_BYTES {
            return Err(protocol_failure(format!(
                "Ollama returned a stream record larger than {MAX_NDJSON_LINE_BYTES} bytes"
            )));
        }
        if self.buffer.last() == Some(&b'\r') {
            self.buffer.truncate(self.buffer.len() - 1);
        }
        if self.buffer.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.buffer.split().freeze()))
    }
}

#[derive(Deserialize)]
struct VersionResponse {
    version: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ListModelsResponse {
    models: Vec<WireModel>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct WireModel {
    name: Option<String>,
    model: Option<String>,
    size: Option<u64>,
    digest: Option<String>,
    details: WireModelDetails,
    capabilities: Vec<String>,
    remote_model: Option<String>,
    remote_host: Option<String>,
    expires_at: Option<String>,
    size_vram: Option<u64>,
    context_length: Option<u64>,
}

impl WireModel {
    fn into_summary(self) -> Option<ModelSummary> {
        let name = self.name.or(self.model)?;
        Some(ModelSummary {
            name,
            size: self.size.unwrap_or_default(),
            digest: self.digest,
            details: self.details.into(),
            capabilities: self.capabilities,
            remote_model: self.remote_model,
            remote_host: self.remote_host,
        })
    }

    fn into_selected(self) -> Option<SelectedModel> {
        let expires_at = self.expires_at.as_deref().and_then(parse_ollama_timestamp);
        let allocated_context = self.context_length;
        let loaded_size = self.size_vram;
        self.into_summary().map(|summary| SelectedModel {
            summary,
            maximum_context: None,
            allocated_context,
            loaded_size,
            expires_at,
        })
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct WireModelDetails {
    format: Option<String>,
    family: Option<String>,
    parameter_size: Option<String>,
    quantization_level: Option<String>,
}

impl From<WireModelDetails> for ModelDetails {
    fn from(details: WireModelDetails) -> Self {
        Self {
            parameter_size: details.parameter_size,
            quantization_level: details.quantization_level,
            format: details.format,
            family: details.family,
        }
    }
}

#[derive(Serialize)]
struct ShowModelRequest<'a> {
    model: &'a str,
    verbose: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ShowModelResponse {
    name: Option<String>,
    model: Option<String>,
    size: Option<u64>,
    digest: Option<String>,
    details: WireModelDetails,
    capabilities: Vec<String>,
    model_info: Map<String, Value>,
    remote_model: Option<String>,
    remote_host: Option<String>,
}

#[derive(Serialize)]
struct ChatWireRequest<'a> {
    model: &'a str,
    messages: Vec<ChatWireMessage<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<&'a str>,
}

impl<'a> From<&'a ChatRequest> for ChatWireRequest<'a> {
    fn from(request: &'a ChatRequest) -> Self {
        Self {
            model: &request.model,
            messages: request
                .messages
                .iter()
                .map(|message| ChatWireMessage {
                    role: message.role.as_ollama_str(),
                    content: &message.content,
                })
                .collect(),
            stream: true,
            keep_alive: request.options.keep_alive.as_deref(),
        }
    }
}

#[derive(Serialize)]
struct ChatWireMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ChatWireChunk {
    message: Option<ChatWireResponseMessage>,
    done: Option<bool>,
    done_reason: Option<String>,
    total_duration: Option<u64>,
    load_duration: Option<u64>,
    prompt_eval_count: Option<u64>,
    prompt_eval_duration: Option<u64>,
    eval_count: Option<u64>,
    eval_duration: Option<u64>,
    error: Option<String>,
    status: Option<u16>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ChatWireResponseMessage {
    content: String,
    thinking: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
}

fn normalize_base_url(mut url: Url) -> Url {
    url.set_query(None);
    url.set_fragment(None);
    let mut path = url.path().trim_end_matches('/').to_owned();
    if path.ends_with("/api") {
        path.truncate(path.len() - "/api".len());
    }
    path.push('/');
    url.set_path(&path);
    url
}

fn maximum_context(model_info: &Map<String, Value>) -> Option<u64> {
    let architecture = model_info.get("general.architecture")?.as_str()?;
    let key = format!("{architecture}.context_length");
    value_as_u64(model_info.get(&key)?)
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn parse_ollama_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn elapsed_millis(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn status_failure(status: StatusCode, body: &[u8]) -> BackendFailure {
    let message = serde_json::from_slice::<ErrorResponse>(body)
        .map(|response| response.error)
        .ok()
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| format!("Ollama request failed with HTTP {status}"));
    failure_for_status(status.as_u16(), message)
}

fn wire_error_failure(status: Option<u16>, message: String) -> BackendFailure {
    match status {
        Some(status) => failure_for_status(status, message),
        None => BackendFailure {
            kind: BackendErrorKind::Server,
            message,
        },
    }
}

fn failure_for_status(status: u16, message: String) -> BackendFailure {
    let kind = match status {
        400 => BackendErrorKind::InvalidRequest,
        404 => BackendErrorKind::MissingModel,
        429 | 503 => BackendErrorKind::Busy,
        _ => BackendErrorKind::Server,
    };
    BackendFailure { kind, message }
}

fn protocol_failure(message: String) -> BackendFailure {
    BackendFailure {
        kind: BackendErrorKind::Protocol,
        message,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use serde_json::json;
    use tokio::time::{Duration, timeout};
    use uuid::Uuid;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };

    use super::*;
    use crate::domain::{ChatMessage, GenerationOptions, MessageOrigin, MessageStatus, Role};

    fn backend(server: &MockServer) -> OllamaBackend {
        OllamaBackend::new(Url::parse(&server.uri()).expect("mock server URI is valid"))
            .expect("backend can be created")
    }

    fn chat_request() -> ChatRequest {
        ChatRequest {
            model: "qwen3:4b".to_owned(),
            messages: vec![ChatMessage {
                id: Uuid::nil(),
                created_at: Utc::now(),
                role: Role::User,
                content: "hello".to_owned(),
                origin: MessageOrigin::Typed,
                status: MessageStatus::Complete,
                model: None,
                finish_reason: None,
                usage: None,
                error: None,
            }],
            options: GenerationOptions {
                keep_alive: Some("5m".to_owned()),
            },
        }
    }

    #[tokio::test]
    async fn probes_version_and_normalizes_api_suffix() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/version"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_bytes!("../../tests/fixtures/ollama/version.json"),
                "application/json",
            ))
            .mount(&server)
            .await;

        let base = Url::parse(&format!("{}/api", server.uri())).unwrap();
        let info = OllamaBackend::new(base).unwrap().probe().await.unwrap();

        assert_eq!(info.version, "0.12.6");
        assert_eq!(info.endpoint, server.uri());
    }

    #[tokio::test]
    async fn pulls_a_model_and_reports_streamed_progress() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .and(body_json(json!({
                "model": "qwen3:1.7b",
                "stream": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                concat!(
                    "{\"status\":\"pulling manifest\"}\n",
                    "{\"status\":\"downloading\",\"completed\":50,\"total\":100}\n",
                    "{\"status\":\"success\"}\n"
                ),
                "application/x-ndjson",
            ))
            .mount(&server)
            .await;

        let mut progress = Vec::new();
        backend(&server)
            .pull_model("qwen3:1.7b", |update| progress.push(update.clone()))
            .await
            .unwrap();

        assert_eq!(progress.len(), 3);
        assert_eq!(progress[1].fraction(), Some(0.5));
        assert_eq!(progress[2].status, "success");
    }

    #[tokio::test]
    async fn lists_local_and_remote_models_without_name_heuristics() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_bytes!("../../tests/fixtures/ollama/tags.json"),
                "application/json",
            ))
            .mount(&server)
            .await;

        let models = backend(&server).list_models().await.unwrap();

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "qwen3:4b");
        assert_eq!(models[0].details.parameter_size.as_deref(), Some("4.0B"));
        assert!(!models[0].is_cloud());
        assert_eq!(
            models[1].remote_host.as_deref(),
            Some("https://ollama.com:443")
        );
        assert_eq!(models[1].remote_model.as_deref(), Some("gemma4:31b-cloud"));
        assert!(models[1].is_cloud());
    }

    #[tokio::test]
    async fn parses_running_model_allocation_and_expiry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_bytes!("../../tests/fixtures/ollama/ps.json"),
                "application/json",
            ))
            .mount(&server)
            .await;

        let models = backend(&server).running_models().await.unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].allocated_context, Some(4096));
        assert_eq!(models[0].loaded_size, Some(2_500_000_000));
        assert!(models[0].expires_at.is_some());
    }

    #[tokio::test]
    async fn derives_maximum_context_from_reported_architecture() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .and(body_json(json!({
                "model": "qwen3:4b",
                "verbose": false
            })))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_bytes!("../../tests/fixtures/ollama/show.json"),
                "application/json",
            ))
            .mount(&server)
            .await;

        let selected = backend(&server).show_model("qwen3:4b").await.unwrap();

        assert_eq!(selected.maximum_context, Some(262_144));
        assert_eq!(
            selected.summary.details.quantization_level.as_deref(),
            Some("Q4_K_M")
        );
        assert_eq!(
            selected.summary.capabilities,
            ["completion", "tools", "thinking"]
        );
    }

    #[tokio::test]
    async fn streams_content_and_exact_final_usage_without_emitting_empty_final_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "qwen3:4b",
                "messages": [{"role": "user", "content": "hello"}],
                "stream": true,
                "keep_alive": "5m"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_bytes!("../../tests/fixtures/ollama/chat_success.ndjson"),
                "application/x-ndjson",
            ))
            .mount(&server)
            .await;

        let mut events = backend(&server)
            .stream_chat(chat_request(), CancellationToken::new())
            .await
            .unwrap();
        let mut received = Vec::new();
        while let Some(event) = events.recv().await {
            let done = matches!(event, ChatEvent::Finished { .. });
            received.push(event);
            if done {
                break;
            }
        }

        assert!(matches!(
            received.as_slice(),
            [
                ChatEvent::ThinkingDelta(thinking),
                ChatEvent::ContentDelta(first),
                ChatEvent::ContentDelta(second),
                ChatEvent::Finished {
                    reason: Some(reason),
                    usage,
                },
            ] if thinking == "checking" && first == "neon " && second == "脳"
                && reason == "stop"
                && usage.total_duration_ns == Some(200_000_000)
                && usage.load_duration_ns == Some(50_000_000)
                && usage.prompt_eval_count == Some(17)
                && usage.prompt_eval_duration_ns == Some(25_000_000)
                && usage.eval_count == Some(2)
                && usage.eval_duration_ns == Some(100_000_000)
                && usage.time_to_first_token_ms.is_some()
        ));
    }

    #[tokio::test]
    async fn surfaces_midstream_error_from_successful_http_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_bytes!("../../tests/fixtures/ollama/chat_midstream_error.ndjson"),
                "application/x-ndjson",
            ))
            .mount(&server)
            .await;

        let mut events = backend(&server)
            .stream_chat(chat_request(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            events.recv().await,
            Some(ChatEvent::ContentDelta("partial".to_owned()))
        );
        assert!(matches!(
            events.recv().await,
            Some(ChatEvent::Failed(BackendFailure {
                kind: BackendErrorKind::Server,
                message,
            })) if message == "runner stopped"
        ));
    }

    #[tokio::test]
    async fn reports_eof_before_done_as_a_protocol_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_bytes!("../../tests/fixtures/ollama/chat_truncated.ndjson"),
                "application/x-ndjson",
            ))
            .mount(&server)
            .await;

        let mut events = backend(&server)
            .stream_chat(chat_request(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            events.recv().await,
            Some(ChatEvent::ContentDelta("unfinished".to_owned()))
        );
        assert!(matches!(
            events.recv().await,
            Some(ChatEvent::Failed(BackendFailure {
                kind: BackendErrorKind::Protocol,
                message,
            })) if message.contains("before done=true")
        ));
    }

    #[tokio::test]
    async fn reports_malformed_ndjson_as_a_protocol_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"{not-json}\n", "application/x-ndjson"),
            )
            .mount(&server)
            .await;

        let mut events = backend(&server)
            .stream_chat(chat_request(), CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(ChatEvent::Failed(BackendFailure {
                kind: BackendErrorKind::Protocol,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn maps_documented_statuses() {
        for (status, expected) in [
            (400, BackendErrorKind::InvalidRequest),
            (404, BackendErrorKind::MissingModel),
            (429, BackendErrorKind::Busy),
            (503, BackendErrorKind::Busy),
            (500, BackendErrorKind::Server),
            (502, BackendErrorKind::Server),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/tags"))
                .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                    "error": "mapped error"
                })))
                .mount(&server)
                .await;

            let failure = backend(&server).list_models().await.unwrap_err();
            assert_eq!(failure.kind, expected);
            assert_eq!(failure.message, "mapped error");
        }
    }

    #[tokio::test]
    async fn cancellation_before_dispatch_returns_a_cancelled_event() {
        let server = MockServer::start().await;
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let mut events = backend(&server)
            .stream_chat(chat_request(), cancellation)
            .await
            .unwrap();

        assert_eq!(
            timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap(),
            Some(ChatEvent::Cancelled)
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_an_in_flight_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(5))
                    .set_body_raw(
                        include_bytes!("../../tests/fixtures/ollama/chat_success.ndjson"),
                        "application/x-ndjson",
                    ),
            )
            .mount(&server)
            .await;

        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task_backend = backend(&server);
        let request = tokio::spawn(async move {
            task_backend
                .stream_chat(chat_request(), task_cancellation)
                .await
        });

        timeout(Duration::from_secs(1), async {
            loop {
                if server
                    .received_requests()
                    .await
                    .is_some_and(|requests| !requests.is_empty())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the request should reach Ollama before cancellation");

        cancellation.cancel();
        let mut events = timeout(Duration::from_secs(1), request)
            .await
            .expect("cancellation should interrupt the pending response")
            .expect("the request task should not panic")
            .expect("cancellation returns an event receiver");

        assert_eq!(
            timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap(),
            Some(ChatEvent::Cancelled)
        );
    }

    #[tokio::test]
    async fn bounded_emission_remains_cancellable_when_the_ui_stalls() {
        let (events, mut receiver) = mpsc::channel(1);
        events
            .send(ChatEvent::ContentDelta("already queued".to_owned()))
            .await
            .unwrap();
        let mut state = ChatStreamState::new(events, Instant::now());
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let outcome = state
            .consume_line(
                br#"{"message":{"content":"blocked"},"done":false}"#,
                &cancellation,
            )
            .await
            .unwrap();

        assert!(matches!(outcome, ConsumeLine::Cancelled));
        assert_eq!(
            receiver.recv().await,
            Some(ChatEvent::ContentDelta("already queued".to_owned()))
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn framing_survives_every_byte_boundary_including_utf8() {
        let input = include_bytes!("../../tests/fixtures/ollama/chat_success.ndjson");
        let mut framer = NdjsonFramer::default();
        let mut lines = Vec::new();

        for byte in input {
            lines.extend(framer.push(&[*byte]).unwrap());
        }
        if let Some(line) = framer.finish().unwrap() {
            lines.push(line);
        }

        assert_eq!(lines.len(), 4);
        let chunks: Vec<ChatWireChunk> = lines
            .iter()
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(
            chunks[2]
                .message
                .as_ref()
                .map(|message| message.content.as_str()),
            Some("脳")
        );
    }

    #[test]
    fn framing_accepts_coalesced_crlf_and_unterminated_final_line() {
        let mut framer = NdjsonFramer::default();
        let lines = framer
            .push(b"{\"done\":false}\r\n{\"done\":false}\n{\"done\":true}")
            .unwrap();

        assert_eq!(lines.len(), 2);
        assert_eq!(framer.finish().unwrap().unwrap(), &b"{\"done\":true}"[..]);
    }

    #[test]
    fn framing_rejects_a_newline_terminated_record_over_the_limit() {
        let mut record = vec![b'x'; MAX_NDJSON_LINE_BYTES + 1];
        record.push(b'\n');
        let error = NdjsonFramer::default()
            .push(&record)
            .expect_err("a 16 MiB-plus NDJSON record with a trailing newline must be rejected");
        assert_eq!(error.kind, BackendErrorKind::Protocol);
    }

    #[test]
    fn dynamic_context_does_not_guess_from_an_unrelated_architecture() {
        let info = Map::from_iter([
            ("general.architecture".to_owned(), json!("qwen3")),
            ("vision.context_length".to_owned(), json!(8192)),
        ]);
        assert_eq!(maximum_context(&info), None);
    }

    #[test]
    fn numeric_string_context_is_supported_without_accepting_negative_values() {
        let info = Map::from_iter([
            ("general.architecture".to_owned(), json!("qwen3")),
            ("qwen3.context_length".to_owned(), json!("262144")),
        ]);
        assert_eq!(maximum_context(&info), Some(262_144));
        assert_eq!(value_as_u64(&json!(-1)), None);
    }

    #[test]
    fn backend_is_dyn_safe() {
        fn accept_backend(_: Arc<dyn ChatBackend>) {}

        let client = Client::new();
        let backend =
            OllamaBackend::with_client(Url::parse("http://127.0.0.1:11434").unwrap(), client);
        accept_backend(Arc::new(backend));
    }

    #[tokio::test]
    async fn probe_times_out_when_the_body_stalls() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/version"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(
                        include_bytes!("../../tests/fixtures/ollama/version.json"),
                        "application/json",
                    )
                    .set_delay(Duration::from_secs(30)),
            )
            .mount(&server)
            .await;

        let mut backend = backend(&server);
        backend.request_timeout = Duration::from_millis(150);
        let started = Instant::now();
        let error = backend
            .probe()
            .await
            .expect_err("stalled version must fail");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(error.kind, BackendErrorKind::Unavailable);
    }

    #[tokio::test]
    async fn error_bodies_are_capped_before_buffering() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/version"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_raw(vec![b'x'; MAX_ERROR_BODY_BYTES + 8], "text/plain"),
            )
            .mount(&server)
            .await;

        let error = backend(&server)
            .probe()
            .await
            .expect_err("oversized error must fail");
        assert_eq!(error.kind, BackendErrorKind::Protocol);
        assert!(error.message.contains("bytes"));
    }
}
