use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::Arc,
    time::SystemTime,
};

use chrono::Utc;
use clap::Parser;
use color_eyre::eyre::{Context, Result};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;
use xikoi::{
    app::App,
    backend::ollama::OllamaBackend,
    config::{AppPaths, Cli, RuntimeConfig},
    storage::Storage,
};

const LOG_FILE_RETENTION: usize = 10;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();
    let paths = AppPaths::discover()?;
    let storage = Storage::new(paths.clone());
    storage.initialize().await?;
    let _log_guard = initialize_logging(&paths)?;
    let config = RuntimeConfig::load(cli, paths).await?;
    let backend = Arc::new(
        OllamaBackend::new(config.ollama_url.clone()).wrap_err("could not create Ollama client")?,
    );
    let mut app = App::bootstrap(config, storage, backend).await?;

    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal).await;
    ratatui::restore();
    result
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
