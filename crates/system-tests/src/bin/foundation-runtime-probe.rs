use std::{env, fs, path::PathBuf, time::Duration};

use local_api::{
    auth::{SessionAuthenticator, SessionSecret, insert_authentication_metadata},
    proto::market_v1::{GetSnapshotRequest, GetSnapshotResponse, SnapshotHealth},
    session::{SessionDescriptor, TOKEN_LIFETIME_SECONDS},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tonic::{
    Code, Request, Response, Status,
    client::Grpc,
    codegen::http::uri::PathAndQuery,
    transport::{Channel, Endpoint},
};
use zeroize::Zeroizing;

const RPC_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Readiness {
    endpoint: String,
    protocol_major: u32,
    protocol_minor: u32,
    daemon_pid: u32,
    process_nonce: String,
    server_nonce: String,
    issued_unix_seconds: i64,
    expiry_unix_seconds: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Healthy,
    Unauthenticated,
}

#[derive(Debug)]
struct Arguments {
    readiness: PathBuf,
    secret: PathBuf,
    mode: Mode,
}

#[derive(Debug, Serialize)]
struct SnapshotDigest<'a> {
    source: &'a str,
    symbol: &'a str,
    generation: u32,
    sequence: u64,
    best_bid: &'a str,
    best_ask: &'a str,
    health: i32,
    event_unix_nanos: i64,
    receive_unix_nanos: i64,
    freshness_millis: u64,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), ProbeError> {
    let arguments = parse_arguments(env::args().skip(1))?;
    let readiness: Readiness = serde_json::from_slice(
        &fs::read(&arguments.readiness).map_err(|_| ProbeError::ReadinessRead)?,
    )
    .map_err(|_| ProbeError::ReadinessInvalid)?;
    let endpoint = validate_readiness(&readiness)?;
    let descriptor = descriptor(&readiness)?;
    let secret_bytes =
        Zeroizing::new(fs::read(&arguments.secret).map_err(|_| ProbeError::SecretRead)?);
    let secret = SessionSecret::try_from(secret_bytes).map_err(|_| ProbeError::SecretInvalid)?;
    let channel = tokio::time::timeout(RPC_TIMEOUT, endpoint.connect())
        .await
        .map_err(|_| ProbeError::Timeout)?
        .map_err(|_| ProbeError::Connect)?;
    let mut market = Grpc::new(channel);

    match arguments.mode {
        Mode::Healthy => {
            let token = SessionAuthenticator::new(secret).token(&descriptor);
            let response = tokio::time::timeout(
                RPC_TIMEOUT,
                get_snapshot(
                    &mut market,
                    insert_authentication_metadata(
                        Request::new(GetSnapshotRequest {}),
                        &descriptor,
                        &token,
                    ),
                ),
            )
            .await
            .map_err(|_| ProbeError::Timeout)?
            .map_err(|_| ProbeError::HealthyRequest)?
            .into_inner();
            validate_snapshot(&response)?;
            let canonical = SnapshotDigest {
                source: &response.source,
                symbol: &response.symbol,
                generation: response.generation,
                sequence: response.sequence,
                best_bid: &response.best_bid,
                best_ask: &response.best_ask,
                health: response.health,
                event_unix_nanos: response.event_unix_nanos,
                receive_unix_nanos: response.receive_unix_nanos,
                freshness_millis: response.freshness_millis,
            };
            let canonical_json =
                serde_json::to_vec(&canonical).map_err(|_| ProbeError::Serialization)?;
            let output = serde_json::json!({
                "digest": blake3::hash(&canonical_json).to_hex().to_string(),
                "snapshot": canonical,
            });
            println!(
                "{}",
                serde_json::to_string(&output).map_err(|_| ProbeError::Serialization)?
            );
        }
        Mode::Unauthenticated => {
            drop(secret);
            let response = tokio::time::timeout(
                RPC_TIMEOUT,
                get_snapshot(&mut market, Request::new(GetSnapshotRequest {})),
            )
            .await
            .map_err(|_| ProbeError::Timeout)?;
            let status = match response {
                Ok(_) => return Err(ProbeError::AuthenticationAccepted),
                Err(status) => status,
            };
            if status.code() != Code::Unauthenticated {
                return Err(ProbeError::UnexpectedAuthCode(status.code()));
            }
            println!(r#"{{"authentication_failure":"rejected","code":"Unauthenticated"}}"#);
        }
    }
    Ok(())
}

async fn get_snapshot(
    client: &mut Grpc<Channel>,
    request: Request<GetSnapshotRequest>,
) -> Result<Response<GetSnapshotResponse>, Status> {
    client
        .ready()
        .await
        .map_err(|_| Status::unavailable("local transport unavailable"))?;
    client
        .unary(
            request,
            PathAndQuery::from_static("/cmti.market.v1.MarketService/GetSnapshot"),
            tonic_prost::ProstCodec::default(),
        )
        .await
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Arguments, ProbeError> {
    let mut readiness = None;
    let mut secret = None;
    let mut mode = None;
    let values = arguments.collect::<Vec<_>>();
    let mut index = 0;
    while index < values.len() {
        let value = values.get(index).ok_or(ProbeError::Arguments)?;
        index += 1;
        let argument = values.get(index).ok_or(ProbeError::Arguments)?;
        index += 1;
        match value.as_str() {
            "--readiness" => readiness = Some(PathBuf::from(argument)),
            "--secret" => secret = Some(PathBuf::from(argument)),
            "--mode" => {
                mode = Some(match argument.as_str() {
                    "healthy" => Mode::Healthy,
                    "unauthenticated" => Mode::Unauthenticated,
                    _ => return Err(ProbeError::Arguments),
                });
            }
            _ => return Err(ProbeError::Arguments),
        }
    }
    Ok(Arguments {
        readiness: readiness.ok_or(ProbeError::Arguments)?,
        secret: secret.ok_or(ProbeError::Arguments)?,
        mode: mode.ok_or(ProbeError::Arguments)?,
    })
}

fn validate_readiness(readiness: &Readiness) -> Result<Endpoint, ProbeError> {
    let endpoint = Endpoint::from_shared(readiness.endpoint.clone())
        .map_err(|_| ProbeError::ReadinessInvalid)?;
    let uri = endpoint.uri();
    let port = uri.port_u16().filter(|port| *port != 0);
    let canonical_endpoint = port.map(|port| format!("http://127.0.0.1:{port}"));
    if readiness.protocol_major != 1
        || readiness.protocol_minor != 0
        || uri.scheme_str() != Some("http")
        || uri.host() != Some("127.0.0.1")
        || canonical_endpoint.as_deref() != Some(readiness.endpoint.as_str())
        || readiness
            .expiry_unix_seconds
            .checked_sub(readiness.issued_unix_seconds)
            != Some(TOKEN_LIFETIME_SECONDS)
    {
        return Err(ProbeError::ReadinessInvalid);
    }
    Ok(endpoint)
}

fn descriptor(readiness: &Readiness) -> Result<SessionDescriptor, ProbeError> {
    let process_nonce = hex::decode(&readiness.process_nonce)
        .map_err(|_| ProbeError::ReadinessInvalid)?
        .try_into()
        .map_err(|_| ProbeError::ReadinessInvalid)?;
    let server_nonce = hex::decode(&readiness.server_nonce)
        .map_err(|_| ProbeError::ReadinessInvalid)?
        .try_into()
        .map_err(|_| ProbeError::ReadinessInvalid)?;
    let descriptor = SessionDescriptor::issue(
        readiness.protocol_major,
        readiness.protocol_minor,
        readiness.daemon_pid,
        process_nonce,
        server_nonce,
        readiness.issued_unix_seconds,
    )
    .map_err(|_| ProbeError::ReadinessInvalid)?;
    if descriptor.expiry_unix_seconds() != readiness.expiry_unix_seconds {
        return Err(ProbeError::ReadinessInvalid);
    }
    Ok(descriptor)
}

fn validate_snapshot(snapshot: &GetSnapshotResponse) -> Result<(), ProbeError> {
    if snapshot.source != "binance-fixture"
        || snapshot.symbol != "BTCUSDT"
        || snapshot.generation != 1
        || snapshot.sequence != 102
        || snapshot.best_bid != "60000.1"
        || snapshot.best_ask != "60000.2"
        || snapshot.health != SnapshotHealth::Healthy as i32
        || snapshot.event_unix_nanos != 1_700_000_000_200_000_000
        || snapshot.receive_unix_nanos != 1_700_000_000_200_000_000
        || snapshot.freshness_millis != 0
    {
        return Err(ProbeError::Snapshot);
    }
    Ok(())
}

#[derive(Debug, Error)]
enum ProbeError {
    #[error(
        "usage: foundation-runtime-probe --readiness PATH --secret PATH --mode healthy|unauthenticated"
    )]
    Arguments,
    #[error("readiness document could not be read")]
    ReadinessRead,
    #[error("readiness document is invalid")]
    ReadinessInvalid,
    #[error("session secret could not be read")]
    SecretRead,
    #[error("session secret is invalid")]
    SecretInvalid,
    #[error("local daemon connection failed")]
    Connect,
    #[error("local daemon probe exceeded five seconds")]
    Timeout,
    #[error("authenticated snapshot request failed")]
    HealthyRequest,
    #[error("snapshot did not match the foundation contract")]
    Snapshot,
    #[error("unauthenticated request returned unexpected status {0}")]
    UnexpectedAuthCode(Code),
    #[error("unauthenticated market request was accepted")]
    AuthenticationAccepted,
    #[error("probe output serialization failed")]
    Serialization,
}
