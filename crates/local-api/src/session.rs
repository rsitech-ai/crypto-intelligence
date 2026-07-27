//! Public, canonical description of an authenticated local session.

use thiserror::Error;

/// Domain separation prefix for the first local session protocol.
pub const SESSION_DOMAIN: &[u8; 16] = b"cmti:session:v1\0";
/// Exact valid lifetime of every local session token.
pub const TOKEN_LIFETIME_SECONDS: i64 = 60;
/// Exact length of [`SessionDescriptor::canonical_bytes`].
pub const CANONICAL_SESSION_LENGTH: usize = 76;

const PROTOCOL_MAJOR_OFFSET: usize = SESSION_DOMAIN.len();
const PROTOCOL_MINOR_OFFSET: usize = PROTOCOL_MAJOR_OFFSET + 4;
const DAEMON_PID_OFFSET: usize = PROTOCOL_MINOR_OFFSET + 4;
const PROCESS_NONCE_OFFSET: usize = DAEMON_PID_OFFSET + 4;
const SERVER_NONCE_OFFSET: usize = PROCESS_NONCE_OFFSET + 16;
const ISSUED_SECONDS_OFFSET: usize = SERVER_NONCE_OFFSET + 16;
const EXPIRY_SECONDS_OFFSET: usize = ISSUED_SECONDS_OFFSET + 8;

/// Public fields that bind an authentication token to one daemon session.
///
/// The canonical form never contains a secret or authentication token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDescriptor {
    protocol_major: u32,
    protocol_minor: u32,
    daemon_pid: u32,
    process_nonce: [u8; 16],
    server_nonce: [u8; 16],
    issued_unix_seconds: i64,
    expiry_unix_seconds: i64,
}

/// Structural session descriptor failures.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SessionError {
    #[error("protocol major must be nonzero")]
    InvalidProtocolMajor,
    #[error("daemon PID must be nonzero")]
    InvalidDaemonPid,
    #[error("session issue time cannot represent the exact token lifetime")]
    LifetimeOverflow,
    #[error("session token lifetime must be exactly 60 seconds")]
    InvalidLifetime,
    #[error(
        "canonical session descriptor must contain exactly {CANONICAL_SESSION_LENGTH} bytes, got {actual}"
    )]
    InvalidCanonicalLength { actual: usize },
    #[error("canonical session descriptor has the wrong domain")]
    InvalidCanonicalDomain,
}

impl SessionDescriptor {
    /// Issues a descriptor whose expiry is exactly 60 seconds after issuance.
    pub fn issue(
        protocol_major: u32,
        protocol_minor: u32,
        daemon_pid: u32,
        process_nonce: [u8; 16],
        server_nonce: [u8; 16],
        issued_unix_seconds: i64,
    ) -> Result<Self, SessionError> {
        let expiry_unix_seconds = issued_unix_seconds
            .checked_add(TOKEN_LIFETIME_SECONDS)
            .ok_or(SessionError::LifetimeOverflow)?;
        Self::new(
            protocol_major,
            protocol_minor,
            daemon_pid,
            process_nonce,
            server_nonce,
            issued_unix_seconds,
            expiry_unix_seconds,
        )
    }

    fn new(
        protocol_major: u32,
        protocol_minor: u32,
        daemon_pid: u32,
        process_nonce: [u8; 16],
        server_nonce: [u8; 16],
        issued_unix_seconds: i64,
        expiry_unix_seconds: i64,
    ) -> Result<Self, SessionError> {
        if protocol_major == 0 {
            return Err(SessionError::InvalidProtocolMajor);
        }
        if daemon_pid == 0 {
            return Err(SessionError::InvalidDaemonPid);
        }
        if expiry_unix_seconds.checked_sub(issued_unix_seconds) != Some(TOKEN_LIFETIME_SECONDS) {
            return Err(SessionError::InvalidLifetime);
        }
        Ok(Self {
            protocol_major,
            protocol_minor,
            daemon_pid,
            process_nonce,
            server_nonce,
            issued_unix_seconds,
            expiry_unix_seconds,
        })
    }

