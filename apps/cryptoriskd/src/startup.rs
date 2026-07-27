use std::{
    fs::File,
    io::{self, Read},
    os::fd::{BorrowedFd, FromRawFd},
    time::{SystemTime, UNIX_EPOCH},
};

use config::ValidatedDirectory;
use local_api::{
    auth::{AuthError, SessionSecret},
    session::{SessionDescriptor, SessionError},
};
use rand::{RngCore, rngs::OsRng};
use rustix::{
    fs::{FileType, FlockOperation, Mode, OFlags, fcntl_getfl, fcntl_setfl},
    io::{FdFlags, fcntl_getfd},
};
use thiserror::Error;
use zeroize::Zeroizing;

const SESSION_SECRET_LENGTH: usize = 32;
const PROTOCOL_MAJOR: u32 = 1;
const PROTOCOL_MINOR: u32 = 0;
const WAL_FILE_NAME: &str = "market.wal";
const LOG_FILE_NAME: &str = "cmti.jsonl";

pub(crate) fn parse_session_secret(
    bytes: Zeroizing<Vec<u8>>,
) -> Result<SessionSecret, StartupError> {
    if bytes.len() != SESSION_SECRET_LENGTH {
        return Err(StartupError::SecretLength {
            actual: bytes.len(),
        });
    }
    SessionSecret::try_from(bytes).map_err(StartupError::Secret)
}

pub async fn read_session_secret_from_fd_async(fd: u32) -> Result<SessionSecret, StartupError> {
    let file = take_inherited_fd(fd).map_err(StartupError::SecretIo)?;
    set_nonblocking(&file).map_err(StartupError::SecretIo)?;
    let file = tokio::io::unix::AsyncFd::new(file).map_err(StartupError::SecretIo)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(SESSION_SECRET_LENGTH + 1));
    while bytes.len() <= SESSION_SECRET_LENGTH {
        let mut ready = file.readable().await.map_err(StartupError::SecretIo)?;
        let mut buffer = [0_u8; SESSION_SECRET_LENGTH + 1];
        let remaining = buffer.len().min(SESSION_SECRET_LENGTH + 1 - bytes.len());
        match ready.try_io(|inner| {
            let mut reader = inner.get_ref();
            reader.read(&mut buffer[..remaining])
        }) {
            Ok(Ok(0)) => break,
            Ok(Ok(read)) => bytes.extend_from_slice(&buffer[..read]),
            Ok(Err(error)) => return Err(StartupError::SecretIo(error)),
            Err(_) => {}
        }
    }
    parse_session_secret(bytes)
}

#[cfg(debug_assertions)]
pub async fn wait_readiness_gate_from_fd(fd: u32) -> io::Result<()> {
    let file = take_inherited_fd(fd)?;
    set_nonblocking(&file)?;
    let file = tokio::io::unix::AsyncFd::new(file)?;
    loop {
        let mut ready = file.readable().await?;
        let mut release = [0_u8; 1];
        match ready.try_io(|inner| {
            let mut reader = inner.get_ref();
            reader.read(&mut release)
        }) {
            Ok(Ok(1)) => return Ok(()),
            Ok(Ok(0)) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "readiness gate closed before release",
                ));
            }
            Ok(Ok(_)) => unreachable!("one-byte readiness gate read is bounded"),
            Ok(Err(error)) => return Err(error),
            Err(_) => {}
        }
    }
}

fn take_inherited_fd(fd: u32) -> io::Result<File> {
    let raw_fd = i32::try_from(fd).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "inherited descriptor is outside the platform range",
        )
    })?;
    if raw_fd < 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inherited descriptor must not alias standard I/O",
        ));
    }
    // SAFETY: borrowing does not transfer ownership. `fcntl_getfd` validates
    // the inherited integer before the sole-owner conversion below.
    #[allow(unsafe_code)]
    let borrowed = unsafe { BorrowedFd::borrow_raw(raw_fd) };
    fcntl_getfd(borrowed).map_err(io::Error::from)?;
    // SAFETY: `fcntl_getfd` above proves the descriptor is valid. Startup is
    // its sole owner and this conversion immediately gives the exact
    // descriptor RAII ownership so every success/error path closes it.
    #[allow(unsafe_code)]
    let file = unsafe { File::from_raw_fd(raw_fd) };
    Ok(file)
}

fn set_nonblocking(file: &File) -> io::Result<()> {
    let flags = fcntl_getfl(file).map_err(io::Error::from)?;
    fcntl_setfl(file, flags | OFlags::NONBLOCK).map_err(io::Error::from)
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
                rustix::fs::openat(
                    root.as_fd(),
                    name,
                    access_flags | OFlags::NONBLOCK,
                    Mode::empty(),
                )?,
                false,
            ),
            Err(error) => return Err(error.into()),
        };
    let file = File::from(owned);
    validate_state_file(&file)?;
    if !created {
        let flags = fcntl_getfl(&file).map_err(io::Error::from)?;
        fcntl_setfl(&file, flags & !OFlags::NONBLOCK).map_err(io::Error::from)?;
    }
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
