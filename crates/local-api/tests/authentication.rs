use local_api::{
    auth::{AuthError, SessionAuthenticator, SessionSecret},
    session::{CANONICAL_SESSION_LENGTH, SessionDescriptor, SessionError, TOKEN_LIFETIME_SECONDS},
};
use zeroize::Zeroizing;

const ISSUED_AT: i64 = 1_700_000_000;
const PROCESS_NONCE: [u8; 16] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
];
const SERVER_NONCE: [u8; 16] = [
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
];
const EXPECTED_TOKEN: [u8; 32] = [
    0xad, 0xf6, 0x79, 0xf3, 0xa0, 0x68, 0x58, 0x98, 0x7c, 0xb0, 0x65, 0x60, 0x21, 0x6b, 0x03, 0xc7,
    0xe2, 0x54, 0xf6, 0x42, 0xb4, 0x2b, 0xe8, 0xb6, 0xb1, 0x5b, 0x8e, 0xde, 0x7d, 0xe8, 0xc4, 0xa8,
];

fn descriptor() -> SessionDescriptor {
    SessionDescriptor::issue(1, 2, 0x0102_0304, PROCESS_NONCE, SERVER_NONCE, ISSUED_AT)
        .expect("fixture descriptor must be valid")
}

fn secret() -> SessionSecret {
    SessionSecret::try_from(Zeroizing::new(vec![0xa5; 32])).expect("fixture secret must be valid")
}

#[test]
fn session_secret_requires_exactly_32_pre_read_zeroizing_bytes() {
    for length in [0, 1, 31, 33, 64] {
        let error = match SessionSecret::try_from(Zeroizing::new(vec![0x5a; length])) {
            Ok(_) => panic!("non-32-byte secrets must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error, AuthError::InvalidSecretLength { actual: length });
        assert_eq!(
            error.to_string(),
            format!("session secret must contain exactly 32 bytes, got {length}")
        );
    }
}

#[test]
fn descriptor_has_the_frozen_canonical_big_endian_encoding() {
    let actual = descriptor().canonical_bytes();
    let mut expected = Vec::with_capacity(CANONICAL_SESSION_LENGTH);
    expected.extend_from_slice(b"cmti:session:v1\0");
    expected.extend_from_slice(&1_u32.to_be_bytes());
    expected.extend_from_slice(&2_u32.to_be_bytes());
    expected.extend_from_slice(&0x0102_0304_u32.to_be_bytes());
    expected.extend_from_slice(&PROCESS_NONCE);
    expected.extend_from_slice(&SERVER_NONCE);
    expected.extend_from_slice(&ISSUED_AT.to_be_bytes());
    expected.extend_from_slice(&(ISSUED_AT + TOKEN_LIFETIME_SECONDS).to_be_bytes());

    assert_eq!(actual.as_slice(), expected.as_slice());
    assert_eq!(actual.len(), 76);
    assert_eq!(
        SessionDescriptor::from_canonical_bytes(&actual).expect("canonical bytes must round trip"),
        descriptor()
    );
}

#[test]
fn malformed_or_short_descriptors_fail_closed() {
    let canonical = descriptor().canonical_bytes();

    assert_eq!(
        SessionDescriptor::from_canonical_bytes(&canonical[..canonical.len() - 1]),
        Err(SessionError::InvalidCanonicalLength { actual: 75 })
    );

    let mut wrong_domain = canonical;
    wrong_domain[0] ^= 1;
    assert_eq!(
        SessionDescriptor::from_canonical_bytes(&wrong_domain),
        Err(SessionError::InvalidCanonicalDomain)
    );

    let mut wrong_lifetime = canonical;
    let expiry_last = wrong_lifetime
        .last_mut()
        .expect("canonical descriptor cannot be empty");
    *expiry_last = expiry_last.wrapping_add(1);
    assert_eq!(
        SessionDescriptor::from_canonical_bytes(&wrong_lifetime),
        Err(SessionError::InvalidLifetime)
    );
}

#[test]
fn hmac_sha256_matches_the_independent_protocol_vector() {
    let authenticator = SessionAuthenticator::new(secret());
    let token = authenticator.token(&descriptor());

    assert_eq!(token.as_bytes(), &EXPECTED_TOKEN);
    authenticator
        .validate_at(&descriptor(), token.as_bytes(), ISSUED_AT)
        .expect("the frozen token must authenticate");
}

#[test]
fn token_mutation_and_wrong_session_fields_are_rejected() {
    let authenticator = SessionAuthenticator::new(secret());
    let expected_descriptor = descriptor();
    let token = authenticator.token(&expected_descriptor);
    let mut mutated = *token.as_bytes();
    mutated[17] ^= 0x80;
    assert_eq!(
        authenticator.validate_at(&expected_descriptor, &mutated, ISSUED_AT),
        Err(AuthError::InvalidToken)
    );

    let mismatches = [
        SessionDescriptor::issue(2, 2, 0x0102_0304, PROCESS_NONCE, SERVER_NONCE, ISSUED_AT),
        SessionDescriptor::issue(1, 3, 0x0102_0304, PROCESS_NONCE, SERVER_NONCE, ISSUED_AT),
        SessionDescriptor::issue(1, 2, 0x0102_0305, PROCESS_NONCE, SERVER_NONCE, ISSUED_AT),
        SessionDescriptor::issue(1, 2, 0x0102_0304, [0x55; 16], SERVER_NONCE, ISSUED_AT),
        SessionDescriptor::issue(1, 2, 0x0102_0304, PROCESS_NONCE, [0xaa; 16], ISSUED_AT),
    ];

    for mismatch in mismatches {
        let mismatch = mismatch.expect("mismatch fixture must remain structurally valid");
        assert_eq!(
            authenticator.validate_at(&mismatch, token.as_bytes(), ISSUED_AT),
            Err(AuthError::InvalidToken)
        );
    }
}

#[test]
fn validation_uses_exact_expiry_boundaries_without_sleeping() {
    let authenticator = SessionAuthenticator::new(secret());
    let descriptor = descriptor();
    let token = authenticator.token(&descriptor);

    assert_eq!(
        authenticator.validate_at(&descriptor, token.as_bytes(), ISSUED_AT - 1),
        Err(AuthError::NotYetValid)
    );
    authenticator
        .validate_at(&descriptor, token.as_bytes(), ISSUED_AT)
        .expect("token must be valid at issuance");
    authenticator
        .validate_at(
            &descriptor,
            token.as_bytes(),
            ISSUED_AT + TOKEN_LIFETIME_SECONDS - 1,
        )
        .expect("token must be valid through the final second");
    assert_eq!(
        authenticator.validate_at(
            &descriptor,
            token.as_bytes(),
            ISSUED_AT + TOKEN_LIFETIME_SECONDS,
        ),
        Err(AuthError::Expired)
    );
}
