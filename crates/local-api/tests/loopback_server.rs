use std::{
    net::SocketAddr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use domain::{
    AssetId as DomainAssetId, AssetNamespace as DomainAssetNamespace, InstrumentId, SourceId,
    SourceKind, UnixNanos, VenueId,
};
use fixed_decimal::{FixedDecimal, Price};
use local_api::{
    auth::{
        SessionAuthenticator, SessionSecret, TOKEN_METADATA_KEY, insert_authentication_metadata,
    },
    proto::{
        common_v1::{
            AssetId, AssetNamespace, StreamDeliveryPolicy, StreamFrameKind, StreamTerminalStatus,
        },
        health_v1::{CheckRequest, CheckResponse, ServingStatus},
        market_v1::{
            GetOrderBookSnapshotRequest, GetOrderBookSnapshotResponse, ProductType, SnapshotHealth,
            SubscribeAssetStateRequest, SubscribeAssetStateResponse, SubscribeVenueStateRequest,
            SubscribeVenueStateResponse,
        },
    },
    server::{LoopbackServer, MarketSnapshot, ServerError, ServerLimits},
    session::{SessionDescriptor, TOKEN_LIFETIME_SECONDS},
};
use prost::Message;
use tokio::net::TcpStream;
use tonic::{
    Code, IntoRequest, Request, Response, Status,
    client::Grpc,
    codegen::http::uri::PathAndQuery,
    transport::{Channel, Endpoint},
};
use zeroize::Zeroizing;

const SECRET_BYTES: [u8; 32] = [0x6b; 32];

#[test]
fn server_limits_reject_zero_and_unrepresentable_values() {
    for result in [
        ServerLimits::new(0, 1, Duration::from_secs(1), Duration::from_secs(1)),
        ServerLimits::new(1, 0, Duration::from_secs(1), Duration::from_secs(1)),
        ServerLimits::new(1, 1, Duration::ZERO, Duration::from_secs(1)),
        ServerLimits::new(1, 1, Duration::from_secs(1), Duration::ZERO),
    ] {
        assert!(matches!(result, Err(ServerError::InvalidLimits)));
    }
}

#[derive(Clone, PartialEq, Message)]
struct OversizedRequest {
    #[prost(bytes = "vec", tag = "1")]
    padding: Vec<u8>,
}

struct HealthTestClient {
    inner: Grpc<Channel>,
}

impl HealthTestClient {
    async fn check(
        &mut self,
        request: impl IntoRequest<CheckRequest>,
    ) -> Result<Response<CheckResponse>, Status> {
        self.inner
            .ready()
            .await
            .map_err(|_| Status::unavailable("loopback transport unavailable"))?;
        self.inner
            .unary(
                request.into_request(),
                PathAndQuery::from_static("/cmti.health.v1.HealthService/Check"),
                tonic_prost::ProstCodec::default(),
            )
            .await
    }
}

struct MarketTestClient {
    inner: Grpc<Channel>,
}

impl MarketTestClient {
    async fn get_order_book_snapshot(
        &mut self,
        request: impl IntoRequest<GetOrderBookSnapshotRequest>,
    ) -> Result<Response<GetOrderBookSnapshotResponse>, Status> {
        self.inner
            .ready()
            .await
            .map_err(|_| Status::unavailable("loopback transport unavailable"))?;
        self.inner
            .unary(
                request.into_request(),
                PathAndQuery::from_static(
                    "/cmti.market.v1.MarketStateService/GetOrderBookSnapshot",
                ),
                tonic_prost::ProstCodec::default(),
            )
            .await
    }

    async fn subscribe_asset_state(
        &mut self,
        request: impl IntoRequest<SubscribeAssetStateRequest>,
    ) -> Result<Response<tonic::codec::Streaming<SubscribeAssetStateResponse>>, Status> {
        self.inner
            .ready()
            .await
            .map_err(|_| Status::unavailable("loopback transport unavailable"))?;
        self.inner
            .server_streaming(
                request.into_request(),
                PathAndQuery::from_static("/cmti.market.v1.MarketStateService/SubscribeAssetState"),
                tonic_prost::ProstCodec::default(),
            )
            .await
    }

    async fn subscribe_venue_state(
        &mut self,
        request: impl IntoRequest<SubscribeVenueStateRequest>,
    ) -> Result<Response<tonic::codec::Streaming<SubscribeVenueStateResponse>>, Status> {
        self.inner
            .ready()
            .await
            .map_err(|_| Status::unavailable("loopback transport unavailable"))?;
        self.inner
            .server_streaming(
                request.into_request(),
                PathAndQuery::from_static("/cmti.market.v1.MarketStateService/SubscribeVenueState"),
                tonic_prost::ProstCodec::default(),
            )
            .await
    }
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
        DomainAssetId::new(DomainAssetNamespace::Native, "bitcoin", "", "BTC", 7)
            .expect("fixture asset must be valid"),
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

fn requested_asset() -> AssetId {
    AssetId {
        namespace: AssetNamespace::Native as i32,
        chain_id: "bitcoin".to_owned(),
        contract_or_mint: String::new(),
        canonical_symbol: "BTC".to_owned(),
        generation: 7,
    }
}

async fn connect(address: SocketAddr) -> (HealthTestClient, MarketTestClient) {
    let channel = Endpoint::from_shared(format!("http://{address}"))
        .expect("loopback endpoint must be valid")
        .connect()
        .await
        .expect("server must accept a local connection");
    (
        HealthTestClient {
            inner: Grpc::new(channel.clone()),
        },
        MarketTestClient {
            inner: Grpc::new(channel),
        },
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
        .get_order_book_snapshot(GetOrderBookSnapshotRequest {})
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
    let request = insert_authentication_metadata(
        Request::new(GetOrderBookSnapshotRequest {}),
        &session,
        &token,
    );

    let response = market
        .get_order_book_snapshot(request)
        .await
        .expect("valid session metadata must authenticate")
        .into_inner();
    assert_eq!(response.source, "binance");
    assert_eq!(response.symbol, "BTCUSDT");
    assert_eq!(response.generation, 7);
    assert_eq!(response.product_type, ProductType::Spot as i32);
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
async fn authenticated_asset_stream_falls_back_to_snapshot_and_terminates_explicitly() {
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
    let asset = requested_asset();
    let request = insert_authentication_metadata(
        Request::new(SubscribeAssetStateRequest {
            assets: vec![asset.clone()],
            resume_token: "not-retained".to_owned(),
        }),
        &session,
        &token,
    );

    let mut stream = market
        .subscribe_asset_state(request)
        .await
        .expect("valid stream request must authenticate")
        .into_inner();
    let first = stream
        .message()
        .await
        .expect("snapshot frame must decode")
        .expect("snapshot frame must be present");
    assert_eq!(first.asset, Some(asset.clone()));
    assert_eq!(first.consolidated_price, "67234.105");
    assert_eq!(first.health, SnapshotHealth::Healthy as i32);
    let first_metadata = first.stream.expect("snapshot metadata must be present");
    assert_eq!(first_metadata.stream_id, "market-asset-state");
    assert_eq!(first_metadata.stream_sequence, 1);
    assert_eq!(
        first_metadata.snapshot_or_delta,
        StreamFrameKind::Snapshot as i32
    );
    assert_eq!(first_metadata.resume_token, "1");
    assert_eq!(first_metadata.schema_version, 1);
    assert_eq!(first_metadata.dropped_since_previous, 0);
    assert_eq!(first_metadata.coalesced_since_previous, 0);
    assert_eq!(
        first_metadata.delivery_policy,
        StreamDeliveryPolicy::CoalesceSuperseded as i32
    );
    assert_eq!(
        first_metadata.terminal_status,
        StreamTerminalStatus::Open as i32
    );
    assert!(first_metadata.terminal_error.is_none());

    let second = stream
        .message()
        .await
        .expect("terminal frame must decode")
        .expect("terminal frame must be present");
    assert_eq!(second.asset, Some(asset));
    let second_metadata = second.stream.expect("terminal metadata must be present");
    assert_eq!(second_metadata.stream_id, "market-asset-state");
    assert_eq!(second_metadata.stream_sequence, 2);
    assert_eq!(
        second_metadata.snapshot_or_delta,
        StreamFrameKind::Terminal as i32
    );
    assert_eq!(second_metadata.resume_token, "2");
    assert_eq!(
        second_metadata.terminal_status,
        StreamTerminalStatus::Completed as i32
    );
    assert!(second_metadata.terminal_error.is_none());
    assert!(
        stream
            .message()
            .await
            .expect("stream completion must decode")
            .is_none(),
        "stream must close immediately after its typed terminal frame"
    );

    server
        .shutdown()
        .await
        .expect("server must shut down cleanly");
}

#[tokio::test]
async fn asset_stream_rejects_missing_authentication_and_empty_selection() {
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

    let unauthenticated = market
        .subscribe_asset_state(SubscribeAssetStateRequest {
            assets: Vec::new(),
            resume_token: String::new(),
        })
        .await
        .expect_err("missing stream credentials must fail");
    assert_eq!(unauthenticated.code(), Code::Unauthenticated);
    assert_eq!(unauthenticated.message(), "authentication failed");

    let empty = insert_authentication_metadata(
        Request::new(SubscribeAssetStateRequest {
            assets: Vec::new(),
            resume_token: String::new(),
        }),
        &session,
        &token,
    );
    let invalid = market
        .subscribe_asset_state(empty)
        .await
        .expect_err("empty stream selection must fail");
    assert_eq!(invalid.code(), Code::InvalidArgument);
    assert_eq!(
        invalid.message(),
        "exactly one asset is required by this runtime"
    );

    server
        .shutdown()
        .await
        .expect("server must shut down cleanly");
}

#[tokio::test]
async fn market_streams_reject_identity_confusion_and_multiple_filters() {
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

    let mut different_asset = requested_asset();
    different_asset.canonical_symbol = "ETH".to_owned();
    let mismatch = insert_authentication_metadata(
        Request::new(SubscribeAssetStateRequest {
            assets: vec![different_asset],
            resume_token: String::new(),
        }),
        &session,
        &token,
    );
    let mismatch = market
        .subscribe_asset_state(mismatch)
        .await
        .expect_err("a different asset must not inherit the configured snapshot");
    assert_eq!(mismatch.code(), Code::NotFound);
    assert_eq!(mismatch.message(), "requested asset is not available");

    let multiple = insert_authentication_metadata(
        Request::new(SubscribeAssetStateRequest {
            assets: vec![requested_asset(), requested_asset()],
            resume_token: String::new(),
        }),
        &session,
        &token,
    );
    let multiple = market
        .subscribe_asset_state(multiple)
        .await
        .expect_err("the single-snapshot runtime must reject multiple assets");
    assert_eq!(multiple.code(), Code::InvalidArgument);
    assert_eq!(
        multiple.message(),
        "exactly one asset is required by this runtime"
    );

    let wrong_venue = insert_authentication_metadata(
        Request::new(SubscribeVenueStateRequest {
            venue_ids: vec!["coinbase".to_owned()],
            resume_token: String::new(),
        }),
        &session,
        &token,
    );
    let wrong_venue = market
        .subscribe_venue_state(wrong_venue)
        .await
        .expect_err("a different venue must not inherit the configured snapshot");
    assert_eq!(wrong_venue.code(), Code::NotFound);
    assert_eq!(wrong_venue.message(), "requested venue is not available");

    let multiple_venues = insert_authentication_metadata(
        Request::new(SubscribeVenueStateRequest {
            venue_ids: vec!["binance".to_owned(), "binance".to_owned()],
            resume_token: String::new(),
        }),
        &session,
        &token,
    );
    let multiple_venues = market
        .subscribe_venue_state(multiple_venues)
        .await
        .expect_err("the single-snapshot runtime must reject multiple venues");
    assert_eq!(multiple_venues.code(), Code::InvalidArgument);
    assert_eq!(
        multiple_venues.message(),
        "exactly one venue is required by this runtime"
    );

    let valid_venue = insert_authentication_metadata(
        Request::new(SubscribeVenueStateRequest {
            venue_ids: vec!["binance".to_owned()],
            resume_token: "not-retained".to_owned(),
        }),
        &session,
        &token,
    );
    let mut venue_stream = market
        .subscribe_venue_state(valid_venue)
        .await
        .expect("the configured venue must stream")
        .into_inner();
    let snapshot = venue_stream
        .message()
        .await
        .expect("venue snapshot must decode")
        .expect("venue snapshot must be present");
    assert_eq!(snapshot.venue_id, "binance");
    let metadata = snapshot.stream.expect("venue snapshot metadata must exist");
    assert_eq!(metadata.snapshot_or_delta, StreamFrameKind::Snapshot as i32);
    assert_eq!(metadata.stream_sequence, 1);
    let terminal = venue_stream
        .message()
        .await
        .expect("venue terminal frame must decode")
        .expect("venue terminal frame must be present");
    let metadata = terminal.stream.expect("venue terminal metadata must exist");
    assert_eq!(metadata.snapshot_or_delta, StreamFrameKind::Terminal as i32);
    assert_eq!(
        metadata.terminal_status,
        StreamTerminalStatus::Completed as i32
    );
    assert!(
        venue_stream
            .message()
            .await
            .expect("venue completion must decode")
            .is_none()
    );

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

    let mut missing_token = insert_authentication_metadata(
        Request::new(GetOrderBookSnapshotRequest {}),
        &session,
        &token,
    );
    missing_token.metadata_mut().remove_bin(TOKEN_METADATA_KEY);
    assert_eq!(
        market
            .get_order_book_snapshot(missing_token)
            .await
            .expect_err("missing token must fail")
            .code(),
        Code::Unauthenticated
    );

    let mut mutated = *token.as_bytes();
    mutated[0] ^= 1;
    let mutated = local_api::auth::AuthenticationToken::from_bytes(mutated);
    let mutated_request = insert_authentication_metadata(
        Request::new(GetOrderBookSnapshotRequest {}),
        &session,
        &mutated,
    );
    assert_eq!(
        market
            .get_order_book_snapshot(mutated_request)
            .await
            .expect_err("mutated token must fail")
            .code(),
        Code::Unauthenticated
    );

    let valid_request = insert_authentication_metadata(
        Request::new(GetOrderBookSnapshotRequest {}),
        &session,
        &token,
    );
    market
        .get_order_book_snapshot(valid_request)
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
        .get_order_book_snapshot(insert_authentication_metadata(
            Request::new(GetOrderBookSnapshotRequest {}),
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
    let request = OversizedRequest {
        padding: vec![0_u8; 64],
    };
    let encoded_length = request.encoded_len();
    let limits = ServerLimits::new(
        encoded_length,
        4,
        Duration::from_secs(1),
        Duration::from_millis(250),
    )
    .expect("bounded custom server limits must validate");
    let server = LoopbackServer::spawn_with_limits(
        "127.0.0.1:0".parse().expect("bind address must parse"),
        secret(),
        descriptor(),
        snapshot(),
        limits,
    )
    .await
    .expect("exact loopback bind must start");
    let mut grpc = Grpc::new(channel(server.local_addr()).await);
    grpc.ready()
        .await
        .expect("local RPC client must become ready");

    let accepted: Result<tonic::Response<CheckResponse>, tonic::Status> = grpc
        .unary(
            Request::new(request.clone()),
            PathAndQuery::from_static("/cmti.health.v1.HealthService/Check"),
            tonic_prost::ProstCodec::default(),
        )
        .await;
    assert_eq!(
        accepted
            .expect("payload at the configured encoded boundary must pass")
            .into_inner()
            .status,
        ServingStatus::Serving as i32
    );
    server
        .shutdown()
        .await
        .expect("boundary server must shut down cleanly");

    let limits = ServerLimits::new(
        encoded_length - 1,
        4,
        Duration::from_secs(1),
        Duration::from_millis(250),
    )
    .expect("one-byte-smaller server limit must validate");
    let server = LoopbackServer::spawn_with_limits(
        "127.0.0.1:0".parse().expect("bind address must parse"),
        secret(),
        descriptor(),
        snapshot(),
        limits,
    )
    .await
    .expect("exact loopback bind must start");
    let mut grpc = Grpc::new(channel(server.local_addr()).await);
    grpc.ready()
        .await
        .expect("local RPC client must become ready");
    let rejected: Result<tonic::Response<CheckResponse>, tonic::Status> = grpc
        .unary(
            Request::new(request),
            PathAndQuery::from_static("/cmti.health.v1.HealthService/Check"),
            tonic_prost::ProstCodec::default(),
        )
        .await;
    assert_eq!(
        rejected
            .expect_err("payload one byte above the configured limit must fail")
            .code(),
        Code::OutOfRange
    );

    server
        .shutdown()
        .await
        .expect("server must shut down cleanly");
}

#[tokio::test]
async fn shutdown_forces_a_stuck_connection_closed_within_the_configured_grace() {
    let limits = ServerLimits::new(256, 1, Duration::from_secs(2), Duration::from_millis(20))
        .expect("short shutdown test limits must validate");
    let server = LoopbackServer::spawn_with_limits(
        "127.0.0.1:0".parse().expect("bind address must parse"),
        secret(),
        descriptor(),
        snapshot(),
        limits,
    )
    .await
    .expect("exact loopback bind must start");
    let _stuck_connection = TcpStream::connect(server.local_addr())
        .await
        .expect("raw loopback connection must open");
    tokio::task::yield_now().await;

    let started = Instant::now();
    tokio::time::timeout(Duration::from_millis(500), server.shutdown())
        .await
        .expect("shutdown must honor the configured internal deadline")
        .expect("forced cleanup must complete successfully");
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "shutdown must not silently retain the old 250 ms fixed grace"
    );
}
