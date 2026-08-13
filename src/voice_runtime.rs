//! Installation and lifecycle management for Xikoi's pinned MLX-Audio sidecar.

use std::{
    env,
    fs::{File, OpenOptions},
    net::IpAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

use thiserror::Error;
use tokio::time::{Instant, sleep};
use url::{Host, Url};

use crate::{
    config::{AppPaths, SpeechConfig},
    speech::{MlxAudioClient, SpeechFailure},
};

const PYPROJECT_TOML: &str = include_str!("../pyproject.toml");
const UV_LOCK: &str = include_str!("../uv.lock");
const SERVER_START_TIMEOUT: Duration = Duration::from_mins(1);

const MODEL_DOWNLOAD_SCRIPT: &str = r#"
import sys
from huggingface_hub import snapshot_download

patterns = [
    "*.json", "*.safetensors", "*.py", "*.model", "*.tiktoken",
    "*.txt", "*.jinja", "*.jsonl", "*.yaml", "*.npz", "*.pth",
]

for model in sys.argv[1:]:
    print(f"Downloading {model}...", flush=True)
    path = snapshot_download(repo_id=model, allow_patterns=patterns)
    print(f"Ready: {model} ({path})", flush=True)
"#;

const MODEL_CACHE_CHECK_SCRIPT: &str = r#"
import sys
from huggingface_hub import snapshot_download

patterns = [
    "*.json", "*.safetensors", "*.py", "*.model", "*.tiktoken",
    "*.txt", "*.jinja", "*.jsonl", "*.yaml", "*.npz", "*.pth",
]

for model in sys.argv[1:]:
    snapshot_download(repo_id=model, allow_patterns=patterns, local_files_only=True)
"#;

#[derive(Clone, Debug)]
pub struct VoiceRuntime {
    root: PathBuf,
}

impl VoiceRuntime {
    pub fn from_paths(paths: &AppPaths) -> Self {
        Self {
            root: paths.voice_runtime.clone(),
        }
    }

    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn python(&self) -> PathBuf {
        self.root.join(".venv/bin/python")
    }

    pub fn server(&self) -> PathBuf {
        self.root.join(".venv/bin/mlx_audio.server")
    }

    pub fn is_installed(&self) -> bool {
        self.python().is_file() && self.server().is_file()
    }

    pub async fn materialize(&self) -> Result<(), VoiceRuntimeError> {
        tokio::fs::create_dir_all(&self.root).await?;
        make_private_directory(&self.root)?;
        write_private_if_changed(&self.root.join("pyproject.toml"), PYPROJECT_TOML).await?;
        write_private_if_changed(&self.root.join("uv.lock"), UV_LOCK).await?;
        Ok(())
    }

    pub async fn sync(&self) -> Result<(), VoiceRuntimeError> {
        let uv = find_program("uv").ok_or(VoiceRuntimeError::MissingProgram("uv"))?;
        let status = tokio::process::Command::new(uv)
            .arg("sync")
            .arg("--frozen")
            .arg("--project")
            .arg(&self.root)
            .arg("--python")
            .arg("3.12")
            .env("UV_PROJECT_ENVIRONMENT", self.root.join(".venv"))
            .stdin(Stdio::null())
            .status()
            .await?;
        if !status.success() {
            return Err(VoiceRuntimeError::CommandFailed {
                command: "uv sync".to_owned(),
                status: status.code(),
            });
        }
        if !self.is_installed() {
            return Err(VoiceRuntimeError::IncompleteInstallation(self.root.clone()));
        }
        Ok(())
    }

    pub async fn download_models(
        &self,
        asr_model: &str,
        tts_model: &str,
    ) -> Result<(), VoiceRuntimeError> {
        self.run_model_script(MODEL_DOWNLOAD_SCRIPT, asr_model, tts_model)
            .await
    }

    pub async fn models_cached(
        &self,
        asr_model: &str,
        tts_model: &str,
    ) -> Result<bool, VoiceRuntimeError> {
        if !self.is_installed() {
            return Ok(false);
        }
        let status = tokio::process::Command::new(self.python())
            .arg("-c")
            .arg(MODEL_CACHE_CHECK_SCRIPT)
            .arg(asr_model)
            .arg(tts_model)
            .env("HF_HUB_DISABLE_TELEMETRY", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;
        Ok(status.success())
    }

    pub fn run_foreground(&self, speech: &SpeechConfig) -> Result<(), VoiceRuntimeError> {
        if !self.is_installed() {
            return Err(VoiceRuntimeError::IncompleteInstallation(self.root.clone()));
        }
        let endpoint = local_endpoint(&speech.url)?;
        let status = server_command(self, &endpoint)
            .stdin(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(VoiceRuntimeError::CommandFailed {
                command: "mlx_audio.server".to_owned(),
                status: status.code(),
            })
        }
    }

    pub async fn spawn_managed(
        &self,
        speech: &SpeechConfig,
        logs: &Path,
    ) -> Result<Option<ManagedVoiceServer>, VoiceRuntimeError> {
        let client = MlxAudioClient::new(speech.url.clone())?;
        if client.probe().await.is_ok() {
            return Ok(None);
        }
        let endpoint = local_endpoint(&speech.url)?;
        if !self.is_installed() {
            return Ok(None);
        }

        std::fs::create_dir_all(logs)?;
        make_private_directory(logs)?;
        let log = open_private_append(&logs.join("voice-server.log"))?;
        let error_log = log.try_clone()?;
        let mut child = server_command(self, &endpoint)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(error_log))
            .spawn()?;

        let deadline = Instant::now() + SERVER_START_TIMEOUT;
        let mut last_error = "server did not answer its health check".to_owned();
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait()? {
                return Err(VoiceRuntimeError::ServerExited(status.code()));
            }
            match client.probe().await {
                Ok(_) => return Ok(Some(ManagedVoiceServer { child })),
                Err(error) => last_error = error.to_string(),
            }
            sleep(Duration::from_millis(250)).await;
        }

        let _ = child.kill();
        let _ = child.wait();
        Err(VoiceRuntimeError::ServerStartTimeout(last_error))
    }

    async fn run_model_script(
        &self,
        script: &str,
        asr_model: &str,
        tts_model: &str,
    ) -> Result<(), VoiceRuntimeError> {
        if !self.is_installed() {
            return Err(VoiceRuntimeError::IncompleteInstallation(self.root.clone()));
        }
        let status = tokio::process::Command::new(self.python())
            .arg("-c")
            .arg(script)
            .arg(asr_model)
            .arg(tts_model)
            .env("HF_HUB_DISABLE_TELEMETRY", "1")
            .stdin(Stdio::null())
            .status()
            .await?;
        if status.success() {
            Ok(())
        } else {
            Err(VoiceRuntimeError::CommandFailed {
                command: "Hugging Face model download".to_owned(),
                status: status.code(),
            })
        }
    }
}

