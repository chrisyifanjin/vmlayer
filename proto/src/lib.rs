//! Generated code for the `vmlayer.v1` gRPC API. The `.proto` files next to
//! this crate are the contract; nothing here is hand-written.

#[allow(clippy::all, missing_docs)]
pub mod v1 {
    tonic::include_proto!("vmlayer.v1");
}

/// What the guest agent serves over vsock, in addition to `v1::Processes`
/// and `v1::Files`.
#[allow(clippy::all, missing_docs)]
pub mod guest {
    pub mod v1 {
        tonic::include_proto!("vmlayer.guest.v1");
    }
}

pub use v1::*;
