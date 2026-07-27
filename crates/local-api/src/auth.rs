//! HMAC-SHA256 authentication for versioned local sessions.

use std::sync::Arc;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;
use tonic::{
    Request,
    metadata::{BinaryMetadataValue, MetadataValue},
};
use zeroize::Zeroizing;

use crate::session::SessionDescriptor;

/// Binary gRPC metadata carrying the canonical public session descriptor.
pub const SESSION_DESCRIPTOR_METADATA_KEY: &str = "cmti-session-bin";
/// Binary gRPC metadata carrying the secret-derived authentication token.
pub const TOKEN_METADATA_KEY: &str = "cmti-token-bin";

const SESSION_SECRET_LENGTH: usize = 32;

type HmacSha256 = Hmac<Sha256>;

/// Time source used for authentication boundary checks.
///
/// Runtime services receive this dependency explicitly. Tests can therefore
/// exercise issuance and expiry without sleeping or changing process time.
pub trait Clock: Send + Sync + 'static {
    fn unix_seconds(&self) -> i64;
}

/// A pre-read, exact-length session secret that zeroizes on final drop.
///
/// This type deliberately implements neither `Debug` nor `Display`.
pub struct SessionSecret(Arc<Zeroizing<[u8; SESSION_SECRET_LENGTH]>>);

impl TryFrom<Zeroizing<Vec<u8>>> for SessionSecret {
    type Error = AuthError;

    fn try_from(bytes: Zeroizing<Vec<u8>>) -> Result<Self, Self::Error> {
        let actual = bytes.len();
        let array = <[u8; SESSION_SECRET_LENGTH]>::try_from(bytes.as_slice())
            .map_err(|_| AuthError::InvalidSecretLength { actual })?;
        Ok(Self(Arc::new(Zeroizing::new(array))))
    }
}

/// A fixed-width authentication tag.
///
/// This type deliberately implements neither `Debug` nor `Display`.
#[derive(Clone)]
pub struct AuthenticationToken(Zeroizing<[u8; 32]>);

impl AuthenticationToken {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Authentication failures. No variant contains secret or token material.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AuthError {
    #[error("session secret must contain exactly 32 bytes, got {actual}")]
    InvalidSecretLength { actual: usize },
    #[error("session token is not yet valid")]
    NotYetValid,
    #[error("session token has expired")]
    Expired,
    #[error("invalid session token")]
    InvalidToken,
}

/// Issues and verifies tokens for one zeroizing session secret.
///
/// This type deliberately implements neither `Debug` nor `Display`.
pub struct SessionAuthenticator {
    secret: SessionSecret,
}

impl SessionAuthenticator {
    pub const fn new(secret: SessionSecret) -> Self {
        Self { secret }
    }

    pub fn token(&self, descriptor: &SessionDescriptor) -> AuthenticationToken {
        let mut mac = HmacSha256::new_from_slice(self.secret.0.as_ref().as_slice())
            .expect("a 32-byte HMAC key is always valid");
        mac.update(&descriptor.canonical_bytes());
        AuthenticationToken::from_bytes(mac.finalize().into_bytes().into())
    }

    /// Validates the full SHA-256 tag through RustCrypto's constant-time
    /// `verify_slice` API and then applies exact issuance/expiry boundaries.
    pub fn validate_at(
        &self,
        descriptor: &SessionDescriptor,
        presented_token: &[u8],
        now_unix_seconds: i64,
    ) -> Result<(), AuthError> {
        let mut mac = HmacSha256::new_from_slice(self.secret.0.as_ref().as_slice())
            .expect("a 32-byte HMAC key is always valid");
        mac.update(&descriptor.canonical_bytes());
        let token_result = mac
            .verify_slice(presented_token)
            .map_err(|_| AuthError::InvalidToken);

        if now_unix_seconds < descriptor.issued_unix_seconds() {
            return Err(AuthError::NotYetValid);
        }
        if now_unix_seconds >= descriptor.expiry_unix_seconds() {
            return Err(AuthError::Expired);
        }
        token_result
    }
}

/// Adds canonical descriptor and sensitive binary token metadata to a request.
pub fn insert_authentication_metadata<T>(
    mut request: Request<T>,
    descriptor: &SessionDescriptor,
    token: &AuthenticationToken,
) -> Request<T> {
    let descriptor_metadata = MetadataValue::from_bytes(descriptor.canonical_bytes().as_slice());
    let mut token_metadata = BinaryMetadataValue::from_bytes(token.as_bytes());
    token_metadata.set_sensitive(true);
    request
        .metadata_mut()
        .insert_bin(SESSION_DESCRIPTOR_METADATA_KEY, descriptor_metadata);
    request
        .metadata_mut()
        .insert_bin(TOKEN_METADATA_KEY, token_metadata);
    request
}
