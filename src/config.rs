use std::{
    env,
    path::{Path, PathBuf},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
const DEFAULT_LLAMA_URL: &str = "http://127.0.0.1:8080";
const DEFAULT_SPEECH_URL: &str = "http://127.0.0.1:8000";
pub const DEFAULT_ASR_MODEL: &str = "mlx-community/Qwen3-ASR-0.6B-8bit";
pub const DEFAULT_TTS_MODEL: &str = "mlx-community/Qwen3-TTS-12Hz-0.6B-CustomVoice-6bit";
const DEFAULT_TTS_VOICE: &str = "Vivian";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ColorMode {
    #[default]
    Auto,
    Truecolor,
    Ansi,
    Mono,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    #[default]
    Ollama,
    LlamaCpp,
}

#[derive(Debug, Parser)]
#[command(
    name = "xikoi",
    version,
    about = "A local-first neural voice interface for local AI"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<CliCommand>,

    /// Ollama base URL.
    #[arg(long, global = true)]
    pub ollama_url: Option<Url>,

    /// Chat server implementation.
    #[arg(long, value_enum, global = true)]
    pub backend: Option<BackendKind>,

    /// llama.cpp OpenAI-compatible server base URL.
    #[arg(long, global = true)]
    pub llama_url: Option<Url>,

    /// Select an installed local model.
    #[arg(long, global = true)]
    pub model: Option<String>,

    /// Resume a saved session by UUID.
    #[arg(long)]
    pub resume: Option<String>,

    /// Force a terminal color profile.
    #[arg(long, value_enum)]
    pub color: Option<ColorMode>,

    /// Disable animated activity signals.
    #[arg(long)]
    pub no_animation: bool,

    /// MLX-Audio base URL.
    #[arg(long, global = true)]
    pub speech_url: Option<Url>,

    /// Qwen speech-to-text model served by MLX-Audio.
    #[arg(long, global = true)]
    pub asr_model: Option<String>,

    /// Qwen text-to-speech model served by MLX-Audio.
    #[arg(long, global = true)]
    pub tts_model: Option<String>,

    /// Named voice exposed by the configured TTS model.
    #[arg(long, global = true)]
    pub tts_voice: Option<String>,

    /// Disable the local speech layer without changing the config file.
    #[arg(long, global = true)]
    pub no_speech: bool,
}

#[derive(Clone, Debug, Subcommand)]
pub enum CliCommand {
    /// Install and verify local runtimes and model weights.
    Setup(SetupArgs),
    /// Report whether Xikoi's local dependencies are ready.
    Doctor,
    /// Run the managed localhost MLX-Audio sidecar.
    VoiceServer,
}

#[derive(Clone, Debug, Args)]
pub struct SetupArgs {
    /// Accept the displayed download and installation plan without prompting.
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// Configure chat only and skip MLX-Audio plus Hugging Face models.
    #[arg(long)]
    pub skip_voice: bool,

    /// Configure voice only and do not install or pull an Ollama model.
    #[arg(long)]
    pub skip_ollama: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileSpeechConfig {
    pub enabled: Option<bool>,
    pub url: Option<Url>,
    pub asr_model: Option<String>,
    pub tts_model: Option<String>,
    pub tts_voice: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub backend: Option<BackendKind>,
    pub ollama_url: Option<Url>,
    pub llama_url: Option<Url>,
    pub keep_alive: Option<String>,
    pub color: Option<ColorMode>,
    pub animation: Option<bool>,
    pub allow_cloud_models: bool,
    pub speech: FileSpeechConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AppStateFile {
    pub last_model: Option<String>,
    pub recent_session: Option<String>,
    pub input_device_id: Option<String>,
    pub output_device_id: Option<String>,
    pub voice_mode: bool,
}

#[derive(Clone, Debug)]
pub struct SpeechConfig {
    pub enabled: bool,
    pub url: Url,
    pub asr_model: String,
    pub tts_model: String,
    pub tts_voice: String,
}

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub chats: PathBuf,
    pub logs: PathBuf,
    pub voice_runtime: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self, ConfigError> {
        let root = if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(xdg).join("xikoi")
        } else {
            home::home_dir()
                .ok_or(ConfigError::HomeUnavailable)?
                .join(".config")
                .join("xikoi")
        };
        Ok(Self::from_root(root))
    }

    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            config: root.join("config.toml"),
            state: root.join("state.json"),
            chats: root.join("chats"),
            logs: root.join("logs"),
            voice_runtime: root.join("voice-runtime"),
            root,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub backend: BackendKind,
    pub ollama_url: Url,
    pub llama_url: Url,
    pub requested_model: Option<String>,
    pub resume: Option<String>,
    pub keep_alive: Option<String>,
    pub color: ColorMode,
    pub animation: bool,
    pub allow_cloud_models: bool,
    pub speech: SpeechConfig,
    pub paths: AppPaths,
}

