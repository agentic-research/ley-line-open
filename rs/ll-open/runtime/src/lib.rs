//! Capability-resolved execution lifecycle and isolation backends.
//!
//! Product policy is resolved before this crate is called. This crate accepts
//! content identities and guest-relative names; backend-only host paths are
//! introduced behind trusted resolver boundaries.

pub mod authorization;
pub mod backends;
mod catalog;
pub mod confinement;
mod error;
mod model;
mod service;
pub mod transport;

pub use authorization::{
    ArtifactIdentity, AuthorizedExecution, CasDsseEvidenceVerifier, EvidenceBinding, EvidenceField,
    EvidenceRef, EvidenceStore, EvidenceVerifier, GrantSignature, MetadataOnlyEvidenceVerifier,
    RejectUnverifiedEvidence, SchemaIntent, SchemaLimits, SignedGrant, WorkspaceInput,
};
pub use catalog::{CatalogBuilder, CatalogResolver};
pub use error::{ErrorCode, ExecutionError};
pub use model::{
    BackendCapabilities, BackendClass, BackendRun, BackendRunStatus, CeilingMechanism, DigestRef,
    EnforcedCeilings, ExecutionRequest, ReceiptContext, ResourceLimits, RunEventRecord,
    RunInspection, RunReceiptData, RunRecord, RunState,
};
pub use service::{Backend, ExecutionResolver, ExecutionService};
