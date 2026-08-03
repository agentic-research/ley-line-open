//! Capability-resolved execution lifecycle and isolation backends.
//!
//! Product policy is resolved before this crate is called. This crate accepts
//! content identities and guest-relative names; backend-only host paths are
//! introduced behind trusted resolver boundaries.

pub mod authorization;
pub mod backends;
mod error;
mod model;
mod service;
pub mod transport;

pub use error::{ErrorCode, ExecutionError};
pub use model::{
    BackendCapabilities, BackendClass, BackendRun, DigestRef, ExecutionRequest, ResourceLimits,
    RunRecord, RunState,
};
pub use service::{Backend, ExecutionResolver, ExecutionService};
