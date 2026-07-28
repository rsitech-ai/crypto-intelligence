//! Minimal health and authenticated market services on IPv4 loopback.

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use domain::{
    AssetId as DomainAssetId, AssetNamespace as DomainAssetNamespace, InstrumentId, SourceId,
    UnixNanos,
};
use fixed_decimal::{FixedDecimal, Price};
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::oneshot,
    task::{JoinError, JoinHandle},
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status, transport::Server};
use tower::limit::GlobalConcurrencyLimitLayer;

use crate::{
    auth::{
        Clock, SESSION_DESCRIPTOR_METADATA_KEY, SessionAuthenticator, SessionSecret, SystemClock,
        TOKEN_METADATA_KEY,
    },
    proto::{
        common_v1::{
            AssetId as ProtoAssetId, AssetNamespace as ProtoAssetNamespace, ProtocolVersion,
            StreamDeliveryPolicy, StreamFrameKind, StreamMetadata, StreamTerminalStatus,
            UnixNanos as ProtoUnixNanos,
        },
        health_v1::{
            CheckRequest, CheckResponse, ServingStatus,
            health_service_server::{HealthService as HealthServiceRpc, HealthServiceServer},
        },
        market_v1::{
            GetOrderBookSnapshotRequest, GetOrderBookSnapshotResponse, SnapshotHealth,
            SubscribeAssetStateRequest, SubscribeAssetStateResponse, SubscribeVenueStateRequest,
            SubscribeVenueStateResponse,
            market_state_service_server::{
                MarketStateService as MarketStateServiceRpc, MarketStateServiceServer,
            },
        },
    },
    session::SessionDescriptor,
};

const LOOPBACK_BIND: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);
const AUTHENTICATION_FAILED: &str = "authentication failed";
const DEFAULT_MAX_REQUEST_MESSAGE_BYTES: usize = 256;
const MAX_RESPONSE_MESSAGE_BYTES: usize = 1_024;
const MAX_HEADER_LIST_BYTES: u32 = 2_048;
const STREAM_WINDOW_BYTES: u32 = 16 * 1_024;
const CONNECTION_WINDOW_BYTES: u32 = 32 * 1_024;
const DEFAULT_CONCURRENCY_LIMIT_PER_CONNECTION: usize = 1;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONNECTION_AGE: Duration = Duration::from_secs(60);
const MAX_CONNECTION_AGE_GRACE: Duration = Duration::from_secs(1);
const DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);

/// Validated resource limits applied by the local transport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerLimits {
    maximum_request_bytes: usize,
    maximum_concurrent_requests: usize,
    request_timeout: Duration,
    shutdown_grace: Duration,
}

impl ServerLimits {
    pub fn new(
        maximum_request_bytes: usize,
        maximum_concurrent_requests: usize,
        request_timeout: Duration,
        shutdown_grace: Duration,
    ) -> Result<Self, ServerError> {
        if maximum_request_bytes == 0
            || maximum_concurrent_requests == 0
            || u32::try_from(maximum_concurrent_requests).is_err()
            || request_timeout.is_zero()
            || shutdown_grace.is_zero()
        {
            return Err(ServerError::InvalidLimits);
        }
        Ok(Self {
            maximum_request_bytes,
            maximum_concurrent_requests,
            request_timeout,
            shutdown_grace,
        })
    }

    fn foundation_defaults() -> Self {
        Self {
            maximum_request_bytes: DEFAULT_MAX_REQUEST_MESSAGE_BYTES,
            maximum_concurrent_requests: DEFAULT_CONCURRENCY_LIMIT_PER_CONNECTION,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            shutdown_grace: DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT,
        }
    }
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self::foundation_defaults()
    }
}

/// Invalid authoritative market snapshot state.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SnapshotError {
    #[error("source and instrument generations must match")]
    GenerationMismatch,
    #[error("snapshot sequence must be nonzero")]
    SequenceZero,
    #[error("best bid must not exceed best ask")]
    CrossedBook,
    #[error("event timestamp must not exceed receive timestamp")]
    InvalidTimestamps,
    #[error("snapshot health must be specified")]
    UnspecifiedHealth,
    #[error("snapshot midpoint cannot be represented exactly")]
    MidpointUnrepresentable,
}

