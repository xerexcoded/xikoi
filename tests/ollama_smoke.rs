use std::{env, error::Error};

use tokio_util::sync::CancellationToken;
use url::Url;
use xikoi::{
    backend::{ChatBackend, ollama::OllamaBackend},
    domain::{ChatEvent, ChatMessage, ChatRequest, GenerationOptions},
};

/// Exercises one already-installed model against a real local Ollama server.
///
/// The model name is supplied explicitly so this test can never pull or choose
/// model weights on the user's behalf.
#[tokio::test]
#[ignore = "requires a running Ollama server and OLLAMA_INTEGRATION_MODEL"]
async fn streams_from_an_installed_local_model() -> Result<(), Box<dyn Error>> {
    let model = env::var("OLLAMA_INTEGRATION_MODEL")?;
    let base_url = integration_url()?;
    let backend = OllamaBackend::new(base_url)?;

    let info = backend.probe().await?;
    assert!(!info.version.is_empty(), "Ollama did not report a version");

    let models = backend.list_models().await?;
    let installed = models
        .iter()
        .find(|candidate| candidate.name == model)
        .unwrap_or_else(|| panic!("OLLAMA_INTEGRATION_MODEL `{model}` is not installed"));
    assert!(
        !installed.is_cloud(),
        "the integration test only accepts a local model"
    );

    let request = ChatRequest {
        model,
        messages: vec![ChatMessage::user(
            "Reply with the single word XIKOI_OK and nothing else.",
        )],
        options: GenerationOptions::default(),
    };
    let mut events = backend
        .stream_chat(request, CancellationToken::new())
        .await?;
    let mut content = String::new();
    let mut finished = false;

    while let Some(event) = events.recv().await {
        match event {
            ChatEvent::ContentDelta(delta) => content.push_str(&delta),
            ChatEvent::ThinkingDelta(_) => {}
            ChatEvent::Finished { usage, .. } => {
                assert!(
                    usage.eval_count.is_some_and(|count| count > 0),
                    "Ollama did not return an exact output-token count"
                );
                finished = true;
            }
            ChatEvent::Failed(error) => panic!("Ollama stream failed: {error}"),
            ChatEvent::Cancelled => panic!("Ollama unexpectedly cancelled the stream"),
        }
    }

    assert!(finished, "Ollama closed the stream before its final record");
    assert!(
        !content.trim().is_empty(),
        "Ollama completed without assistant content"
    );
    Ok(())
}

fn integration_url() -> Result<Url, url::ParseError> {
    let value = env::var("XIKOI_OLLAMA_URL")
        .or_else(|_| env::var("OLLAMA_HOST"))
        .unwrap_or_else(|_| "http://127.0.0.1:11434".to_owned());
    if value.contains("://") {
        Url::parse(&value)
    } else {
        Url::parse(&format!("http://{value}"))
    }
}
