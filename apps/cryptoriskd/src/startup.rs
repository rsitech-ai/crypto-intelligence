use std::{
    fs::File,
    io::{self, Read},
    os::fd::FromRawFd,
    time::{SystemTime, UNIX_EPOCH},
};

use config::ValidatedDirectory;
use local_api::{
    auth::{AuthError, SessionSecret},
    session::{SessionDescriptor, SessionError},
};
use rand::{RngCore, rngs::OsRng};
use rustix::{
    fs::{FileType, FlockOperation, Mode, OFlags},
    io::{FdFlags, fcntl_getfd},
};
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
    let raw_fd = i32::try_from(fd).map_err(|_| {
        StartupError::SecretIo(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inherited descriptor is outside the platform range",
        ))
    })?;
    if raw_fd < 3 {
        return Err(StartupError::SecretIo(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inherited descriptor must not alias standard I/O",
        )));
    }
    File::open(format!("/dev/fd/{fd}"))
        .map_err(StartupError::SecretIo)
        .map(drop)?;
    // SAFETY: opening `/dev/fd/{fd}` above proves the inherited descriptor is
    // valid. Startup is its sole owner and this conversion immediately gives
    // the exact descriptor RAII ownership so every success/error path closes it.
    #[allow(unsafe_code)]
    let file = unsafe { File::from_raw_fd(raw_fd) };
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
    let file = open_or_create_state_file(
        data_root,
        WAL_FILE_NAME,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
    )
    .map_err(StartupError::WalIo)?;
    rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive)
        .map_err(io::Error::from)
        .map_err(StartupError::WalIo)?;
    Ok(file)
}

pub fn open_log_file(log_root: &ValidatedDirectory) -> Result<File, StartupError> {
    open_or_create_state_file(
        log_root,
        LOG_FILE_NAME,
        OFlags::WRONLY | OFlags::APPEND | OFlags::CLOEXEC | OFlags::NOFOLLOW,
    )
    .map_err(StartupError::LogIo)
}

fn open_or_create_state_file(
    root: &ValidatedDirectory,
    name: &str,
    access_flags: OFlags,
) -> io::Result<File> {
    let create_flags = access_flags | OFlags::CREATE | OFlags::EXCL;
    let (owned, created) =
        match rustix::fs::openat(root.as_fd(), name, create_flags, Mode::RUSR | Mode::WUSR) {
            Ok(owned) => (owned, true),
            Err(rustix::io::Errno::EXIST) => (
                rustix::fs::openat(root.as_fd(), name, access_flags, Mode::empty())?,
                false,
            ),
            Err(error) => return Err(error.into()),
        };
    let file = File::from(owned);
    validate_state_file(&file)?;
    if created {
        rustix::fs::fsync(root.as_fd()).map_err(io::Error::from)?;
    }
    Ok(file)
}

fn validate_state_file(file: &File) -> io::Result<()> {
    let stat = rustix::fs::fstat(file).map_err(io::Error::from)?;
    let safe = FileType::from_raw_mode(stat.st_mode).is_file()
        && stat.st_uid == rustix::process::geteuid().as_raw()
        && stat.st_mode & 0o077 == 0
        && stat.st_nlink == 1
        && fcntl_getfd(file)
            .map_err(io::Error::from)?
            .contains(FdFlags::CLOEXEC);
    if !safe {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "state file metadata violates the local security contract",
        ));
    }
    Ok(())
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