/// Immutable authoritative market state returned to local clients.
#[derive(Clone)]
pub struct MarketSnapshot {
    source: SourceId,
    instrument: InstrumentId,
    asset: DomainAssetId,
    sequence: u64,
    best_bid: Price,
    best_ask: Price,
    consolidated_price: Price,
    health: SnapshotHealth,
    event_timestamp: UnixNanos,
    receive_timestamp: UnixNanos,
    freshness_millis: u64,
}

impl MarketSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: SourceId,
        instrument: InstrumentId,
        asset: DomainAssetId,
        sequence: u64,
        best_bid: Price,
        best_ask: Price,
        health: SnapshotHealth,
        event_timestamp: UnixNanos,
        receive_timestamp: UnixNanos,
        freshness_millis: u64,
    ) -> Result<Self, SnapshotError> {
        if source.generation() != instrument.generation()
            || source.generation() != asset.generation()
        {
            return Err(SnapshotError::GenerationMismatch);
        }
        if sequence == 0 {
            return Err(SnapshotError::SequenceZero);
        }
        if best_bid > best_ask {
            return Err(SnapshotError::CrossedBook);
        }
        if event_timestamp > receive_timestamp {
            return Err(SnapshotError::InvalidTimestamps);
        }
        if health == SnapshotHealth::Unspecified {
            return Err(SnapshotError::UnspecifiedHealth);
        }
        let half = FixedDecimal::new(5, 1).expect("one half is always representable");
        let consolidated_price = best_ask
            .value()
            .checked_sub(best_bid.value())
            .and_then(|spread| spread.checked_mul(half))
            .and_then(|half_spread| best_bid.value().checked_add(half_spread))
            .and_then(Price::new)
            .map_err(|_| SnapshotError::MidpointUnrepresentable)?;

        Ok(Self {
            source,
            instrument,
            asset,
            sequence,
            best_bid,
            best_ask,
            consolidated_price,
            health,
            event_timestamp,
            receive_timestamp,
            freshness_millis,
        })
    }

    fn response(&self) -> GetOrderBookSnapshotResponse {
        GetOrderBookSnapshotResponse {
            source: self.source.name().to_owned(),
            symbol: self.instrument.venue_symbol().to_owned(),
            generation: self.instrument.generation(),
            sequence: self.sequence,
            best_bid: self.best_bid.to_string(),
            best_ask: self.best_ask.to_string(),
            health: self.health as i32,
            event_unix_nanos: self.event_timestamp.value(),
            receive_unix_nanos: self.receive_timestamp.value(),
            freshness_millis: self.freshness_millis,
        }
    }

    fn proto_asset(&self) -> ProtoAssetId {
        let namespace = match self.asset.namespace() {
            DomainAssetNamespace::Native => ProtoAssetNamespace::Native,
            DomainAssetNamespace::Evm => ProtoAssetNamespace::Evm,
            DomainAssetNamespace::Solana => ProtoAssetNamespace::Solana,
            DomainAssetNamespace::Fiat => ProtoAssetNamespace::Fiat,
            DomainAssetNamespace::Synthetic => ProtoAssetNamespace::Synthetic,
        };
        ProtoAssetId {
            namespace: namespace as i32,
            chain_id: self.asset.chain_id().to_owned(),
            contract_or_mint: self.asset.contract_or_mint().to_owned(),
            canonical_symbol: self.asset.canonical_symbol().to_owned(),
            generation: self.asset.generation(),
        }
    }

    fn matches_asset(&self, requested: &ProtoAssetId) -> bool {
        *requested == self.proto_asset()
    }
}

/// Minimal public liveness implementation.
#[derive(Clone)]
struct HealthService {
    protocol: ProtocolVersion,
}

impl HealthService {
    const fn new(protocol_major: u32, protocol_minor: u32) -> Self {
        Self {
            protocol: ProtocolVersion {
                major: protocol_major,
                minor: protocol_minor,
            },
        }
    }
}

