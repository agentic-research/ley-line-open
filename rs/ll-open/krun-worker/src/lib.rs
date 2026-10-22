//! The microVM worker (ADR-0044, bead `ley-line-open-395e43`): the one crate
//! that links the vendored libkrun.
//!
//! `leyline-runtime`'s libkrun backend supervises the `leyline-krun-worker`
//! binary this crate builds. The worker reads one [`ExecutionRequest`],
//! checks, plans, materializes and confines the run through
//! [`leyline_runtime::backends::libkrun::worker::prepare`], then builds the
//! VM through libkrun ([`api`]), attaches the workspace share ([`device`])
//! and enters it. Everything up to the VMM lives in `leyline-runtime`, which
//! stays free of libkrun and publishable; this crate is `publish = false`
//! because it links libkrun by path.
//!
//! [`ExecutionRequest`]: leyline_runtime::ExecutionRequest

pub mod api;
pub mod device;
pub mod worker;

#[cfg(test)]
mod device_tests;