    /// Serializes fields in the binding protocol order and big-endian format.
    pub fn canonical_bytes(&self) -> [u8; CANONICAL_SESSION_LENGTH] {
        let mut bytes = [0_u8; CANONICAL_SESSION_LENGTH];
        bytes[..SESSION_DOMAIN.len()].copy_from_slice(SESSION_DOMAIN);
        bytes[PROTOCOL_MAJOR_OFFSET..PROTOCOL_MINOR_OFFSET]
            .copy_from_slice(&self.protocol_major.to_be_bytes());
        bytes[PROTOCOL_MINOR_OFFSET..DAEMON_PID_OFFSET]
            .copy_from_slice(&self.protocol_minor.to_be_bytes());
        bytes[DAEMON_PID_OFFSET..PROCESS_NONCE_OFFSET]
            .copy_from_slice(&self.daemon_pid.to_be_bytes());
        bytes[PROCESS_NONCE_OFFSET..SERVER_NONCE_OFFSET].copy_from_slice(&self.process_nonce);
        bytes[SERVER_NONCE_OFFSET..ISSUED_SECONDS_OFFSET].copy_from_slice(&self.server_nonce);
        bytes[ISSUED_SECONDS_OFFSET..EXPIRY_SECONDS_OFFSET]
            .copy_from_slice(&self.issued_unix_seconds.to_be_bytes());
        bytes[EXPIRY_SECONDS_OFFSET..].copy_from_slice(&self.expiry_unix_seconds.to_be_bytes());
        bytes
    }

    /// Parses and validates the exact canonical descriptor format.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, SessionError> {
        let bytes: &[u8; CANONICAL_SESSION_LENGTH] =
            bytes
                .try_into()
                .map_err(|_| SessionError::InvalidCanonicalLength {
                    actual: bytes.len(),
                })?;
        if &bytes[..SESSION_DOMAIN.len()] != SESSION_DOMAIN {
            return Err(SessionError::InvalidCanonicalDomain);
        }

        Self::new(
            read_u32(bytes, PROTOCOL_MAJOR_OFFSET),
            read_u32(bytes, PROTOCOL_MINOR_OFFSET),
            read_u32(bytes, DAEMON_PID_OFFSET),
            read_array(bytes, PROCESS_NONCE_OFFSET),
            read_array(bytes, SERVER_NONCE_OFFSET),
            read_i64(bytes, ISSUED_SECONDS_OFFSET),
            read_i64(bytes, EXPIRY_SECONDS_OFFSET),
        )
    }

    pub const fn protocol_major(&self) -> u32 {
        self.protocol_major
    }

    pub const fn protocol_minor(&self) -> u32 {
        self.protocol_minor
    }

    pub const fn daemon_pid(&self) -> u32 {
        self.daemon_pid
    }

    pub const fn process_nonce(&self) -> &[u8; 16] {
        &self.process_nonce
    }

    pub const fn server_nonce(&self) -> &[u8; 16] {
        &self.server_nonce
    }

    pub const fn issued_unix_seconds(&self) -> i64 {
        self.issued_unix_seconds
    }

    pub const fn expiry_unix_seconds(&self) -> i64 {
        self.expiry_unix_seconds
    }
}

fn read_u32(bytes: &[u8; CANONICAL_SESSION_LENGTH], offset: usize) -> u32 {
    u32::from_be_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated descriptor offsets are in bounds"),
    )
}

fn read_i64(bytes: &[u8; CANONICAL_SESSION_LENGTH], offset: usize) -> i64 {
    i64::from_be_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("validated descriptor offsets are in bounds"),
    )
}

fn read_array<const N: usize>(bytes: &[u8; CANONICAL_SESSION_LENGTH], offset: usize) -> [u8; N] {
    bytes[offset..offset + N]
        .try_into()
        .expect("validated descriptor offsets are in bounds")
}