#[tonic::async_trait]
impl HealthServiceRpc for HealthService {
    async fn check(
        &self,
        _request: Request<CheckRequest>,
    ) -> Result<Response<CheckResponse>, Status> {
        Ok(Response::new(CheckResponse {
            status: ServingStatus::Serving as i32,
            protocol: Some(self.protocol),
        }))
    }
}

/// Authenticated authoritative snapshot implementation.
#[derive(Clone)]
struct MarketStateService {
    expected_descriptor: SessionDescriptor,
    authenticator: Arc<SessionAuthenticator>,
    clock: Arc<dyn Clock>,
    snapshot: MarketSnapshot,
    #[cfg(test)]
    response_delay: Duration,
}

impl MarketStateService {
    fn new(
        expected_descriptor: SessionDescriptor,
        authenticator: Arc<SessionAuthenticator>,
        clock: Arc<dyn Clock>,
        snapshot: MarketSnapshot,
    ) -> Self {
        Self {
            expected_descriptor,
            authenticator,
            clock,
            snapshot,
            #[cfg(test)]
            response_delay: Duration::ZERO,
        }
    }

    #[cfg(test)]
    fn with_response_delay(mut self, response_delay: Duration) -> Self {
        self.response_delay = response_delay;
        self
    }

    fn authenticate<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let descriptor_metadata = request
            .metadata()
            .get_bin(SESSION_DESCRIPTOR_METADATA_KEY)
            .ok_or_else(authentication_failed)?;
        let descriptor_bytes = descriptor_metadata
            .to_bytes()
            .map_err(|_| authentication_failed())?;
        let descriptor = SessionDescriptor::from_canonical_bytes(&descriptor_bytes)
            .map_err(|_| authentication_failed())?;
        if descriptor != self.expected_descriptor {
            return Err(authentication_failed());
        }

        let token_metadata = request
            .metadata()
            .get_bin(TOKEN_METADATA_KEY)
            .ok_or_else(authentication_failed)?;
        let token = token_metadata
            .to_bytes()
            .map_err(|_| authentication_failed())?;
        self.authenticator
            .validate_at(&descriptor, &token, self.clock.unix_seconds())
            .map_err(|_| authentication_failed())
    }

    fn stream_metadata(
        &self,
        stream_id: &str,
        stream_sequence: u64,
        frame: StreamFrameKind,
        terminal_status: StreamTerminalStatus,
    ) -> StreamMetadata {
        StreamMetadata {
            stream_id: stream_id.to_owned(),
            stream_sequence,
            snapshot_or_delta: frame as i32,
            as_of_time: Some(ProtoUnixNanos {
                value: self.clock.unix_seconds().saturating_mul(1_000_000_000),
            }),
            resume_token: stream_sequence.to_string(),
            schema_version: 1,
            dropped_since_previous: 0,
            coalesced_since_previous: 0,
            delivery_policy: StreamDeliveryPolicy::CoalesceSuperseded as i32,
            terminal_status: terminal_status as i32,
            terminal_error: None,
        }
    }
}

#[tonic::async_trait]
impl MarketStateServiceRpc for MarketStateService {
    type SubscribeAssetStateStream =
        tokio_stream::Iter<std::vec::IntoIter<Result<SubscribeAssetStateResponse, Status>>>;

    async fn subscribe_asset_state(
        &self,
        request: Request<SubscribeAssetStateRequest>,
    ) -> Result<Response<Self::SubscribeAssetStateStream>, Status> {
        self.authenticate(&request)?;
        let requested = request.into_inner();
        if requested.assets.len() != 1 {
            return Err(Status::invalid_argument(
                "exactly one asset is required by this runtime",
            ));
        }
        if !self.snapshot.matches_asset(&requested.assets[0]) {
            return Err(Status::not_found("requested asset is not available"));
        }
        let asset = self.snapshot.proto_asset();
        let snapshot = SubscribeAssetStateResponse {
            stream: Some(self.stream_metadata(
                "market-asset-state",
                1,
                StreamFrameKind::Snapshot,
                StreamTerminalStatus::Open,
            )),
            asset: Some(asset.clone()),
            consolidated_price: self.snapshot.consolidated_price.to_string(),
            health: self.snapshot.health as i32,
        };
        let terminal = SubscribeAssetStateResponse {
            stream: Some(self.stream_metadata(
                "market-asset-state",
                2,
                StreamFrameKind::Terminal,
                StreamTerminalStatus::Completed,
            )),
            asset: Some(asset),
            consolidated_price: self.snapshot.consolidated_price.to_string(),
            health: self.snapshot.health as i32,
        };
        Ok(Response::new(tokio_stream::iter(vec![
            Ok(snapshot),
            Ok(terminal),
        ])))
    }

