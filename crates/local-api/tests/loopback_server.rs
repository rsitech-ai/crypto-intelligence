use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

use domain::{InstrumentId, SourceId, SourceKind, UnixNanos, VenueId};
use fixed_decimal::{FixedDecimal, Price};
use local_api::{
    auth::{
        Clock, SessionAuthenticator, SessionSecret, TOKEN_METADATA_KEY,
        insert_authentication_metadata,
    },
    proto::{
        health_v1::{CheckRequest, ServingStatus, health_service_client::HealthServiceClient},
        market_v1::{
            GetSnapshotRequest, SnapshotHealth, market_service_client::MarketServiceClient,
        },
    },
    server::{LoopbackServer, MarketSnapshot, ServerError},
    session::{SessionDescriptor, TOKEN_LIFETIME_SECONDS},
};
use prost::Message;
use tonic::{Code, Request, transport::Endpoint};
use zeroize::Zeroizing;

const ISSUED_AT: i64 = 1_700_000_000;
const SECRET_BYTES: [u8; 32] = [0x6b; 32];

#[derive(Default)]
struct TestClock {
    now: AtomicI64,
}

impl TestClock {
    fn at(now: i64) -> Self {
        Self {
            now: AtomicI64::new(now),
        }
    }

    fn set(&self, now: i64) {
        self.now.store(now, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn unix_seconds(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}

fn descriptor() -> SessionDescriptor {
    SessionDescriptor::issue(1, 0, 4_242, [0x11; 16], [0x22; 16], ISSUED_AT)
        .expect("fixture descriptor must be valid")
}

fn secret() -> SessionSecret {
    SessionSecret::try_from(Zeroizing::new(SECRET_BYTES.to_vec()))
        .expect("fixture secret must be valid")
}

fn snapshot() -> MarketSnapshot {
    let source =
        SourceId::new(SourceKind::Exchange, "binance", 7).expect("fixture source must be valid");
    let instrument = InstrumentId::new(
        VenueId::new("binance").expect("fixture venue must be valid"),
        "btcusdt",
        7,
    )
    .expect("fixture instrument must be valid");
    let best_bid = Price::new(
        FixedDecimal::parse_canonical("67234.1").expect("fixture bid must be canonical"),
    )
    .expect("fixture bid must be positive");
    let best_ask = Price::new(
        FixedDecimal::parse_canonical("67234.11").expect("fixture ask must be canonical"),
    )
    .expect("fixture ask must be positive");

    MarketSnapshot::new(
        source,
        instrument,
        9_001,
        best_bid,
        best_ask,
        SnapshotHealth::Healthy,
        UnixNanos::new(1_700_000_000_100_000_000),
        UnixNanos::new(1_700_000_000_125_000_000),
        25,
    )
    .expect("fixture snapshot must be valid")
}

async fn connect(
    address: SocketAddr,
) -> (
    HealthServiceClient<tonic::transport::Channel>,
    MarketServiceClient<tonic::transport::Channel>,
) {
    let channel = Endpoint::from_shared(format!("http://{address}"))
        .expect("loopback endpoint must be valid")
        .connect()
        .await
        .expect("server must accept a local connection");
    (
        HealthServiceClient::new(channel.clone()),
        MarketServiceClient::new(channel),
    )
}

#[tokio::test]
async fn rejects_every_bind_except_exact_ephemeral_ipv4_loopback() {
    for address in [
        "0.0.0.0:0".parse().expect("fixture address must parse"),
        "127.0.0.2:0".parse().expect("fixture address must parse"),
        "127.0.0.1:9000"
            .parse()
            .expect("fixture address must parse"),
        "[::]:0".parse().expect("fixture address must parse"),
        "[::1]:0".parse().expect("fixture address must parse"),
    ] {
        let result = LoopbackServer::spawn(
            address,
            secret(),
            descriptor(),
            snapshot(),
            Arc::new(TestClock::at(ISSUED_AT)),
        )
        .await;
        assert!(
            matches!(result, Err(ServerError::NonLoopbackBind)),
            "unexpected acceptance for {address}"
        );
    }
}

#[tokio::test]
async fn liveness_is_unauthenticated_and_wire_minimal() {
    let server = LoopbackServer::spawn(
        "127.0.0.1:0".parse().expect("bind address must parse"),
        secret(),
        descriptor(),
        snapshot(),
        Arc::new(TestClock::at(ISSUED_AT)),
    )
    .await
    .expect("exact loopback bind must start");
    let (mut health, mut market) = connect(server.local_addr()).await;

    let response = health
        .check(CheckRequest {})
        .await
        .expect("liveness must not require authentication")
        .into_inner();
    assert_eq!(response.status, ServingStatus::Serving as i32);
    let protocol = response.protocol.expect("protocol version must be present");
    assert_eq!((protocol.major, protocol.minor), (1, 0));
    assert_eq!(
        local_api::proto::health_v1::CheckResponse {
            status: response.status,
            protocol: Some(protocol),
        }
        .encode_to_vec(),
        [0x08, 0x01, 0x12, 0x02, 0x08, 0x01]
    );

    let unauthenticated = market
        .get_snapshot(GetSnapshotRequest {})
        .await
        .expect_err("snapshot must reject missing metadata");
    assert_eq!(unauthenticated.code(), Code::Unauthenticated);
    assert_eq!(unauthenticated.message(), "authentication failed");

    server
        .shutdown()
        .await
        .expect("server must shut down cleanly");
}

#[tokio::test]
async fn authenticated_snapshot_returns_only_canonical_authoritative_fields() {
    let session = descriptor();
    let client_authenticator = SessionAuthenticator::new(secret());
    let token = client_authenticator.token(&session);
    let server = LoopbackServer::spawn(
        "127.0.0.1:0".parse().expect("bind address must parse"),
        secret(),
        session.clone(),
        snapshot(),
        Arc::new(TestClock::at(ISSUED_AT)),
    )
    .await
    .expect("exact loopback bind must start");
    let (_, mut market) = connect(server.local_addr()).await;
    let request =
        insert_authentication_metadata(Request::new(GetSnapshotRequest {}), &session, &token);

    let response = market
        .get_snapshot(request)
        .await
        .expect("valid session metadata must authenticate")
        .into_inner();
    assert_eq!(response.source, "binance");
    assert_eq!(response.symbol, "BTCUSDT");
    assert_eq!(response.generation, 7);
    assert_eq!(response.sequence, 9_001);
    assert_eq!(response.best_bid, "67234.1");
    assert_eq!(response.best_ask, "67234.11");
    assert_eq!(response.health, SnapshotHealth::Healthy as i32);
    assert_eq!(response.event_unix_nanos, 1_700_000_000_100_000_000);
    assert_eq!(response.receive_unix_nanos, 1_700_000_000_125_000_000);
    assert_eq!(response.freshness_millis, 25);

    server
        .shutdown()
        .await
        .expect("server must shut down cleanly");
}

#[tokio::test]
async fn missing_mutated_and_replayed_credentials_fail_closed() {
    let clock = Arc::new(TestClock::at(ISSUED_AT));
    let session = descriptor();
    let client_authenticator = SessionAuthenticator::new(secret());
    let token = client_authenticator.token(&session);
    let server = LoopbackServer::spawn(
        "127.0.0.1:0".parse().expect("bind address must parse"),
        secret(),
        session.clone(),
        snapshot(),
        clock.clone(),
    )
    .await
    .expect("exact loopback bind must start");
    let (_, mut market) = connect(server.local_addr()).await;

    let mut missing_token =
        insert_authentication_metadata(Request::new(GetSnapshotRequest {}), &session, &token);
    missing_token.metadata_mut().remove_bin(TOKEN_METADATA_KEY);
    assert_eq!(
        market
            .get_snapshot(missing_token)
            .await
            .expect_err("missing token must fail")
            .code(),
        Code::Unauthenticated
    );

    let mut mutated = *token.as_bytes();
    mutated[0] ^= 1;
    let mutated = local_api::auth::AuthenticationToken::from_bytes(mutated);
    let mutated_request =
        insert_authentication_metadata(Request::new(GetSnapshotRequest {}), &session, &mutated);
    assert_eq!(
        market
            .get_snapshot(mutated_request)
            .await
            .expect_err("mutated token must fail")
            .code(),
        Code::Unauthenticated
    );

    let valid_request =
        insert_authentication_metadata(Request::new(GetSnapshotRequest {}), &session, &token);
    market
        .get_snapshot(valid_request)
        .await
        .expect("token must work before expiry");

    clock.set(ISSUED_AT + TOKEN_LIFETIME_SECONDS);
    let replay =
        insert_authentication_metadata(Request::new(GetSnapshotRequest {}), &session, &token);
    assert_eq!(
        market
            .get_snapshot(replay)
            .await
            .expect_err("replay at expiry must fail")
            .code(),
        Code::Unauthenticated
    );

    server
        .shutdown()
        .await
        .expect("server must shut down cleanly");
}