impl RuntimeConfig {
    pub async fn load(cli: Cli, paths: AppPaths) -> Result<Self, ConfigError> {
        let file = load_file_config(&paths.config).await?;
        let environment = EnvironmentConfig::read()?;
        Ok(Self::resolve(cli, paths, file, environment))
    }

    fn resolve(
        cli: Cli,
        paths: AppPaths,
        file: FileConfig,
        environment: EnvironmentConfig,
    ) -> Self {
        let ollama_url = cli
            .ollama_url
            .or(environment.ollama_url)
            .or(environment.ollama_host)
            .or(file.ollama_url)
            .unwrap_or_else(|| Url::parse(DEFAULT_OLLAMA_URL).expect("default URL is valid"));
        let llama_url = cli
            .llama_url
            .or(environment.llama_url)
            .or(file.llama_url)
            .unwrap_or_else(|| Url::parse(DEFAULT_LLAMA_URL).expect("default URL is valid"));
        let backend = cli
            .backend
            .or(environment.backend)
            .or(file.backend)
            .unwrap_or_default();
        let color = cli
            .color
            .or(environment.color)
            .or(file.color)
            .unwrap_or_default();
        let animation = if cli.no_animation || environment.no_animation.unwrap_or(false) {
            false
        } else {
            file.animation.unwrap_or(true)
        };
        let speech = SpeechConfig {
            enabled: !cli.no_speech
                && environment
                    .speech_enabled
                    .or(file.speech.enabled)
                    .unwrap_or(true),
            url: cli
                .speech_url
                .or(environment.speech_url)
                .or(file.speech.url)
                .unwrap_or_else(|| {
                    Url::parse(DEFAULT_SPEECH_URL).expect("default speech URL is valid")
                }),
            asr_model: cli
                .asr_model
                .or(environment.asr_model)
                .or(file.speech.asr_model)
                .unwrap_or_else(|| DEFAULT_ASR_MODEL.to_owned()),
            tts_model: cli
                .tts_model
                .or(environment.tts_model)
                .or(file.speech.tts_model)
                .unwrap_or_else(|| DEFAULT_TTS_MODEL.to_owned()),
            tts_voice: cli
                .tts_voice
                .or(environment.tts_voice)
                .or(file.speech.tts_voice)
                .unwrap_or_else(|| DEFAULT_TTS_VOICE.to_owned()),
        };

        Self {
            backend,
            ollama_url,
            llama_url,
            requested_model: cli.model.or(environment.model),
            resume: cli.resume.or(environment.resume),
            keep_alive: environment.keep_alive.or(file.keep_alive),
            color,
            animation,
            allow_cloud_models: environment
                .allow_cloud_models
                .unwrap_or(file.allow_cloud_models),
            speech,
            paths,
        }
    }
}

#[derive(Default)]
struct EnvironmentConfig {
    ollama_url: Option<Url>,
    ollama_host: Option<Url>,
    llama_url: Option<Url>,
    backend: Option<BackendKind>,
    model: Option<String>,
    resume: Option<String>,
    keep_alive: Option<String>,
    color: Option<ColorMode>,
    no_animation: Option<bool>,
    allow_cloud_models: Option<bool>,
    speech_url: Option<Url>,
    speech_enabled: Option<bool>,
    asr_model: Option<String>,
    tts_model: Option<String>,
    tts_voice: Option<String>,
}

