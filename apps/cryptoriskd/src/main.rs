mod runtime;
mod startup;

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use clap::Parser;
use config::{ConfigError, Overrides};
use runtime::{RuntimeError, RuntimeOptions, start_fixture_runtime};
use serde_json::json;
use startup::{StartupError, issue_session_descriptor, open_wal_file, read_session_secret_from_fd};
use thiserror::Error;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const REQUIRED_SHUTDOWN_SECONDS: u64 = 5;

#[derive(Debug, Parser)]
#[command(name = "cryptoriskd")]
struct Arguments {
    #[arg(long)]
    approved_root: PathBuf,
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        let diagnostic = json!({
            "level": "error",
            "event": "fixture_runtime_failed",
            "message": error.to_string()
        });
        let mut stderr = io::stderr().lock();
        let _ = serde_json::to_writer(&mut stderr, &diagnostic);
        let _ = writeln!(stderr);
        let _ = stderr.flush();
        std::process::exit(1);
    }
}

async fn run() -> Result<(), AppError> {
    let arguments = Arguments::parse();
    let user_config = arguments
        .config
        .as_deref()
        .map(read_bounded_config)
        .transpose()?;
    let user_source = arguments
        .config
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let user_layer = user_config.as_deref().zip(user_source.as_deref());
    let effective = config::load(
        include_str!("../../../configs/default.toml"),
        "embedded-default",
        user_layer,
        Overrides::default(),
        &arguments.approved_root,
    )?;
    if effective.config.ingestion_queue_capacity() != runtime::INGESTION_QUEUE_CAPACITY
        || effective.config.shutdown_grace_seconds() != REQUIRED_SHUTDOWN_SECONDS
    {
        return Err(AppError::RuntimeContract);
    }

    let secret = read_session_secret_from_fd(effective.config.session_secret_fd())?;
    let descriptor = issue_session_descriptor()?;
    let fixture = effective.paths.fixture_input.try_clone()?;
    let wal = open_wal_file(&effective.paths.data_root)?;
    let log = startup::open_log_file(&effective.paths.log_root)?;
    let running = start_fixture_runtime(RuntimeOptions {
        fixture,
        wal,
        log,
        secret,
        descriptor,
    })
    .await?;

    {
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, running.readiness())?;
        writeln!(stdout)?;
        stdout.flush()?;
    }

    shutdown_signal().await?;
    tokio::time::timeout(
        Duration::from_secs(REQUIRED_SHUTDOWN_SECONDS),
        running.shutdown(),
    )
    .await
    .map_err(|_| AppError::ShutdownTimeout)??;
    Ok(())
}

fn read_bounded_config(path: &Path) -> Result<String, AppError> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(AppError::ConfigTooLarge);
    }
    Ok(fs::read_to_string(path)?)
}

async fn shutdown_signal() -> Result<(), AppError> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(AppError::Signal),
            signal = terminate.recv() => {
                signal.ok_or(AppError::SignalClosed)?;
                Ok(())
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.map_err(AppError::Signal)
    }
}

#[derive(Debug, Error)]
enum AppError {
    #[error("configuration file exceeds the local size bound")]
    ConfigTooLarge,
    #[error("configuration must set ingestion_queue_capacity=1024 and shutdown_grace_seconds=5")]
    RuntimeContract,
    #[error("shutdown signal stream closed unexpectedly")]
    SignalClosed,
    #[error("healthy shutdown exceeded five seconds")]
    ShutdownTimeout,
    #[error("configuration failed: {0}")]
    Config(#[from] ConfigError),
    #[error("startup failed: {0}")]
    Startup(#[from] StartupError),
    #[error("runtime failed: {0}")]
    Runtime(#[from] RuntimeError),
    #[error("local file or signal I/O failed: {0}")]
    Signal(#[from] io::Error),
    #[error("readiness serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}
