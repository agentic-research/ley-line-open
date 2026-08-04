//! Shared execution/v1 adapter used by daemon transports.

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
}

impl ExecutionDaemonExt {
    pub fn new(handler: Arc<dyn ExecutionHandler>) -> Self {
        Self { handler }
    }
}

impl super::DaemonExt for ExecutionDaemonExt {
    fn execution_handler(&self) -> Option<Arc<dyn ExecutionHandler>> {
        Some(Arc::clone(&self.handler))
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
