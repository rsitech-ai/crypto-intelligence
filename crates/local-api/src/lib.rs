//! Authenticated, loopback-only gRPC contracts for the local runtime.

pub mod auth;
pub mod generated;
pub mod server;
pub mod session;

/// Checked-in, reproducibly generated protobuf and Tonic server bindings.
pub use generated as proto;