impl EnvironmentConfig {
    fn read() -> Result<Self, ConfigError> {
        Ok(Self {
            ollama_url: read_nonempty("XIKOI_OLLAMA_URL")
                .map(|value| normalize_ollama_host(&value))
                .transpose()?,
            ollama_host: read_nonempty("OLLAMA_HOST")
                .map(|value| normalize_ollama_host(&value))
                .transpose()?,
            llama_url: read_nonempty("XIKOI_LLAMA_URL")
                .map(|value| normalize_http_host(&value))
                .transpose()?,
            backend: read_nonempty("XIKOI_BACKEND")
                .map(|value| parse_backend("XIKOI_BACKEND", &value))
                .transpose()?,
            model: read_nonempty("XIKOI_MODEL"),
            resume: read_nonempty("XIKOI_RESUME"),
            keep_alive: read_nonempty("XIKOI_KEEP_ALIVE"),
            color: read_nonempty("XIKOI_COLOR")
                .map(|value| parse_color("XIKOI_COLOR", &value))
                .transpose()?,
            no_animation: read_nonempty("XIKOI_NO_ANIMATION")
                .map(|value| parse_bool("XIKOI_NO_ANIMATION", &value))
                .transpose()?,
            allow_cloud_models: read_nonempty("XIKOI_ALLOW_CLOUD_MODELS")
                .map(|value| parse_bool("XIKOI_ALLOW_CLOUD_MODELS", &value))
                .transpose()?,
            speech_url: read_nonempty("XIKOI_SPEECH_URL")
                .map(|value| normalize_http_host(&value))
                .transpose()?,
            speech_enabled: read_nonempty("XIKOI_SPEECH_ENABLED")
                .map(|value| parse_bool("XIKOI_SPEECH_ENABLED", &value))
                .transpose()?,
            asr_model: read_nonempty("XIKOI_ASR_MODEL"),
            tts_model: read_nonempty("XIKOI_TTS_MODEL"),
            tts_voice: read_nonempty("XIKOI_TTS_VOICE"),
        })
    }
}

async fn load_file_config(path: &Path) -> Result<FileConfig, ConfigError> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => toml::from_str(&contents).map_err(ConfigError::InvalidFile),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(error) => Err(ConfigError::Io(error)),
    }
}

fn normalize_ollama_host(value: &str) -> Result<Url, ConfigError> {
    normalize_http_host(value)
}

fn normalize_http_host(value: &str) -> Result<Url, ConfigError> {
    if value.contains("://") {
        return Url::parse(value).map_err(ConfigError::InvalidUrl);
    }
    Url::parse(&format!("http://{value}")).map_err(ConfigError::InvalidUrl)
}

fn read_nonempty(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn parse_color(name: &'static str, value: &str) -> Result<ColorMode, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "auto" => Ok(ColorMode::Auto),
        "truecolor" => Ok(ColorMode::Truecolor),
        "ansi" => Ok(ColorMode::Ansi),
        "mono" => Ok(ColorMode::Mono),
        _ => Err(ConfigError::InvalidEnvironment {
            name,
            value: value.to_owned(),
        }),
    }
}

fn parse_backend(name: &'static str, value: &str) -> Result<BackendKind, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "ollama" => Ok(BackendKind::Ollama),
        "llama_cpp" | "llama-cpp" | "llamacpp" => Ok(BackendKind::LlamaCpp),
        _ => Err(ConfigError::InvalidEnvironment {
            name,
            value: value.to_owned(),
        }),
    }
}