    type SubscribeVenueStateStream =
        tokio_stream::Iter<std::vec::IntoIter<Result<SubscribeVenueStateResponse, Status>>>;

    async fn subscribe_venue_state(
        &self,
        request: Request<SubscribeVenueStateRequest>,
    ) -> Result<Response<Self::SubscribeVenueStateStream>, Status> {
        self.authenticate(&request)?;
        let requested = request.into_inner();
        if requested.venue_ids.len() != 1 {
            return Err(Status::invalid_argument(
                "exactly one venue is required by this runtime",
            ));
        }
        let venue_id = self.snapshot.instrument.venue().as_str().to_owned();
        if requested.venue_ids[0] != venue_id {
            return Err(Status::not_found("requested venue is not available"));
        }
        let snapshot = SubscribeVenueStateResponse {
            stream: Some(self.stream_metadata(
                "market-venue-state",
                1,
                StreamFrameKind::Snapshot,
                StreamTerminalStatus::Open,
            )),
            venue_id: venue_id.clone(),
            health: self.snapshot.health as i32,
            source_latency_millis: self.snapshot.freshness_millis,
        };
        let terminal = SubscribeVenueStateResponse {
            stream: Some(self.stream_metadata(
                "market-venue-state",
                2,
                StreamFrameKind::Terminal,
                StreamTerminalStatus::Completed,
            )),
            venue_id,
            health: self.snapshot.health as i32,
            source_latency_millis: self.snapshot.freshness_millis,
        };
        Ok(Response::new(tokio_stream::iter(vec![
            Ok(snapshot),
            Ok(terminal),
        ])))
    }

    async fn get_order_book_snapshot(
        &self,
        request: Request<GetOrderBookSnapshotRequest>,
    ) -> Result<Response<GetOrderBookSnapshotResponse>, Status> {
        self.authenticate(&request)?;
        #[cfg(test)]
        if !self.response_delay.is_zero() {
            tokio::time::sleep(self.response_delay).await;
        }
        Ok(Response::new(self.snapshot.response()))
    }
}

fn authentication_failed() -> Status {
    Status::unauthenticated(AUTHENTICATION_FAILED)
}

/// Startup or graceful-shutdown failures for the loopback server.
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("server bind must be exactly 127.0.0.1:0")]
    NonLoopbackBind,
    #[error("local RPC resource limits must be nonzero and transport-representable")]
    InvalidLimits,
    #[error("could not bind the local RPC listener: {source}")]
    Bind {
        #[source]
        source: io::Error,
    },
    #[error("local RPC transport failed: {source}")]
    Transport {
        #[source]
        source: tonic::transport::Error,
    },
    #[error("local RPC server task failed: {source}")]
    Join {
        #[source]
        source: JoinError,
    },
    #[error("local RPC server stopped before graceful shutdown")]
    ShutdownUnavailable,
}

/// Running loopback server with explicit graceful shutdown ownership.
pub struct LoopbackServer {
    local_addr: SocketAddr,
    limits: ServerLimits,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), tonic::transport::Error>>,
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}

impl LoopbackServer {
    /// Binds only the exact ephemeral IPv4 loopback address and starts both
    /// services without reflection.
    pub async fn spawn(
        bind: SocketAddr,
        secret: SessionSecret,
        descriptor: SessionDescriptor,
        snapshot: MarketSnapshot,
    ) -> Result<Self, ServerError> {
        Self::spawn_with_limits(
            bind,
            secret,
            descriptor,
            snapshot,
            ServerLimits::foundation_defaults(),
        )
        .await
    }

