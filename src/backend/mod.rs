use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::{
    BackendFailure, BackendInfo, ChatEvent, ChatRequest, ModelSummary, SelectedModel,
};

pub mod llama_cpp;
pub mod ollama;

pub type ChatEventReceiver = mpsc::Receiver<ChatEvent>;

/// An asynchronous chat backend that can be used behind a trait object.
#[async_trait]
pub trait ChatBackend: Send + Sync {
    async fn probe(&self) -> Result<BackendInfo, BackendFailure>;

    async fn list_models(&self) -> Result<Vec<ModelSummary>, BackendFailure>;

    async fn running_models(&self) -> Result<Vec<SelectedModel>, BackendFailure>;

    async fn show_model(&self, model: &str) -> Result<SelectedModel, BackendFailure>;

    /// Starts a chat response after the configured local server accepts the
    /// request. Cancellation remains active during model loading and streaming.
    async fn stream_chat(
        &self,
        request: ChatRequest,
        cancellation: CancellationToken,
    ) -> Result<ChatEventReceiver, BackendFailure>;
}
