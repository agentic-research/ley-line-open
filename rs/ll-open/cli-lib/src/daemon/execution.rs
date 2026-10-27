//! Shared execution/v1 adapter used by daemon transports.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use leyline_runtime::{
    Backend, ExecutionResolver, ExecutionService, authorization::AuthorizationPolicy, transport,
};

/// Transport-neutral execution operations. UDS and MCP call these methods;
/// neither transport owns policy or lifecycle state.
pub trait ExecutionHandler: Send + Sync {
    fn capabilities(&self) -> Result<String>;
    fn provision(&self, input: &Value) -> Result<String>;
    fn start(&self, input: &Value) -> Result<String>;
    fn status(&self, input: &Value) -> Result<String>;
    fn inspect(&self, input: &Value) -> Result<String>;
    fn collect(&self, input: &Value) -> Result<String>;
    fn cleanup(&self, input: &Value) -> Result<String>;
    fn cancel(&self, input: &Value) -> Result<String>;
}

/// Daemon extension that exposes one trusted execution/v1 handler.
///
/// The handler is constructed by the embedding application, where the host
/// capability policy, CAS resolver, and backend configuration are available.
/// Callers pass only signed execution intent over the daemon protocol; this
/// extension never accepts host paths from a request.
pub struct ExecutionDaemonExt {
    handler: Arc<dyn ExecutionHandler>,
    context: Option<super::ContextSlot>,
}

impl ExecutionDaemonExt {
    pub fn new(handler: Arc<dyn ExecutionHandler>) -> Self {
        Self {
            handler,
            context: None,
        }
    }

    /// Publish the daemon's context to `slot` once it exists: the embedder's
    /// workspace-share provider serves runs from it (ADR-0044 V5).
    pub fn with_context_slot(mut self, slot: super::ContextSlot) -> Self {
        self.context = Some(slot);
        self
    }
}

impl super::DaemonExt for ExecutionDaemonExt {
    fn execution_handler(&self) -> Option<Arc<dyn ExecutionHandler>> {
        Some(Arc::clone(&self.handler))
    }

    fn on_context(&self, ctx: &Arc<super::DaemonContext>) {
        if let Some(slot) = &self.context {
            // One daemon, one context: a second call keeps the first.
            let _ = slot.set(Arc::clone(ctx));
        }
    }
}

/// The evidence-verifier flags of `leyline execution-daemon`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvidenceVerifierConfig {
    /// `--allow-unverified-evidence`: the metadata-only fixture downgrade.
    pub allow_unverified_evidence: bool,
    /// `--trust-key FILE`, repeatable: operator-supplied Ed25519 trust keys.
    pub trust_keys: Vec<PathBuf>,
    /// `--evidence-store DIR`: the CAS blob store holding DSSE evidence.
    pub evidence_store: Option<PathBuf>,
}

/// The verifier an [`EvidenceVerifierConfig`] selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceVerifierMode {
    /// No verifier configured: every start is refused (`RejectUnverifiedEvidence`).
    RejectAll,
    /// `--allow-unverified-evidence`: authenticates nothing.
    MetadataOnly,
    /// `CasDsseEvidenceVerifier` over the evidence store, trusting the keys
    /// whose ADR-012 canonical kids are listed.
    CasDsse {
        evidence_store: PathBuf,
        trusted_kids: Vec<String>,
    },
}

