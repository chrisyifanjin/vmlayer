//! vmlayerd: the per-host daemon. Owns the sandbox table, volumes,
//! snapshots and process buffers; picks a backend; serves the `vmlayer.v1`
//! gRPC API. See ADR 0002 and 0007.

pub mod config;
pub mod grpc;
pub mod manager;
pub mod processes;
pub mod registry;
pub mod server;
pub mod store;
pub mod volumes;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
