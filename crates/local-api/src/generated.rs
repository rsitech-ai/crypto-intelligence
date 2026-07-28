//! Reproducibly generated protobuf and Tonic server bindings.

pub mod cmti {
    pub mod admin {
        pub mod v1 {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/generated/cmti.admin.v1.rs"
            ));
        }
    }

    pub mod common {
        pub mod v1 {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/generated/cmti.common.v1.rs"
            ));
        }
    }

    pub mod health {
        pub mod v1 {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/generated/cmti.health.v1.rs"
            ));
        }
    }

    pub mod market {
        pub mod v1 {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/generated/cmti.market.v1.rs"
            ));
        }
    }

    pub mod replay {
        pub mod v1 {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/generated/cmti.replay.v1.rs"
            ));
        }
    }

    pub mod risk {
        pub mod v1 {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/generated/cmti.risk.v1.rs"
            ));
        }
    }

    pub mod settings {
        pub mod v1 {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/generated/cmti.settings.v1.rs"
            ));
        }
    }
}

pub use cmti::admin::v1 as admin_v1;
pub use cmti::common::v1 as common_v1;
pub use cmti::health::v1 as health_v1;
pub use cmti::market::v1 as market_v1;
pub use cmti::replay::v1 as replay_v1;
pub use cmti::risk::v1 as risk_v1;
pub use cmti::settings::v1 as settings_v1;