impl EvidenceVerifierConfig {
    /// Build the verifier, failing closed.
    ///
    /// - `--trust-key` and `--evidence-store` together install
    ///   `CasDsseEvidenceVerifier`. Either one alone, an unreadable or
    ///   malformed key, a repeated key, or a missing store directory is an
    ///   error: the daemon refuses to start rather than run with a verifier
    ///   other than the one the operator asked for.
    /// - `--allow-unverified-evidence` with either of them is an error: a
    ///   downgrade never coexists with a real verifier, so a verifier that
    ///   ran and refused is terminal.
    /// - No flags: every start is refused.
    pub fn build(
        &self,
    ) -> Result<(
        Arc<dyn leyline_runtime::EvidenceVerifier>,
        EvidenceVerifierMode,
    )> {
        let real = !self.trust_keys.is_empty() || self.evidence_store.is_some();
        if self.allow_unverified_evidence && real {
            anyhow::bail!(
                "--allow-unverified-evidence cannot be combined with --trust-key or --evidence-store"
            );
        }
        if self.allow_unverified_evidence {
            log::warn!(
                "execution daemon: --allow-unverified-evidence — grants are NOT authenticated"
            );
            return Ok((
                Arc::new(leyline_runtime::MetadataOnlyEvidenceVerifier),
                EvidenceVerifierMode::MetadataOnly,
            ));
        }
        if !real {
            log::warn!(
                "execution daemon: no --trust-key/--evidence-store configured — every start is refused"
            );
            return Ok((
                Arc::new(leyline_runtime::RejectUnverifiedEvidence),
                EvidenceVerifierMode::RejectAll,
            ));
        }
        let Some(evidence_store) = &self.evidence_store else {
            anyhow::bail!("--trust-key requires --evidence-store");
        };
        if self.trust_keys.is_empty() {
            anyhow::bail!("--evidence-store requires at least one --trust-key");
        }
        let verifier =
            leyline_runtime::cas_evidence::operator_verifier(evidence_store, &self.trust_keys)?;
        let trusted_kids = verifier.trusted_kids();
        log::info!(
            "execution daemon: CAS DSSE evidence verifier over {} trusting kids {:?}",
            evidence_store.display(),
            trusted_kids
        );
        Ok((
            Arc::new(verifier),
            EvidenceVerifierMode::CasDsse {
                evidence_store: evidence_store.clone(),
                trusted_kids,
            },
        ))
    }
}

/// First-party handler backed by one shared LLO `ExecutionService`.
pub struct RuntimeExecutionHandler<B, R> {
    service: Arc<ExecutionService<B>>,
    policy: AuthorizationPolicy,
    resolver: Arc<R>,
    verifier: Arc<dyn leyline_runtime::EvidenceVerifier>,
}

impl<B, R> RuntimeExecutionHandler<B, R>
where
    B: Backend,
    R: ExecutionResolver,
{
    pub fn new(
        service: Arc<ExecutionService<B>>,
        policy: AuthorizationPolicy,
        resolver: Arc<R>,
    ) -> Self {
        Self::new_with_verifier(
            service,
            policy,
            resolver,
            Arc::new(leyline_runtime::RejectUnverifiedEvidence),
        )
    }

    pub fn new_with_verifier(
        service: Arc<ExecutionService<B>>,
        policy: AuthorizationPolicy,
        resolver: Arc<R>,
        verifier: Arc<dyn leyline_runtime::EvidenceVerifier>,
    ) -> Self {
        Self {
            service,
            policy,
            resolver,
            verifier,
        }
    }
}

impl<B, R> ExecutionHandler for RuntimeExecutionHandler<B, R>
where
    B: Backend,
    R: ExecutionResolver,
{
    fn capabilities(&self) -> Result<String> {
        Ok(transport::capabilities_json(&self.service)?)
    }

    fn provision(&self, input: &Value) -> Result<String> {
        Ok(transport::provision_json(
            &self.service,
            &input.to_string(),
        )?)
    }

    fn start(&self, input: &Value) -> Result<String> {
        Ok(transport::start_json_with_verifier(
            &self.service,
            &input.to_string(),
            &self.policy,
            self.resolver.as_ref(),
            self.verifier.as_ref(),
        )?)
    }

    fn status(&self, input: &Value) -> Result<String> {
        Ok(transport::status_json(&self.service, &input.to_string())?)
    }

    fn inspect(&self, input: &Value) -> Result<String> {
        Ok(transport::inspect_json(&self.service, &input.to_string())?)
    }

    fn collect(&self, input: &Value) -> Result<String> {
        Ok(transport::collect_json(&self.service, &input.to_string())?)
    }

    fn cleanup(&self, input: &Value) -> Result<String> {
        Ok(transport::cleanup_json(&self.service, &input.to_string())?)
    }

    fn cancel(&self, input: &Value) -> Result<String> {
        Ok(transport::cancel_json(&self.service, &input.to_string())?)
    }
}
