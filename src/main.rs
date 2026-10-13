use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use chrono::Utc;
use clap::Parser;
use color_eyre::eyre::{Context, Result};
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;
use xikoi::{
    app::App,
    backend::{ChatBackend, llama_cpp::LlamaCppBackend, ollama::OllamaBackend},
    config::{AppPaths, BackendKind, Cli, CliCommand, RuntimeConfig},
    setup,
    storage::Storage,
    voice_runtime::VoiceRuntime,
};

const LOG_FILE_RETENTION: usize = 10;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let mut cli = Cli::parse();
    let paths = AppPaths::discover()?;
    let command = cli.command.take();
    let config = RuntimeConfig::load(cli, paths.clone()).await?;

    match command {
        Some(CliCommand::Setup(arguments)) => return setup::run(&config, &arguments).await,
        Some(CliCommand::Doctor) => return setup::doctor(&config).await,
        Some(CliCommand::VoiceServer) => return setup::voice_server(&config),
        None => {}
    }

    let storage = Storage::new(paths.clone());
    storage.initialize().await?;
    let _log_guard = initialize_logging(&paths)?;
    let _voice_server = if config.speech.enabled {
        match VoiceRuntime::from_paths(&config.paths)
            .spawn_managed(&config.speech, &config.paths.logs)
            .await
        {
            Ok(server) => server,
            Err(error) => {
                tracing::warn!(%error, "managed voice server could not start");
                None
            }
        }
    } else {
        None
    };
    let backend: Arc<dyn ChatBackend> = match config.backend {
        BackendKind::Ollama => Arc::new(
            OllamaBackend::new(config.ollama_url.clone())
                .wrap_err("could not create Ollama client")?,
        ),
        BackendKind::LlamaCpp => Arc::new(
            LlamaCppBackend::new(config.llama_url.clone())
                .wrap_err("could not create llama.cpp client")?,
        ),
    };
    let mut app = App::bootstrap(config, storage, backend).await?;

    let mut terminal = ratatui::init();
    let terminal_session = TerminalSession::new();
    TerminalSession::enable_bracketed_paste()?;
    let result = app.run(&mut terminal).await;
    terminal_session.restore();
    result
}

struct TerminalSession {
    active: Arc<AtomicBool>,
}

impl TerminalSession {
    fn new() -> Self {
        let active = Arc::new(AtomicBool::new(true));
        let panic_active = Arc::clone(&active);
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            if panic_active.swap(false, Ordering::SeqCst) {
                let _ = execute!(io::stdout(), DisableBracketedPaste);
            }
            previous_hook(panic_info);
        }));

        Self { active }
    }

    fn enable_bracketed_paste() -> Result<()> {
        execute!(io::stdout(), EnableBracketedPaste)
            .wrap_err("could not enable bracketed paste mode")?;
        Ok(())
    }

    fn restore(&self) {
        if self.active.swap(false, Ordering::SeqCst) {
            if let Err(error) = execute!(io::stdout(), DisableBracketedPaste) {
                tracing::warn!(%error, "could not disable bracketed paste mode");
            }
            ratatui::restore();
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.restore();
    }
}

fn initialize_logging(paths: &AppPaths) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let log_file = open_private_log(&paths.logs)?;
    let (writer, guard) = tracing_appender::non_blocking(log_file);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("xikoi=info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_writer(writer),
        )
        .try_init()
        .wrap_err("could not initialize diagnostics")?;
    Ok(guard)
}

fn open_private_log(directory: &Path) -> Result<File> {
    prune_old_logs(directory)?;
    let filename = format!(
        "xikoi-{}-{}.log",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        Uuid::now_v7()
    );
    let mut options = OpenOptions::new();
    options.create_new(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(directory.join(filename))
        .wrap_err("could not create private diagnostic log")
}

fn prune_old_logs(directory: &Path) -> Result<()> {
    let mut logs = std::fs::read_dir(directory)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("xikoi-"))
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("log"))
        })
        .map(|entry| {
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (modified, entry.path())
        })
        .collect::<Vec<_>>();
    logs.sort_by_key(|(modified, _)| *modified);

    let remove_count = logs.len().saturating_sub(LOG_FILE_RETENTION - 1);
    for (_, path) in logs.into_iter().take(remove_count) {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn diagnostics_are_private_and_retained_with_a_fixed_bound() {
        let temporary = TempDir::new().unwrap();
        for _ in 0..(LOG_FILE_RETENTION + 2) {
            drop(open_private_log(temporary.path()).unwrap());
        }

        let logs = std::fs::read_dir(temporary.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(logs.len(), LOG_FILE_RETENTION);
        for path in logs {
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
