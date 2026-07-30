use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A role accepted by Ollama's chat API and persisted in a session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub const fn as_ollama_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// Where a user message originated. Speech is reserved for the voice phase.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    #[default]
    Typed,
    Speech,
    Generated,
}

/// Whether the message reached a normal terminal state.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    #[default]
    Complete,
    Streaming,
    Cancelled,
    Failed,
}

/// Exact metrics returned by Ollama's final streaming record.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Usage {
    pub prompt_eval_count: Option<u64>,
    pub prompt_eval_duration_ns: Option<u64>,
    pub eval_count: Option<u64>,
    pub eval_duration_ns: Option<u64>,
    pub load_duration_ns: Option<u64>,
    pub total_duration_ns: Option<u64>,
    /// Measured by Xikoi from dispatch until the first content delta.
    pub time_to_first_token_ms: Option<u64>,
}

impl Usage {
    #[allow(clippy::cast_precision_loss)]
    pub fn output_tokens_per_second(&self) -> Option<f64> {
        let count = self.eval_count?;
        let duration = self.eval_duration_ns?;
        if duration == 0 {
            return None;
        }
        Some((count as f64) * 1_000_000_000.0 / duration as f64)
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn context_ratio(&self, allocated_context: Option<u64>) -> Option<f64> {
        let prompt = self.prompt_eval_count?;
        let context = allocated_context?;
        if context == 0 {
            return None;
        }
        Some((prompt as f64 / context as f64).clamp(0.0, 1.0))
    }

    pub fn total_duration(&self) -> Option<Duration> {
        self.total_duration_ns.map(Duration::from_nanos)
    }

    pub fn load_duration(&self) -> Option<Duration> {
        self.load_duration_ns.map(Duration::from_nanos)
    }
}

/// A chat message used by the app and written to JSONL.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChatMessage {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub role: Role,
    pub content: String,
    pub origin: MessageOrigin,
    pub status: MessageStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            id: Uuid::now_v7(),
            created_at: Utc::now(),
            role: Role::User,
            content: content.into(),
            origin: MessageOrigin::Typed,
            status: MessageStatus::Complete,
            model: None,
            finish_reason: None,
            usage: None,
            error: None,
        }
    }

    pub fn streaming_assistant(model: impl Into<String>) -> Self {
        Self {
            id: Uuid::now_v7(),
            created_at: Utc::now(),
            role: Role::Assistant,
            content: String::new(),
            origin: MessageOrigin::Generated,
            status: MessageStatus::Streaming,
            model: Some(model.into()),
            finish_reason: None,
            usage: None,
            error: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelDetails {
    pub parameter_size: Option<String>,
    pub quantization_level: Option<String>,
    pub format: Option<String>,
    pub family: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ModelSummary {
    pub name: String,
    pub size: u64,
    pub digest: Option<String>,
    pub details: ModelDetails,
    pub capabilities: Vec<String>,
    pub remote_model: Option<String>,
    pub remote_host: Option<String>,
}

impl ModelSummary {
    pub fn is_cloud(&self) -> bool {
        self.remote_model
            .as_ref()
            .is_some_and(|value| !value.is_empty())
            || self
                .remote_host
                .as_ref()
                .is_some_and(|value| !value.is_empty())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SelectedModel {
    pub summary: ModelSummary,
    pub maximum_context: Option<u64>,
    pub allocated_context: Option<u64>,
    pub loaded_size: Option<u64>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackendInfo {
    pub version: String,
    pub endpoint: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct GenerationOptions {
    pub keep_alive: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub options: GenerationOptions,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ChatEvent {
    ContentDelta(String),
    ThinkingDelta(String),
    Finished {
        reason: Option<String>,
        usage: Usage,
    },
    Failed(BackendFailure),
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendErrorKind {
    Unavailable,
    InvalidRequest,
    MissingModel,
    Busy,
    Protocol,
    Server,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendFailure {
    pub kind: BackendErrorKind,
    pub message: String,
}

impl std::fmt::Display for BackendFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BackendFailure {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_per_second_is_exact_and_zero_safe() {
        let usage = Usage {
            eval_count: Some(20),
            eval_duration_ns: Some(500_000_000),
            ..Usage::default()
        };
        assert_eq!(usage.output_tokens_per_second(), Some(40.0));

        let zero = Usage {
            eval_count: Some(20),
            eval_duration_ns: Some(0),
            ..Usage::default()
        };
        assert_eq!(zero.output_tokens_per_second(), None);
    }

    #[test]
    fn detects_remote_models_without_name_heuristics() {
        let local = ModelSummary {
            name: "qwen3.5:9b".into(),
            ..ModelSummary::default()
        };
        assert!(!local.is_cloud());

        let cloud = ModelSummary {
            name: "friendly-name".into(),
            remote_host: Some("https://ollama.com:443".into()),
            ..ModelSummary::default()
        };
        assert!(cloud.is_cloud());
    }
}