fn parse_bool(name: &'static str, value: &str) -> Result<bool, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::InvalidEnvironment {
            name,
            value: value.to_owned(),
        }),
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine the home directory")]
    HomeUnavailable,
    #[error("could not read configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error("config.toml is invalid: {0}")]
    InvalidFile(#[from] toml::de::Error),
    #[error("endpoint URL is invalid: {0}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("{name} has invalid value `{value}`")]
    InvalidEnvironment { name: &'static str, value: String },
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn paths_default_to_requested_xdg_shape() {
        let paths = AppPaths::from_root("/tmp/config/xikoi");
        assert_eq!(paths.chats, PathBuf::from("/tmp/config/xikoi/chats"));
        assert_eq!(paths.state, PathBuf::from("/tmp/config/xikoi/state.json"));
        assert_eq!(
            paths.voice_runtime,
            PathBuf::from("/tmp/config/xikoi/voice-runtime")
        );
    }

    #[test]
    fn normalizes_bare_ollama_host() {
        let url = normalize_ollama_host("localhost:11434").unwrap();
        assert_eq!(url.as_str(), "http://localhost:11434/");
    }

    #[test]
    fn configuration_precedence_is_cli_then_xikoi_then_ollama_then_file() {
        let paths = AppPaths::from_root("/tmp/config/xikoi");
        let file = FileConfig {
            backend: None,
            ollama_url: Some(Url::parse("http://file:11434").unwrap()),
            llama_url: None,
            keep_alive: Some("5m".to_owned()),
            color: Some(ColorMode::Mono),
            animation: Some(true),
            allow_cloud_models: false,
            speech: FileSpeechConfig::default(),
        };
        let environment = EnvironmentConfig {
            ollama_url: Some(Url::parse("http://xikoi-env:11434").unwrap()),
            ollama_host: Some(Url::parse("http://ollama-env:11434").unwrap()),
            model: Some("env-model".to_owned()),
            color: Some(ColorMode::Ansi),
            ..EnvironmentConfig::default()
        };
        let cli = Cli::try_parse_from([
            "xikoi",
            "--ollama-url",
            "http://cli:11434",
            "--model",
            "cli-model",
            "--color",
            "truecolor",
        ])
        .unwrap();

        let resolved = RuntimeConfig::resolve(cli, paths, file, environment);
        assert_eq!(resolved.ollama_url.as_str(), "http://cli:11434/");
        assert_eq!(resolved.requested_model.as_deref(), Some("cli-model"));
        assert_eq!(resolved.color, ColorMode::Truecolor);
    }

    #[test]
    fn xikoi_endpoint_precedes_ollama_host_and_file() {
        let environment = EnvironmentConfig {
            ollama_url: Some(Url::parse("http://xikoi-env:11434").unwrap()),
            ollama_host: Some(Url::parse("http://ollama-env:11434").unwrap()),
            ..EnvironmentConfig::default()
        };
        let file = FileConfig {
            ollama_url: Some(Url::parse("http://file:11434").unwrap()),
            ..FileConfig::default()
        };
        let resolved = RuntimeConfig::resolve(
            Cli::try_parse_from(["xikoi"]).unwrap(),
            AppPaths::from_root("/tmp/config/xikoi"),
            file,
            environment,
        );

        assert_eq!(resolved.ollama_url.as_str(), "http://xikoi-env:11434/");
    }

    #[test]
    fn validates_boolean_and_color_environment_values() {
        assert!(parse_bool("XIKOI_NO_ANIMATION", "yes").unwrap());
        assert!(!parse_bool("XIKOI_NO_ANIMATION", "0").unwrap());
        assert_eq!(
            parse_color("XIKOI_COLOR", "TRUECOLOR").unwrap(),
            ColorMode::Truecolor
        );
        assert!(parse_bool("XIKOI_NO_ANIMATION", "sometimes").is_err());
        assert!(parse_color("XIKOI_COLOR", "rainbow").is_err());
        assert_eq!(
            parse_backend("XIKOI_BACKEND", "llama-cpp").unwrap(),
            BackendKind::LlamaCpp
        );
    }

    #[test]
    fn speech_defaults_to_local_quantized_qwen_models() {
        let resolved = RuntimeConfig::resolve(
            Cli::try_parse_from(["xikoi"]).unwrap(),
            AppPaths::from_root("/tmp/config/xikoi"),
            FileConfig::default(),
            EnvironmentConfig::default(),
        );

        assert!(resolved.speech.enabled);
        assert_eq!(resolved.speech.url.as_str(), "http://127.0.0.1:8000/");
        assert_eq!(resolved.speech.asr_model, DEFAULT_ASR_MODEL);
        assert_eq!(resolved.speech.tts_model, DEFAULT_TTS_MODEL);
        assert_eq!(resolved.speech.tts_voice, DEFAULT_TTS_VOICE);
    }
}
