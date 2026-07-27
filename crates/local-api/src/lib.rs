//! Authenticated, loopback-only gRPC contracts for the local runtime.

pub mod auth;
pub mod server;
pub mod session;

/// Generated protobuf and Tonic bindings for the exact Task 3 packages.
pub mod proto {
    pub mod cmti {
        pub mod common {
            pub mod v1 {
                tonic::include_proto!("cmti.common.v1");
            }
        }

        pub mod health {
            pub mod v1 {
                tonic::include_proto!("cmti.health.v1");
            }
        }

        pub mod market {
            pub mod v1 {
                tonic::include_proto!("cmti.market.v1");
            }
        }
    }

    pub use cmti::common::v1 as common_v1;
    pub use cmti::health::v1 as health_v1;
    pub use cmti::market::v1 as market_v1;
}