pub struct ManagedVoiceServer {
    child: Child,
}

impl Drop for ManagedVoiceServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Debug)]
struct LocalEndpoint {
    host: String,
    port: u16,
}

fn local_endpoint(url: &Url) -> Result<LocalEndpoint, VoiceRuntimeError> {
    if url.scheme() != "http" {
        return Err(VoiceRuntimeError::NonLocalSpeechUrl(url.clone()));
    }
    let host = match url.host() {
        Some(Host::Domain(host)) if host.eq_ignore_ascii_case("localhost") => {
            "localhost".to_owned()
        }
        Some(Host::Ipv4(address)) if IpAddr::V4(address).is_loopback() => address.to_string(),
        Some(Host::Ipv6(address)) if IpAddr::V6(address).is_loopback() => address.to_string(),
        _ => return Err(VoiceRuntimeError::NonLocalSpeechUrl(url.clone())),
    };
    if !matches!(url.path(), "" | "/") || url.query().is_some() {
        return Err(VoiceRuntimeError::NonLocalSpeechUrl(url.clone()));
    }
    Ok(LocalEndpoint {
        host,
        port: url.port_or_known_default().unwrap_or(8000),
    })
}

fn server_command(runtime: &VoiceRuntime, endpoint: &LocalEndpoint) -> Command {
    let mut command = Command::new(runtime.server());
    command
        .arg("--host")
        .arg(&endpoint.host)
        .arg("--port")
        .arg(endpoint.port.to_string())
        .current_dir(runtime.root())
        .env("HF_HUB_DISABLE_TELEMETRY", "1")
        .env("TOKENIZERS_PARALLELISM", "false");
    command
}

