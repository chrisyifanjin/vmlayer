//! vmlayer-core: the contract every isolation backend implements.
//!
//! Step 0 of the project. This crate holds *only* the [`Backend`] trait, the
//! handle traits it returns, and the shared value types. There is no
//! implementation here yet. Firecracker, libkrun and Docker backends will each
//! be their own crate that depends on this one.
//!
//! Layering, bottom to top:
//!
//! ```text
//!   SDKs (Python / TS / Rust)      <- talk gRPC to vmlayerd
//!   vmlayerd                       <- resolves images, owns the sandbox table,
//!                                     picks a Backend, forwards calls
//!   Backend trait (this crate)     <- one impl per engine
//!   firecracker | libkrun | docker <- the engines
//! ```
//!
//! Two rules keep the trait honest:
//!
//! 1. A backend never sees an OCI reference. `vmlayerd` resolves images into a
//!    [`Rootfs`] the backend can consume directly. Image handling is one
//!    component, not N.
//! 2. Optional features are declared in [`Capabilities`] and refused with
//!    [`Error::Unsupported`]. A backend never silently degrades (e.g. Docker
//!    must not pretend a filesystem-only snapshot "includes memory").

pub mod backend;
pub mod error;
pub mod types;

pub use backend::{Backend, Process, Pty, SandboxHandle};
pub use error::{Error, Result};
pub use types::*;
