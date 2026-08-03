//! Guided public-install setup and deterministic dependency diagnostics.

use std::{
    fs::OpenOptions,
    io::{self, IsTerminal, Write},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use color_eyre::eyre::{Context, Result, bail, eyre};
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use crate::{
    audio::{AudioInventory, QWEN_TTS_SAMPLE_RATE, RecordedAudio},
    backend::{ChatBackend, llama_cpp::LlamaCppBackend, ollama::OllamaBackend},
    config::{BackendKind, RuntimeConfig, SetupArgs},
    speech::{MlxAudioClient, SynthesisRequest, TranscriptionRequest},
    storage::Storage,
    voice_runtime::{VoiceRuntime, find_program},
};

const LARGE_MEMORY_THRESHOLD_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const LARGE_DEFAULT_MODEL: &str = "qwen3.5:9b";
const SMALL_DEFAULT_MODEL: &str = "qwen3:1.7b";
const OLLAMA_START_TIMEOUT: Duration = Duration::from_mins(1);

pub async fn run(config: &RuntimeConfig, arguments: &SetupArgs) -> Result<()> {
    if !arguments.skip_voice && !is_apple_silicon_macos() {
        bail!(
            "the complete Xikoi voice runtime currently requires Apple Silicon macOS; use --skip-voice for chat-only setup"
        );
    }

    let storage = Storage::new(config.paths.clone());
    storage.initialize().await?;
    let memory = system_memory_bytes();
    let chat_model = config
        .requested_model
        .clone()
        .unwrap_or_else(|| recommended_chat_model(memory).to_owned());

    print_plan(config, arguments, &chat_model, memory);
    if !arguments.yes && !confirm("Proceed with this setup? [y/N] ")? {
        println!("Setup cancelled; no dependencies or models were changed.");
        return Ok(());
    }

    if !arguments.skip_ollama {
        println!("\n[1/2] Preparing Ollama and {chat_model}");
        let backend = ensure_ollama(config).await?;
        let installed = backend.list_models().await?;
        if installed.iter().any(|model| model.name == chat_model) {
            println!("Ready: Ollama model {chat_model} is already installed.");
        } else {
            println!("Downloading Ollama model {chat_model}...");
            let mut last_status = String::new();
            let mut last_percent = None;
            backend
                .pull_model(&chat_model, |progress| {
                    if let Some(fraction) = progress.fraction() {
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let percent = (fraction * 100.0).round() as u8;
                        if last_percent != Some(percent) {
                            print!("\r  {:3}%  {}", percent, progress.status);
                            let _ = io::stdout().flush();
                            last_percent = Some(percent);
                        }
                    } else if progress.status != last_status {
                        if last_percent.is_some() {
                            println!();
                            last_percent = None;
                        }
                        println!("  {}", progress.status);
                        last_status.clone_from(&progress.status);
                    }
                })
                .await?;
            if last_percent.is_some() {
                println!();
            }
            println!("Ready: Ollama model {chat_model}.");
        }

        let mut state = storage.load_state().await?;
        state.last_model = Some(chat_model.clone());
        storage.save_state(&state).await?;
    }

    if !arguments.skip_voice {
        println!("\n[2/2] Preparing the pinned MLX-Audio voice runtime");
        ensure_uv().await?;
        let runtime = VoiceRuntime::from_paths(&config.paths);
        runtime.materialize().await?;
        runtime.sync().await?;
        runtime
            .download_models(&config.speech.asr_model, &config.speech.tts_model)
            .await?;
        if !runtime
            .models_cached(&config.speech.asr_model, &config.speech.tts_model)
            .await?
        {
            bail!("the voice model cache did not pass its offline verification");
        }

        println!("Warming and verifying real TTS and ASR inference...");
        let _managed = runtime
            .spawn_managed(&config.speech, &config.paths.logs)
            .await?;
        warm_voice_models(config).await?;
        println!("Ready: Qwen speech-to-text and text-to-speech both completed inference.");
    }

    println!("\nXikoi setup is complete. Run `xikoi` to open the interface.");
    Ok(())
}

pub async fn doctor(config: &RuntimeConfig) -> Result<()> {
    let storage = Storage::new(config.paths.clone());
    storage.initialize().await?;
    let state = storage.load_state().await?;
    let mut healthy = true;

    println!("Xikoi doctor\n");
    healthy &= check_chat(config, state.last_model.as_deref()).await;
    if config.speech.enabled {
        healthy &= report(
            "platform",
            is_apple_silicon_macos(),
            if is_apple_silicon_macos() {
                "Apple Silicon macOS"
            } else {
                "voice requires Apple Silicon macOS"
            },
        );
        healthy &= check_voice(config).await;
        healthy &= check_audio();
    } else {
        report("voice layer", true, "disabled by configuration");
    }

    if healthy {
        println!("\nAll required Xikoi components are ready.");
        Ok(())
    } else {
        bail!("one or more Xikoi checks failed; run `xikoi setup` and retry")
    }
}

async fn check_chat(config: &RuntimeConfig, last_model: Option<&str>) -> bool {
    let mut healthy = true;
    match config.backend {
        BackendKind::Ollama => {
            let Ok(backend) = OllamaBackend::new(config.ollama_url.clone()) else {
                return report("Ollama server", false, "could not create the HTTP client");
            };
            match backend.probe().await {
                Ok(info) => {
                    report(
                        "Ollama server",
                        true,
                        &format!("{} at {}", info.version, info.endpoint),
                    );
                    let expected = config
                        .requested_model
                        .as_deref()
                        .or(last_model)
                        .unwrap_or_else(|| recommended_chat_model(system_memory_bytes()));
                    match backend.list_models().await {
                        Ok(models) => {
                            healthy &= report(
                                "chat model",
                                models.iter().any(|model| model.name == expected),
                                expected,
                            );
                        }
                        Err(error) => {
                            healthy &= report("chat model", false, &error.to_string());
                        }
                    }
                }
                Err(error) => healthy &= report("Ollama server", false, &error.to_string()),
            }
        }
        BackendKind::LlamaCpp => {
            let Ok(backend) = LlamaCppBackend::new(config.llama_url.clone()) else {
                return report(
                    "llama.cpp server",
                    false,
                    "could not create the HTTP client",
                );
            };
            match backend.probe().await {
                Ok(info) => {
                    report(
                        "llama.cpp server",
                        true,
                        &format!("{} at {}", info.version, info.endpoint),
                    );
                }
                Err(error) => healthy &= report("llama.cpp server", false, &error.to_string()),
            }
        }
    }
    healthy
}

async fn check_voice(config: &RuntimeConfig) -> bool {
    let mut healthy = true;
    let runtime = VoiceRuntime::from_paths(&config.paths);
    healthy &= report(
        "uv",
        find_program("uv").is_some(),
        find_program("uv")
            .as_deref()
            .and_then(Path::to_str)
            .unwrap_or("not found"),
    );
    healthy &= report(
        "voice runtime",
        runtime.is_installed(),
        runtime.root().to_string_lossy().as_ref(),
    );
    let cached = runtime
        .models_cached(&config.speech.asr_model, &config.speech.tts_model)
        .await
        .unwrap_or(false);
    healthy &= report(
        "voice models",
        cached,
        if cached {
            "ASR and TTS snapshots cached"
        } else {
            "run `xikoi setup`"
        },
    );

    if runtime.is_installed() {
        match runtime
            .spawn_managed(&config.speech, &config.paths.logs)
            .await
        {
            Ok(_guard) => {
                report("speech server", true, config.speech.url.as_str());
            }
            Err(error) => healthy &= report("speech server", false, &error.to_string()),
        }
    }
    healthy
}

fn check_audio() -> bool {
    match AudioInventory::discover() {
        Ok(inventory) => report(
            "audio devices",
            !inventory.inputs.is_empty() && !inventory.outputs.is_empty(),
            &format!(
                "{} input(s), {} output(s)",
                inventory.inputs.len(),
                inventory.outputs.len()
            ),
        ),
        Err(error) => report("audio devices", false, &error.to_string()),
    }
}

pub fn voice_server(config: &RuntimeConfig) -> Result<()> {
    VoiceRuntime::from_paths(&config.paths)
        .run_foreground(&config.speech)
        .wrap_err("could not run the managed voice server")
}

async fn ensure_ollama(config: &RuntimeConfig) -> Result<OllamaBackend> {
    let backend = OllamaBackend::new(config.ollama_url.clone())?;
    if backend.probe().await.is_ok() {
        return Ok(backend);
    }
    if !is_loopback_url(&config.ollama_url) {
        bail!(
            "Ollama is unavailable at {}; Xikoi will not install or start software for a remote endpoint",
            config.ollama_url
        );
    }

    if find_program("ollama").is_none() && !ollama_app_exists() {
        let brew = find_program("brew").ok_or_else(|| {
            eyre!("Ollama is missing and Homebrew is unavailable; install Ollama, then retry setup")
        })?;
        run_command(&brew, &["install", "ollama"], "brew install ollama").await?;
    }
    start_ollama(&config.paths.logs).await?;

    let deadline = Instant::now() + OLLAMA_START_TIMEOUT;
    let mut last_error = "Ollama did not answer its health check".to_owned();
    while Instant::now() < deadline {
        match backend.probe().await {
            Ok(_) => return Ok(backend),
            Err(error) => last_error = error.to_string(),
        }
        sleep(Duration::from_millis(300)).await;
    }
    bail!("Ollama did not become ready: {last_error}")
}

async fn start_ollama(logs: &Path) -> Result<()> {
    if ollama_app_exists() {
        run_command(
            Path::new("/usr/bin/open"),
            &["-gj", "-a", "Ollama"],
            "open Ollama",
        )
        .await?;
        return Ok(());
    }

    if let Some(brew) = find_program("brew") {
        let installed = tokio::process::Command::new(&brew)
            .args(["list", "--formula", "ollama"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?
            .success();
        if installed {
            run_command(
                &brew,
                &["services", "start", "ollama"],
                "brew services start ollama",
            )
            .await?;
            return Ok(());
        }
    }

    let ollama = find_program("ollama").ok_or_else(|| eyre!("Ollama is not installed"))?;
    std::fs::create_dir_all(logs)?;
    let log = open_private_append(&logs.join("ollama-setup.log"))?;
    let error_log = log.try_clone()?;
    Command::new(ollama)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log))
        .spawn()
        .wrap_err("could not start `ollama serve`")?;
    Ok(())
}

fn ollama_app_exists() -> bool {
    Path::new("/Applications/Ollama.app").exists()
        || home::home_dir().is_some_and(|home| home.join("Applications/Ollama.app").exists())
}

async fn ensure_uv() -> Result<()> {
    if find_program("uv").is_some() {
        return Ok(());
    }
    let brew = find_program("brew")
        .ok_or_else(|| eyre!("uv is missing and Homebrew is unavailable; install uv and retry"))?;
    run_command(&brew, &["install", "uv"], "brew install uv").await
}

async fn run_command(program: &Path, arguments: &[&str], description: &str) -> Result<()> {
    let status = tokio::process::Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .status()
        .await
        .wrap_err_with(|| format!("could not run {description}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{description} failed with status {:?}", status.code())
    }
}

async fn warm_voice_models(config: &RuntimeConfig) -> Result<()> {
    let client = MlxAudioClient::new(config.speech.url.clone())?;
    let cancellation = CancellationToken::new();
    let response = client
        .synthesize(
            &SynthesisRequest::qwen(
                &config.speech.tts_model,
                "Xikoi voice setup complete.",
                &config.speech.tts_voice,
            ),
            &cancellation,
        )
        .await?;
    let pcm = response.bytes().await?;
    if pcm.len() < 2 {
        bail!("MLX-Audio returned no PCM during TTS warm-up");
    }
    let samples = pcm
        .chunks_exact(2)
        .map(|chunk| f32::from(i16::from_le_bytes([chunk[0], chunk[1]])) / 32_768.0)
        .collect();
    let wav = RecordedAudio {
        samples,
        sample_rate: QWEN_TTS_SAMPLE_RATE,
    }
    .to_asr_wav()?;
    let transcript = client
        .transcribe(
            TranscriptionRequest {
                model: config.speech.asr_model.clone(),
                wav,
                language: Some("en".to_owned()),
            },
            &cancellation,
        )
        .await?;
    println!("Warm-up transcript: {transcript}");
    Ok(())
}

fn print_plan(
    config: &RuntimeConfig,
    arguments: &SetupArgs,
    chat_model: &str,
    memory: Option<u64>,
) {
    println!("Xikoi guided setup\n");
    if let Some(memory) = memory {
        let gib_tenths = memory.saturating_mul(10) / 1024_u64.pow(3);
        println!(
            "Detected memory: {}.{} GiB",
            gib_tenths / 10,
            gib_tenths % 10
        );
    }
    if !arguments.skip_ollama {
        println!(
            "- Ollama: install/start if required and ensure {chat_model} ({})",
            chat_model_size(chat_model)
        );
    }
    if !arguments.skip_voice {
        println!("- MLX-Audio: pinned private Python 3.12 environment");
        println!("- ASR: {} (about 1.0 GB)", config.speech.asr_model);
        println!("- TTS: {} (about 1.7 GB)", config.speech.tts_model);
        println!("- Verification: offline cache check plus real TTS → ASR warm-up");
    }
    println!("Downloads come from Homebrew, Ollama, PyPI, and Hugging Face as needed.");
}

fn confirm(prompt: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("setup needs confirmation; rerun interactively or pass --yes")
    }
    print!("\n{prompt}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn report(label: &str, success: bool, detail: &str) -> bool {
    println!(
        "{} {label}: {detail}",
        if success { "[ok]" } else { "[!!]" }
    );
    success
}

fn recommended_chat_model(memory: Option<u64>) -> &'static str {
    if memory.is_some_and(|bytes| bytes >= LARGE_MEMORY_THRESHOLD_BYTES) {
        LARGE_DEFAULT_MODEL
    } else {
        SMALL_DEFAULT_MODEL
    }
}

