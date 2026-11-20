//! Prototype transport registration and recovery (ADR-017), not application membership.
pub mod client;
pub mod nat;
pub mod runtime;
pub mod server;
pub mod wire;

use std::time::Duration;

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
pub const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(6);
pub const CHECK_INTERVAL: Duration = Duration::from_millis(250);
pub const RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);
pub const RETRY_SECONDS: [u64; 6] = [0, 1, 3, 7, 15, 23];
