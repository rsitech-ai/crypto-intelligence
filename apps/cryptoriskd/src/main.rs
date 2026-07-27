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
#[cfg(debug_assertions)]
use startup::wait_readiness_gate_from_fd;
use startup::{
    StartupError, issue_session_descriptor, open_wal_file, read_session_secret_from_fd_async,
};
use thiserror::Error;
use tokio::sync::watch;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const REQUIRED_SHUTDOWN_SECONDS: u64 = 5;

#[derive(Debug, Parser)]
#[command(name = "cryptoriskd")]
struct Arguments {
    #[arg(long)]
    approved_root: PathBuf,
    #[arg(long)]
    config: Option<PathBuf>,
    #[cfg(debug_assertions)]
    #[arg(long, hide = true)]
    readiness_gate_fd: Option<u32>,
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
    let mut cancellation = install_shutdown_listener()?;
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

    let mut secret_cancellation = cancellation.clone();
    let secret = tokio::select! {
        biased;
        () = wait_for_cancellation(&mut secret_cancellation) => return Ok(()),
        result = read_session_secret_from_fd_async(effective.config.session_secret_fd()) => result?,
    };
    let descriptor = issue_session_descriptor()?;
    let fixture = effective.paths.fixture_input.try_clone()?;
    let wal = open_wal_file(&effective.paths.data_root)?;
    let log = startup::open_log_file(&effective.paths.log_root)?;
    let running = match start_fixture_runtime(RuntimeOptions {
        fixture,
        wal,
        log,
        secret,
        descriptor,
        cancellation: cancellation.clone(),
    })
    .await
    {
        Ok(running) => running,
        Err(RuntimeError::Cancelled) => return Ok(()),
        Err(error) => return Err(error.into()),
    };

    if is_cancelled(&cancellation) {
        shutdown_running(running).await?;
        return Ok(());
    }
    #[cfg(debug_assertions)]
    if let Some(fd) = arguments.readiness_gate_fd {
        let mut readiness_cancellation = cancellation.clone();
        let cancelled = tokio::select! {
            biased;
            () = wait_for_cancellation(&mut readiness_cancellation) => true,
            result = wait_readiness_gate_from_fd(fd) => {
                result?;
                false
            }
        };
        if cancelled {
            shutdown_running(running).await?;
            return Ok(());
        }
    }
    if is_cancelled(&cancellation) {
        shutdown_running(running).await?;
        return Ok(());
    }

    {
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, running.readiness())?;
        writeln!(stdout)?;
        stdout.flush()?;
    }

    wait_for_cancellation(&mut cancellation).await;
    shutdown_running(running).await
}

async fn shutdown_running(running: runtime::RunningDaemon) -> Result<(), AppError> {
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

fn install_shutdown_listener() -> Result<watch::Receiver<bool>, io::Error> {
    let (sender, receiver) = watch::channel(false);
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        tokio::spawn(async move {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
            let _ = sender.send(true);
        });
    }
    #[cfg(not(unix))]
    {
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = sender.send(true);
        });
    }
    Ok(receiver)
}

async fn wait_for_cancellation(cancellation: &mut watch::Receiver<bool>) {
    while !*cancellation.borrow() {
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}

fn is_cancelled(cancellation: &watch::Receiver<bool>) -> bool {
    *cancellation.borrow()
}

#[derive(Debug, Error)]
enum AppError {
    #[error("configuration file exceeds the local size bound")]
    ConfigTooLarge,
    #[error("configuration must set ingestion_queue_capacity=1024 and shutdown_grace_seconds=5")]
    RuntimeContract,
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
