use std::{
    fs::File,
    io::{self, Read},
    time::{SystemTime, UNIX_EPOCH},
};

use config::ValidatedDirectory;
use local_api::{
    auth::{AuthError, SessionSecret},
    session::{SessionDescriptor, SessionError},
};
use rand::{RngCore, rngs::OsRng};
use rustix::fs::{Mode, OFlags};
use thiserror::Error;
use zeroize::Zeroizing;

const SESSION_SECRET_LENGTH: usize = 32;
const PROTOCOL_MAJOR: u32 = 1;
const PROTOCOL_MINOR: u32 = 0;
const WAL_FILE_NAME: &str = "market.wal";
const LOG_FILE_NAME: &str = "cmti.jsonl";

pub fn read_session_secret(mut reader: impl Read) -> Result<SessionSecret, StartupError> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(SESSION_SECRET_LENGTH + 1));
    reader
        .by_ref()
        .take((SESSION_SECRET_LENGTH + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(StartupError::SecretIo)?;
    if bytes.len() != SESSION_SECRET_LENGTH {
        return Err(StartupError::SecretLength {
            actual: bytes.len(),
        });
    }
    SessionSecret::try_from(bytes).map_err(StartupError::Secret)
}

pub fn read_session_secret_from_fd(fd: u32) -> Result<SessionSecret, StartupError> {
    let file = File::open(format!("/dev/fd/{fd}")).map_err(StartupError::SecretIo)?;
    read_session_secret(file)
}

pub fn issue_session_descriptor() -> Result<SessionDescriptor, StartupError> {
    let mut process_nonce = [0_u8; 16];
    let mut server_nonce = [0_u8; 16];
    OsRng
        .try_fill_bytes(&mut process_nonce)
        .map_err(|_| StartupError::Random)?;
    OsRng
        .try_fill_bytes(&mut server_nonce)
        .map_err(|_| StartupError::Random)?;
    let issued_unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StartupError::Clock)
        .and_then(|duration| i64::try_from(duration.as_secs()).map_err(|_| StartupError::Clock))?;
    SessionDescriptor::issue(
        PROTOCOL_MAJOR,
        PROTOCOL_MINOR,
        std::process::id(),
        process_nonce,
        server_nonce,
        issued_unix_seconds,
    )
    .map_err(StartupError::Session)
}

pub fn open_wal_file(data_root: &ValidatedDirectory) -> Result<File, StartupError> {
    match data_root.create_new_file(WAL_FILE_NAME) {
        Ok(file) => {
            drop(file);
            rustix::fs::fsync(data_root.as_fd()).map_err(io::Error::from)?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(StartupError::WalIo(error)),
    }
    let file = rustix::fs::openat(
        data_root.as_fd(),
        WAL_FILE_NAME,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    Ok(File::from(file))
}

pub fn open_log_file(log_root: &ValidatedDirectory) -> Result<File, StartupError> {
    match log_root.create_new_file(LOG_FILE_NAME) {
        Ok(file) => {
            rustix::fs::fsync(log_root.as_fd())
                .map_err(io::Error::from)
                .map_err(StartupError::LogIo)?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let file = rustix::fs::openat(
                log_root.as_fd(),
                LOG_FILE_NAME,
                OFlags::WRONLY | OFlags::APPEND | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(io::Error::from)
            .map_err(StartupError::LogIo)?;
            Ok(File::from(file))
        }
        Err(error) => Err(StartupError::LogIo(error)),
    }
}

#[derive(Debug, Error)]
pub enum StartupError {
    #[error("inherited session descriptor read failed")]
    SecretIo(#[source] io::Error),
    #[error("inherited session secret must contain exactly 32 bytes, got {actual}")]
    SecretLength { actual: usize },
    #[error("inherited session secret is invalid")]
    Secret(#[source] AuthError),
    #[error("operating system randomness is unavailable")]
    Random,
    #[error("system clock cannot issue a local session")]
    Clock,
    #[error("public session descriptor is invalid")]
    Session(#[source] SessionError),
    #[error("market WAL open failed")]
    WalIo(#[from] io::Error),
    #[error("local JSON log open failed")]
    LogIo(#[source] io::Error),
}
