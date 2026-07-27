use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use domain::{InstrumentId, SourceId, SourceKind, UnixNanos, VenueId};
use fixed_decimal::{FixedDecimal, Price};
use local_api::{
    auth::{
        SessionAuthenticator, SessionSecret, TOKEN_METADATA_KEY, insert_authentication_metadata,
    },
    proto::{
        health_v1::{
            CheckRequest, CheckResponse, ServingStatus, health_service_client::HealthServiceClient,
        },
        market_v1::{
            GetSnapshotRequest, SnapshotHealth, market_service_client::MarketServiceClient,
        },
    },
    server::{LoopbackServer, MarketSnapshot, ServerError},
    session::{SessionDescriptor, TOKEN_LIFETIME_SECONDS},
};
use prost::Message;
use tokio::net::TcpStream;
use tonic::{
    Code, Request,
    client::Grpc,
    codegen::http::uri::PathAndQuery,
    transport::{Channel, Endpoint},
};
use zeroize::Zeroizing;

const SECRET_BYTES: [u8; 32] = [0x6b; 32];

#[derive(Clone, PartialEq, Message)]
struct OversizedRequest {
    #[prost(bytes = "vec", tag = "1")]
    padding: Vec<u8>,
}

fn current_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("fixture clock must be after the Unix epoch")
        .as_secs()
        .try_into()
        .expect("fixture time must fit i64")
}

fn descriptor_at(issued_unix_seconds: i64) -> SessionDescriptor {
    SessionDescriptor::issue(1, 0, 4_242, [0x11; 16], [0x22; 16], issued_unix_seconds)
        .expect("fixture descriptor must be valid for sixty seconds")
}

fn descriptor() -> SessionDescriptor {
    descriptor_at(current_unix_seconds())
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

async fn channel(address: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{address}"))
        .expect("loopback endpoint must be valid")
        .connect()
        .await
        .expect("server must accept a local connection")
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
        let result = LoopbackServer::spawn(address, secret(), descriptor(), snapshot()).await;
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
async fn missing_and_mutated_credentials_fail_closed() {
    let session = descriptor();
    let client_authenticator = SessionAuthenticator::new(secret());
    let token = client_authenticator.token(&session);
    let server = LoopbackServer::spawn(
        "127.0.0.1:0".parse().expect("bind address must parse"),
        secret(),
        session.clone(),
        snapshot(),
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

    server
        .shutdown()
        .await
        .expect("server must shut down cleanly");
}

#[tokio::test]
async fn public_server_uses_system_time_to_reject_an_already_expired_session() {
    let session = descriptor_at(current_unix_seconds() - TOKEN_LIFETIME_SECONDS - 1);
    let client_authenticator = SessionAuthenticator::new(secret());
    let token = client_authenticator.token(&session);
    let server = LoopbackServer::spawn(
        "127.0.0.1:0".parse().expect("bind address must parse"),
        secret(),
        session.clone(),
        snapshot(),
    )
    .await
    .expect("exact loopback bind must start");
    let (_, mut market) = connect(server.local_addr()).await;

    let status = market
        .get_snapshot(insert_authentication_metadata(
            Request::new(GetSnapshotRequest {}),
            &session,
            &token,
        ))
        .await
        .expect_err("production server must reject an already expired session");
    assert_eq!(status.code(), Code::Unauthenticated);
    assert_eq!(status.message(), "authentication failed");

    server
        .shutdown()
        .await
        .expect("server must shut down cleanly");
}

#[tokio::test]
async fn oversized_empty_method_payload_is_rejected_before_service_logic() {
    let server = LoopbackServer::spawn(
        "127.0.0.1:0".parse().expect("bind address must parse"),
        secret(),
        descriptor(),
        snapshot(),
    )
    .await
    .expect("exact loopback bind must start");
    let mut grpc = Grpc::new(channel(server.local_addr()).await);
    grpc.ready()
        .await
        .expect("local RPC client must become ready");

    let result: Result<tonic::Response<CheckResponse>, tonic::Status> = grpc
        .unary(
            Request::new(OversizedRequest {
                padding: vec![0_u8; 1_024],
            }),
            PathAndQuery::from_static("/cmti.health.v1.HealthService/Check"),
            tonic_prost::ProstCodec::default(),
        )
        .await;
    assert_eq!(
        result
            .expect_err("oversized payload must fail before health logic")
            .code(),
        Code::OutOfRange
    );

    server
        .shutdown()
        .await
        .expect("server must shut down cleanly");
}

#[tokio::test]
async fn shutdown_forces_a_stuck_connection_closed_within_a_fixed_bound() {
    let server = LoopbackServer::spawn(
        "127.0.0.1:0".parse().expect("bind address must parse"),
        secret(),
        descriptor(),
        snapshot(),
    )
    .await
    .expect("exact loopback bind must start");
    let _stuck_connection = TcpStream::connect(server.local_addr())
        .await
        .expect("raw loopback connection must open");
    tokio::task::yield_now().await;

    tokio::time::timeout(Duration::from_secs(2), server.shutdown())
        .await
        .expect("shutdown must have a fixed internal deadline")
        .expect("forced cleanup must complete successfully");
}
