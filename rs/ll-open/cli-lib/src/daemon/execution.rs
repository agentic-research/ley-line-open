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
    fn start(&self, input: &Value) -> Result<String>;
    fn status(&self, input: &Value) -> Result<String>;
    fn inspect(&self, input: &Value) -> Result<String>;
    fn cancel(&self, input: &Value) -> Result<String>;
}

/// First-party handler backed by one shared LLO `ExecutionService`.
pub struct RuntimeExecutionHandler<B, R> {
    service: Arc<ExecutionService<B>>,
    policy: AuthorizationPolicy,
    resolver: Arc<R>,
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
        Self {
            service,
            policy,
            resolver,
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

    fn start(&self, input: &Value) -> Result<String> {
        Ok(transport::start_json(
            &self.service,
            &input.to_string(),
            &self.policy,
            self.resolver.as_ref(),
        )?)
    }

    fn status(&self, input: &Value) -> Result<String> {
        Ok(transport::status_json(&self.service, &input.to_string())?)
    }

    fn inspect(&self, input: &Value) -> Result<String> {
        Ok(transport::inspect_json(&self.service, &input.to_string())?)
    }

    fn cancel(&self, input: &Value) -> Result<String> {
        Ok(transport::cancel_json(&self.service, &input.to_string())?)
    }
}