pub fn find_program(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 && candidate.is_file() {
        return Some(candidate.to_owned());
    }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

async fn write_private_if_changed(path: &Path, contents: &str) -> Result<(), VoiceRuntimeError> {
    if tokio::fs::read_to_string(path)
        .await
        .is_ok_and(|current| current == contents)
    {
        return Ok(());
    }
    tokio::fs::write(path, contents).await?;
    make_private_file(path)?;
    Ok(())
}

fn open_private_append(path: &Path) -> Result<File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(unix)]
fn make_private_directory(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn make_private_directory(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
fn make_private_file(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn make_private_file(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[derive(Debug, Error)]
pub enum VoiceRuntimeError {
    #[error("required program `{0}` is not installed")]
    MissingProgram(&'static str),
    #[error("the MLX-Audio runtime is incomplete under {0}")]
    IncompleteInstallation(PathBuf),
    #[error("managed MLX-Audio requires a loopback HTTP URL, not {0}")]
    NonLocalSpeechUrl(Url),
    #[error("{command} failed with status {status:?}")]
    CommandFailed {
        command: String,
        status: Option<i32>,
    },
    #[error("MLX-Audio exited during startup with status {0:?}")]
    ServerExited(Option<i32>),
    #[error("MLX-Audio did not become ready: {0}")]
    ServerStartTimeout(String),
    #[error("speech client error: {0}")]
    Speech(#[from] SpeechFailure),
    #[error("voice runtime I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn materializes_the_pinned_runtime_privately() {
        let temporary = TempDir::new().unwrap();
        let runtime = VoiceRuntime::from_root(temporary.path().join("voice"));
        runtime.materialize().await.unwrap();

        assert_eq!(
            std::fs::read_to_string(runtime.root().join("pyproject.toml")).unwrap(),
            PYPROJECT_TOML
        );
        assert_eq!(
            std::fs::read_to_string(runtime.root().join("uv.lock")).unwrap(),
            UV_LOCK
        );
    }

    #[test]
    fn managed_server_only_accepts_loopback_http() {
        assert!(local_endpoint(&Url::parse("http://127.0.0.1:8000").unwrap()).is_ok());
        assert!(local_endpoint(&Url::parse("http://[::1]:8000").unwrap()).is_ok());
        assert!(local_endpoint(&Url::parse("https://127.0.0.1:8000").unwrap()).is_err());
        assert!(local_endpoint(&Url::parse("http://example.com:8000").unwrap()).is_err());
        assert!(local_endpoint(&Url::parse("http://127.0.0.1:8000/api").unwrap()).is_err());
    }

    #[test]
    fn managed_server_binds_the_configured_loopback_address() {
        let ipv6 = local_endpoint(&Url::parse("http://[::1]:8123").unwrap()).unwrap();
        assert_eq!(ipv6.host, "::1");
        assert_eq!(ipv6.port, 8123);

        let ipv4 = local_endpoint(&Url::parse("http://127.0.0.1:8000").unwrap()).unwrap();
        assert_eq!(ipv4.host, "127.0.0.1");

        let localhost = local_endpoint(&Url::parse("http://localhost:8000").unwrap()).unwrap();
        assert_eq!(localhost.host, "localhost");
    }
}