    /// Starts the server with already validated application resource limits.
    pub async fn spawn_with_limits(
        bind: SocketAddr,
        secret: SessionSecret,
        descriptor: SessionDescriptor,
        snapshot: MarketSnapshot,
        limits: ServerLimits,
    ) -> Result<Self, ServerError> {
        Self::spawn_with_clock(
            bind,
            secret,
            descriptor,
            snapshot,
            Arc::new(SystemClock),
            limits,
        )
        .await
    }

    async fn spawn_with_clock(
        bind: SocketAddr,
        secret: SessionSecret,
        descriptor: SessionDescriptor,
        snapshot: MarketSnapshot,
        clock: Arc<dyn Clock>,
        limits: ServerLimits,
    ) -> Result<Self, ServerError> {
        let listener = Self::bind_loopback(bind).await?;
        let authenticator = Arc::new(SessionAuthenticator::new(secret));
        let health = HealthService::new(descriptor.protocol_major(), descriptor.protocol_minor());
        let market = MarketStateService::new(descriptor, authenticator, clock, snapshot);
        Self::spawn_services(listener, health, market, limits).await
    }

    #[cfg(test)]
    async fn spawn_with_clock_and_delay(
        bind: SocketAddr,
        secret: SessionSecret,
        descriptor: SessionDescriptor,
        snapshot: MarketSnapshot,
        clock: Arc<dyn Clock>,
        limits: ServerLimits,
        response_delay: Duration,
    ) -> Result<Self, ServerError> {
        let listener = Self::bind_loopback(bind).await?;
        let authenticator = Arc::new(SessionAuthenticator::new(secret));
        let health = HealthService::new(descriptor.protocol_major(), descriptor.protocol_minor());
        let market = MarketStateService::new(descriptor, authenticator, clock, snapshot)
            .with_response_delay(response_delay);
        Self::spawn_services(listener, health, market, limits).await
    }

    async fn bind_loopback(bind: SocketAddr) -> Result<TcpListener, ServerError> {
        if bind != crate::server::LOOPBACK_BIND {
            return Err(ServerError::NonLoopbackBind);
        }
        TcpListener::bind(crate::server::LOOPBACK_BIND)
            .await
            .map_err(|source| ServerError::Bind { source })
    }

    async fn spawn_services(
        listener: TcpListener,
        health: HealthService,
        market: MarketStateService,
        limits: ServerLimits,
    ) -> Result<Self, ServerError> {
        let local_addr = listener
            .local_addr()
            .map_err(|source| ServerError::Bind { source })?;
        let incoming = TcpListenerStream::new(listener);
        let (shutdown, shutdown_signal) = oneshot::channel();
        let health = HealthServiceServer::new(health)
            .max_decoding_message_size(limits.maximum_request_bytes)
            .max_encoding_message_size(MAX_RESPONSE_MESSAGE_BYTES);
        let market = MarketStateServiceServer::new(market)
            .max_decoding_message_size(limits.maximum_request_bytes)
            .max_encoding_message_size(MAX_RESPONSE_MESSAGE_BYTES);
        let maximum_concurrent_streams = u32::try_from(limits.maximum_concurrent_requests)
            .map_err(|_| ServerError::InvalidLimits)?;

        let task = tokio::spawn(async move {
            Server::builder()
                .load_shed(true)
                .timeout(limits.request_timeout)
                .initial_stream_window_size(STREAM_WINDOW_BYTES)
                .initial_connection_window_size(CONNECTION_WINDOW_BYTES)
                .max_concurrent_streams(maximum_concurrent_streams)
                .http2_max_header_list_size(MAX_HEADER_LIST_BYTES)
                .max_frame_size(STREAM_WINDOW_BYTES)
                .max_connection_age(MAX_CONNECTION_AGE)
                .max_connection_age_grace(MAX_CONNECTION_AGE_GRACE)
                .layer(GlobalConcurrencyLimitLayer::new(
                    limits.maximum_concurrent_requests,
                ))
                .add_service(health)
                .add_service(market)
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_signal.await;
                })
                .await
        });

        Ok(Self {
            local_addr,
            limits,
            shutdown: Some(shutdown),
            task,
        })
    }

    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn shutdown(mut self) -> Result<(), ServerError> {
        self.shutdown
            .take()
            .ok_or(ServerError::ShutdownUnavailable)?
            .send(())
            .map_err(|_| ServerError::ShutdownUnavailable)?;
        match tokio::time::timeout(self.limits.shutdown_grace, &mut self.task).await {
            Ok(result) => server_task_result(result),
            Err(_) => {
                self.task.abort();
                match (&mut self.task).await {
                    Err(source) if source.is_cancelled() => Ok(()),
                    result => server_task_result(result),
                }
            }
        }
    }
}