fn chat_model_size(model: &str) -> &'static str {
    match model {
        LARGE_DEFAULT_MODEL => "about 6.6 GB",
        SMALL_DEFAULT_MODEL => "about 1.4 GB",
        _ => "size reported by Ollama during download",
    }
}

fn is_apple_silicon_macos() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

fn is_loopback_url(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn system_memory_bytes() -> Option<u64> {
    if cfg!(target_os = "macos") {
        let output = Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        return String::from_utf8(output.stdout).ok()?.trim().parse().ok();
    }
    if cfg!(target_os = "linux") {
        let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kib = contents
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?;
        return kib.checked_mul(1024);
    }
    None
}

fn open_private_append(path: &Path) -> Result<std::fs::File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_the_chat_model_from_memory_tier() {
        assert_eq!(
            recommended_chat_model(Some(24 * 1024_u64.pow(3))),
            LARGE_DEFAULT_MODEL
        );
        assert_eq!(
            recommended_chat_model(Some(16 * 1024_u64.pow(3))),
            SMALL_DEFAULT_MODEL
        );
        assert_eq!(recommended_chat_model(None), SMALL_DEFAULT_MODEL);
    }

    #[test]
    fn only_local_ollama_endpoints_can_be_started() {
        assert!(is_loopback_url(&"http://127.0.0.1:11434".parse().unwrap()));
        assert!(is_loopback_url(&"http://[::1]:11434".parse().unwrap()));
        assert!(!is_loopback_url(
            &"https://ollama.example.com".parse().unwrap()
        ));
    }
}
