//! Process bookkeeping shared by the daemon (per sandbox) and the guest agent
//! (per VM): a table that adopts a [`vmlayer_core::Process`], buffers its
//! output for re-attach, enforces timeouts, and the glue that turns table
//! events into `vmlayer.v1` gRPC messages.

pub mod grpc;
pub mod table;

pub use table::{Entry, Event, ProcessTable};