fn server_task_result(
    result: Result<Result<(), tonic::transport::Error>, JoinError>,
) -> Result<(), ServerError> {
    result
        .map_err(|source| ServerError::Join { source })?
        .map_err(|source| ServerError::Transport { source })
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::Duration,
    };

    use domain::{AssetNamespace as DomainAssetNamespace, SourceKind, VenueId};
    use fixed_decimal::FixedDecimal;
    use tonic::{
        Code, Request, Response, Status,
        client::Grpc,
        codegen::http::uri::PathAndQuery,
        transport::{Channel, Endpoint},
    };
    use zeroize::Zeroizing;

    use super::*;
    use crate::{
        auth::{SessionAuthenticator, insert_authentication_metadata},
        proto::market_v1::{GetOrderBookSnapshotRequest, GetOrderBookSnapshotResponse},
    };

    const ISSUED_AT: i64 = 1_700_000_000;

    struct BlockingFirstClock {
        first_call: AtomicBool,
        entered: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    struct FixedClock;

    impl Clock for FixedClock {
        fn unix_seconds(&self) -> i64 {
            ISSUED_AT
        }
    }

    impl Clock for BlockingFirstClock {
        fn unix_seconds(&self) -> i64 {
            if !self.first_call.swap(true, Ordering::SeqCst) {
                if let Some(entered) = self
                    .entered
                    .lock()
                    .expect("test clock entry lock must not be poisoned")
                    .take()
                {
                    let _ = entered.send(());
                }
                let _ = self
                    .release
                    .lock()
                    .expect("test clock release lock must not be poisoned")
                    .recv();
            }
            ISSUED_AT
        }
    }

    fn test_secret() -> SessionSecret {
        SessionSecret::try_from(Zeroizing::new(vec![0x6b; 32])).expect("test secret must be valid")
    }

    fn test_descriptor() -> SessionDescriptor {
        SessionDescriptor::issue(1, 0, 4_242, [0x11; 16], [0x22; 16], ISSUED_AT)
            .expect("test descriptor must be valid")
    }

    fn test_snapshot() -> MarketSnapshot {
        let source =
            SourceId::new(SourceKind::Exchange, "binance", 7).expect("source must be valid");
        let instrument = InstrumentId::new(
            VenueId::new("binance").expect("venue must be valid"),
            "btcusdt",
            7,
        )
        .expect("instrument must be valid");
        let bid =
            Price::new(FixedDecimal::parse_canonical("67234.1").expect("bid must be canonical"))
                .expect("bid must be valid");
        let ask =
            Price::new(FixedDecimal::parse_canonical("67234.11").expect("ask must be canonical"))
                .expect("ask must be valid");
        MarketSnapshot::new(
            source,
            instrument,
            DomainAssetId::new(DomainAssetNamespace::Native, "bitcoin", "", "BTC", 7)
                .expect("asset must be valid"),
            9_001,
            bid,
            ask,
            SnapshotHealth::Healthy,
            UnixNanos::new(1_700_000_000_100_000_000),
            UnixNanos::new(1_700_000_000_125_000_000),
            25,
        )
        .expect("snapshot must be valid")
    }

    async fn get_order_book_snapshot(
        client: &mut Grpc<Channel>,
        request: Request<GetOrderBookSnapshotRequest>,
    ) -> Result<Response<GetOrderBookSnapshotResponse>, Status> {
        client
            .ready()
            .await
            .map_err(|_| Status::unavailable("loopback transport unavailable"))?;
        client
            .unary(
                request,
                PathAndQuery::from_static(
                    "/cmti.market.v1.MarketStateService/GetOrderBookSnapshot",
                ),
                tonic_prost::ProstCodec::default(),
            )
            .await
    }

    #[tokio::test]
    async fn configured_request_timeout_cancels_a_pending_handler() {
        let descriptor = test_descriptor();
        let token = SessionAuthenticator::new(test_secret()).token(&descriptor);
        let server = LoopbackServer::spawn_with_clock_and_delay(
            LOOPBACK_BIND,
            test_secret(),
            descriptor.clone(),
            test_snapshot(),
            Arc::new(FixedClock),
            ServerLimits::new(
                256,
                1,
                Duration::from_millis(20),
                Duration::from_millis(250),
            )
            .expect("timeout test limits must validate"),
            Duration::from_millis(200),
        )
        .await
        .expect("timeout test server must start");
        let channel = Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("test endpoint must be valid")
            .connect()
            .await
            .expect("test connection must open");
        let status = get_order_book_snapshot(
            &mut Grpc::new(channel),
            insert_authentication_metadata(
                Request::new(GetOrderBookSnapshotRequest {}),
                &descriptor,
                &token,
            ),
        )
        .await
        .expect_err("pending handler must exceed the configured request timeout");
        assert_eq!(status.code(), Code::Cancelled);
        assert_eq!(status.message(), "Timeout expired");
        server.shutdown().await.expect("test server must stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn global_concurrency_is_bounded_across_independent_connections() {
        let (entered_sender, entered_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let clock = Arc::new(BlockingFirstClock {
            first_call: AtomicBool::new(false),
            entered: Mutex::new(Some(entered_sender)),
            release: Mutex::new(release_receiver),
        });
        let descriptor = test_descriptor();
        let authenticator = SessionAuthenticator::new(test_secret());
        let token = authenticator.token(&descriptor);
        let server = LoopbackServer::spawn_with_clock(
            LOOPBACK_BIND,
            test_secret(),
            descriptor.clone(),
            test_snapshot(),
            clock,
            ServerLimits::new(256, 1, Duration::from_secs(2), Duration::from_millis(250))
                .expect("test limits must validate"),
        )
        .await
        .expect("test server must start");
        let endpoint = format!("http://{}", server.local_addr());
        let first_channel = Endpoint::from_shared(endpoint.clone())
            .expect("test endpoint must be valid")
            .connect()
            .await
            .expect("test connection must open");
        let second_channel = Endpoint::from_shared(endpoint)
            .expect("test endpoint must be valid")
            .connect()
            .await
            .expect("second test connection must open");
        let mut first_client = Grpc::new(first_channel);
        let mut second_client = Grpc::new(second_channel);
        let first_descriptor = descriptor.clone();
        let first_token = token.clone();
        let first_request = tokio::spawn(async move {
            get_order_book_snapshot(
                &mut first_client,
                insert_authentication_metadata(
                    Request::new(GetOrderBookSnapshotRequest {}),
                    &first_descriptor,
                    &first_token,
                ),
            )
            .await
        });
        tokio::task::spawn_blocking(move || entered_receiver.recv_timeout(Duration::from_secs(1)))
            .await
            .expect("entry observer task must join")
            .expect("first request must reach authentication");

        let second_result = tokio::time::timeout(
            Duration::from_secs(1),
            get_order_book_snapshot(
                &mut second_client,
                insert_authentication_metadata(
                    Request::new(GetOrderBookSnapshotRequest {}),
                    &descriptor,
                    &token,
                ),
            ),
        )
        .await;
        release_sender
            .send(())
            .expect("first request must still be waiting");
        first_request
            .await
            .expect("first request task must join")
            .expect("first request must complete after release");
        server.shutdown().await.expect("test server must stop");

        let status = second_result
            .expect("second request must receive a bounded response")
            .expect_err("second concurrent request must be rejected");
        assert_eq!(status.code(), Code::ResourceExhausted);
    }
}
