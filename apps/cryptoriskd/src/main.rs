mod runtime;
mod startup;

use std::{
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

use clap::Parser;
use config::{
    ConfigError, ConfigLayers, EffectiveConfig, EnvironmentOverrides, LogLevel, Overrides,
    RuntimeOverrides, TextLayer,
};
use observability::LocalLogLevel;
use runtime::{RuntimeError, RuntimeLimits, RuntimeOptions, start_fixture_runtime_with_limits};
use serde_json::json;
#[cfg(debug_assertions)]
use startup::wait_readiness_gate_from_fd;
use startup::{
    StartupError, issue_session_descriptor, open_wal_file, read_session_secret_from_fd_async,
};
use thiserror::Error;
use tokio::{runtime::Runtime, sync::watch};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Debug, Parser)]
#[command(name = "cryptoriskd")]
struct Arguments {
    #[arg(long)]
    approved_root: PathBuf,
    #[arg(long)]
    system_policy: Option<PathBuf>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    ingestion_queue_capacity: Option<usize>,
    #[arg(long)]
    maximum_concurrent_requests: Option<usize>,
    #[arg(long)]
    request_timeout_seconds: Option<u64>,
    #[arg(long)]
    log_level: Option<LogLevel>,
    #[cfg(debug_assertions)]
    #[arg(long, hide = true)]
    readiness_gate_fd: Option<u32>,
}

fn main() {
    if let Err(error) = run_main() {
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

fn run_main() -> Result<(), AppError> {
    let arguments = Arguments::parse();
    let prepared = prepare(arguments)?;
    let runtime = build_async_runtime(prepared.effective.config.runtime_threads())
        .map_err(AppError::AsyncRuntime)?;
    runtime.block_on(run(prepared))
}

struct PreparedStartup {
    arguments: Arguments,
    effective: EffectiveConfig,
    runtime_limits: RuntimeLimits,
}

fn prepare(arguments: Arguments) -> Result<PreparedStartup, AppError> {
    let system_config = arguments
        .system_policy
        .as_deref()
        .map(|path| config::read_bounded_file(path, MAX_CONFIG_BYTES))
        .transpose()?;
    let system_source = arguments
        .system_policy
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let user_config = arguments
        .config
        .as_deref()
        .map(|path| config::read_bounded_file(path, MAX_CONFIG_BYTES))
        .transpose()?;
    let user_source = arguments
        .config
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let effective = config::load_layers(
        ConfigLayers {
            compiled_defaults: TextLayer::new(
                include_str!("../../../configs/default.toml"),
                "embedded-default",
            ),
            system_policy: system_config
                .as_deref()
                .zip(system_source.as_deref())
                .map(|(text, source)| TextLayer::new(text, source)),
            user_config: user_config
                .as_deref()
                .zip(user_source.as_deref())
                .map(|(text, source)| TextLayer::new(text, source)),
            environment: supported_environment_overrides()?,
            command_line: Overrides {
                ingestion_queue_capacity: arguments.ingestion_queue_capacity,
                maximum_concurrent_requests: arguments.maximum_concurrent_requests,
                request_timeout_seconds: arguments.request_timeout_seconds,
                log_level: arguments.log_level,
                ..Overrides::default()
            },
            runtime_settings: RuntimeOverrides::default(),
        },
        &arguments.approved_root,
    )?;
    effective.config.validate_foundation_runtime()?;
    let shutdown_grace = Duration::from_secs(effective.config.shutdown_grace_seconds());
    let runtime_limits = RuntimeLimits::new(
        effective.config.ingestion_queue_capacity(),
        effective.config.maximum_request_bytes(),
        effective.config.maximum_concurrent_requests(),
        Duration::from_secs(effective.config.request_timeout_seconds()),
        shutdown_grace,
        local_log_level(effective.config.log_level()),
    )?;

    Ok(PreparedStartup {
        arguments,
        effective,
        runtime_limits,
    })
}

const fn local_log_level(level: LogLevel) -> LocalLogLevel {
    match level {
        LogLevel::Error => LocalLogLevel::Error,
        LogLevel::Warn => LocalLogLevel::Warn,
        LogLevel::Info => LocalLogLevel::Info,
        LogLevel::Debug => LocalLogLevel::Debug,
        LogLevel::Trace => LocalLogLevel::Trace,
    }
}

fn build_async_runtime(worker_threads: usize) -> io::Result<Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .thread_name("cryptoriskd-worker")
        .enable_all()
        .build()
}

async fn run(prepared: PreparedStartup) -> Result<(), AppError> {
    let PreparedStartup {
        arguments,
        effective,
        runtime_limits,
    } = prepared;
    let mut cancellation = install_shutdown_listener()?;
    let mut secret_cancellation = cancellation.clone();
    let secret = tokio::select! {
        biased;
        () = wait_for_cancellation(&mut secret_cancellation) => return Ok(()),
        result = read_session_secret_from_fd_async(effective.config.session_secret_fd()) => result?,
    };
    let descriptor = issue_session_descriptor()?;
    let fixture = effective.paths.fixture_input.try_clone()?;
    let wal = open_wal_file(&effective.paths.data_root)?;
    let log = startup::open_rotating_log(&effective.paths.log_root)?;
    let running = match start_fixture_runtime_with_limits(
        RuntimeOptions {
            fixture,
            wal,
            log,
            secret,
            descriptor,
            cancellation: cancellation.clone(),
        },
        runtime_limits,
    )
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

fn supported_environment_overrides() -> Result<EnvironmentOverrides, ConfigError> {
    const SUPPORTED: [&str; 3] = [
        "CMTI_LOG_LEVEL",
        "CMTI_REQUEST_TIMEOUT_SECONDS",
        "CMTI_MAXIMUM_CONCURRENT_REQUESTS",
    ];
    let mut pairs = Vec::new();
    for name in SUPPORTED {
        if let Some(value) = std::env::var_os(name) {
            let value = value
                .into_string()
                .map_err(|_| ConfigError::InvalidOverride {
                    name: name.to_owned(),
                })?;
            pairs.push((name, value));
        }
    }
    EnvironmentOverrides::from_pairs(pairs)
}

async fn shutdown_running(running: runtime::RunningDaemon) -> Result<(), AppError> {
    running.shutdown().await?;
    Ok(())
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
    #[error("async runtime initialization failed: {0}")]
    AsyncRuntime(io::Error),
}

#[cfg(test)]
mod tests {
    use super::{build_async_runtime, local_log_level};
    use config::LogLevel;
    use observability::LocalLogLevel;

    #[test]
    fn configured_worker_thread_count_is_applied_to_the_async_runtime() {
        let runtime = build_async_runtime(4).expect("configured Tokio runtime must build");

        assert_eq!(runtime.metrics().num_workers(), 4);
    }

    #[test]
    fn every_configured_log_level_preserves_its_exact_severity() {
        assert_eq!(local_log_level(LogLevel::Error), LocalLogLevel::Error);
        assert_eq!(local_log_level(LogLevel::Warn), LocalLogLevel::Warn);
        assert_eq!(local_log_level(LogLevel::Info), LocalLogLevel::Info);
        assert_eq!(local_log_level(LogLevel::Debug), LocalLogLevel::Debug);
        assert_eq!(local_log_level(LogLevel::Trace), LocalLogLevel::Trace);
    }
}
