//! Minimal health and authenticated market services on IPv4 loopback.

use std::{io, net::SocketAddr, sync::Arc};

use domain::{InstrumentId, SourceId, UnixNanos};
use fixed_decimal::Price;
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::oneshot,
    task::{JoinError, JoinHandle},
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status, transport::Server};

use crate::{
    auth::{
        Clock, SESSION_DESCRIPTOR_METADATA_KEY, SessionAuthenticator, SessionSecret,
        TOKEN_METADATA_KEY,
    },
    proto::{
        common_v1::ProtocolVersion,
        health_v1::{
            CheckRequest, CheckResponse, ServingStatus,
            health_service_server::{HealthService as HealthServiceRpc, HealthServiceServer},
        },
        market_v1::{
            GetSnapshotRequest, GetSnapshotResponse, SnapshotHealth,
            market_service_server::{MarketService as MarketServiceRpc, MarketServiceServer},
        },
    },
    session::SessionDescriptor,
};

const LOOPBACK_BIND: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);
const AUTHENTICATION_FAILED: &str = "authentication failed";

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
}

/// Immutable authoritative market state returned to local clients.
#[derive(Clone)]
pub struct MarketSnapshot {
    source: SourceId,
    instrument: InstrumentId,
    sequence: u64,
    best_bid: Price,
    best_ask: Price,
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
        sequence: u64,
        best_bid: Price,
        best_ask: Price,
        health: SnapshotHealth,
        event_timestamp: UnixNanos,
        receive_timestamp: UnixNanos,
        freshness_millis: u64,
    ) -> Result<Self, SnapshotError> {
        if source.generation() != instrument.generation() {
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

        Ok(Self {
            source,
            instrument,
            sequence,
            best_bid,
            best_ask,
            health,
            event_timestamp,
            receive_timestamp,
            freshness_millis,
        })
    }

    fn response(&self) -> GetSnapshotResponse {
        GetSnapshotResponse {
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
}

/// Minimal public liveness implementation.
#[derive(Clone)]
pub struct HealthService {
    protocol: ProtocolVersion,
}

impl HealthService {
    pub const fn new(protocol_major: u32, protocol_minor: u32) -> Self {
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
pub struct MarketService {
    expected_descriptor: SessionDescriptor,
    authenticator: Arc<SessionAuthenticator>,
    clock: Arc<dyn Clock>,
    snapshot: MarketSnapshot,
}

impl MarketService {
    pub fn new(
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
        }
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
}

#[tonic::async_trait]
impl MarketServiceRpc for MarketService {
    async fn get_snapshot(
        &self,
        request: Request<GetSnapshotRequest>,
    ) -> Result<Response<GetSnapshotResponse>, Status> {
        self.authenticate(&request)?;
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
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), tonic::transport::Error>>,
}

impl LoopbackServer {
    /// Binds only the exact ephemeral IPv4 loopback address and starts both
    /// services without reflection.
    pub async fn spawn(
        bind: SocketAddr,
        secret: SessionSecret,
        descriptor: SessionDescriptor,
        snapshot: MarketSnapshot,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ServerError> {
        if bind != LOOPBACK_BIND {
            return Err(ServerError::NonLoopbackBind);
        }

        let listener = TcpListener::bind(bind)
            .await
            .map_err(|source| ServerError::Bind { source })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| ServerError::Bind { source })?;
        let authenticator = Arc::new(SessionAuthenticator::new(secret));
        let health = HealthService::new(descriptor.protocol_major(), descriptor.protocol_minor());
        let market = MarketService::new(descriptor, authenticator, clock, snapshot);
        let incoming = TcpListenerStream::new(listener);
        let (shutdown, shutdown_signal) = oneshot::channel();

        let task = tokio::spawn(async move {
            Server::builder()
                .add_service(HealthServiceServer::new(health))
                .add_service(MarketServiceServer::new(market))
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_signal.await;
                })
                .await
        });

        Ok(Self {
            local_addr,
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
        self.task
            .await
            .map_err(|source| ServerError::Join { source })?
            .map_err(|source| ServerError::Transport { source })
    }
}
