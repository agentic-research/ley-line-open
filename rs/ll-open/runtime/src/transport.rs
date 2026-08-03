//! JSON adapters over the generated execution/v1 Cap'n Proto surface.
//!
//! These functions are intentionally small: decode the generated input,
//! invoke [`ExecutionService`], and encode the generated output. UDS, CLI,
//! and MCP callers can therefore share this adapter without duplicating
//! authorization or lifecycle state.

use capnp::message::{Builder, HeapAllocator};
use leyline_public_schema::execution_capnp;

use crate::authorization::AuthorizationPolicy;
use crate::{ExecutionError, ExecutionResolver, ExecutionService, RunState};

pub fn capabilities_json<B: crate::Backend>(
    service: &ExecutionService<B>,
) -> Result<String, ExecutionError> {
    let mut message = Builder::new_default();
    let mut output = message.init_root::<execution_capnp::capabilities_output::Builder<'_>>();
    let capabilities = service.capabilities();
    let mut entries = output.reborrow().init_capabilities(3);
    entries.reborrow().get(0).set_name("cloister/execution/v1");
    entries.reborrow().get(0).set_version("v1");
    entries.reborrow().get(1).set_name("backend/microvm");
    entries
        .reborrow()
        .get(1)
        .set_version(if capabilities.available {
            &capabilities.backend_id
        } else {
            "unavailable"
        });
    entries.reborrow().get(2).set_name("backend/native");
    entries.get(2).set_version("not-configured");
    encode_capabilities(message)
}

pub fn start_json<B: crate::Backend, R: ExecutionResolver>(
    service: &ExecutionService<B>,
    input_json: &str,
    policy: &AuthorizationPolicy,
    resolver: &R,
) -> Result<String, ExecutionError> {
    let mut input_message = Builder::new_default();
    let input = input_message.init_root::<execution_capnp::start_input::Builder<'_>>();
    capnp_json::from_json(input_json, input).map_err(|error| {
        ExecutionError::invalid(format!("invalid execution start input: {error}"))
    })?;

    let spec = input_message
        .get_root_as_reader::<execution_capnp::start_input::Reader<'_>>()
        .map_err(|error| ExecutionError::invalid(format!("invalid start input root: {error}")))?
        .get_spec()
        .map_err(|error| ExecutionError::invalid(format!("invalid start spec: {error}")))?;
    let grant = input_message
        .get_root_as_reader::<execution_capnp::start_input::Reader<'_>>()
        .map_err(|error| ExecutionError::invalid(format!("invalid start input root: {error}")))?
        .get_grant()
        .map_err(|error| ExecutionError::invalid(format!("invalid start grant: {error}")))?;
    let spec_bytes = serialize_root::<execution_capnp::run_spec::Owned>(spec)?;
    let grant_bytes = serialize_root::<execution_capnp::run_grant::Owned>(grant)?;
    let record = service.start_authorized(&spec_bytes, &grant_bytes, policy, resolver)?;

    let mut output_message = Builder::new_default();
    let mut output = output_message.init_root::<execution_capnp::start_output::Builder<'_>>();
    output.set_run_id(&record.run_id);
    output.set_state(execution_capnp::RunState::Accepted);
    encode_start(output_message)
}

pub fn status_json<B: crate::Backend>(
    service: &ExecutionService<B>,
    input_json: &str,
) -> Result<String, ExecutionError> {
    let mut input_message = Builder::new_default();
    let input = input_message.init_root::<execution_capnp::status_input::Builder<'_>>();
    capnp_json::from_json(input_json, input).map_err(|error| {
        ExecutionError::invalid(format!("invalid execution status input: {error}"))
    })?;
    let input = input_message
        .get_root_as_reader::<execution_capnp::status_input::Reader<'_>>()
        .map_err(|error| ExecutionError::invalid(format!("invalid status input root: {error}")))?;
    let run_id = input
        .get_run_id()
        .map_err(|error| ExecutionError::invalid(format!("invalid status runId: {error}")))?
        .to_str()
        .map_err(|error| ExecutionError::invalid(format!("status runId is not UTF-8: {error}")))?;

    let mut output_message = Builder::new_default();
    let mut output = output_message.init_root::<execution_capnp::status_output::Builder<'_>>();
    let capabilities = service.capabilities();
    output.set_provisioned(capabilities.available);
    output.set_backend(&capabilities.backend_id);
    if !run_id.is_empty() {
        let record = service
            .status(run_id)?
            .ok_or_else(|| ExecutionError::invalid("run_id not found"))?;
        output.set_run_id(&record.run_id);
        output.set_state(to_schema_state(record.state));
    } else {
        output.set_state(execution_capnp::RunState::Accepted);
    }
    encode_status(output_message)
}

pub fn cancel_json<B: crate::Backend>(
    service: &ExecutionService<B>,
    input_json: &str,
) -> Result<String, ExecutionError> {
    let mut input_message = Builder::new_default();
    let input = input_message.init_root::<execution_capnp::cancel_input::Builder<'_>>();
    capnp_json::from_json(input_json, input).map_err(|error| {
        ExecutionError::invalid(format!("invalid execution cancel input: {error}"))
    })?;
    let input = input_message
        .get_root_as_reader::<execution_capnp::cancel_input::Reader<'_>>()
        .map_err(|error| ExecutionError::invalid(format!("invalid cancel input root: {error}")))?;
    let run_id = input
        .get_run_id()
        .map_err(|error| ExecutionError::invalid(format!("invalid cancel runId: {error}")))?
        .to_str()
        .map_err(|error| ExecutionError::invalid(format!("cancel runId is not UTF-8: {error}")))?;
    let record = service.cancel(run_id)?;
    let mut output_message = Builder::new_default();
    let mut output = output_message.init_root::<execution_capnp::cancel_output::Builder<'_>>();
    output.set_run_id(&record.run_id);
    output.set_state(to_schema_state(record.state));
    encode_cancel(output_message)
}

pub fn inspect_json<B: crate::Backend>(
    service: &ExecutionService<B>,
    input_json: &str,
) -> Result<String, ExecutionError> {
    let mut input_message = Builder::new_default();
    let input = input_message.init_root::<execution_capnp::inspect_input::Builder<'_>>();
    capnp_json::from_json(input_json, input).map_err(|error| {
        ExecutionError::invalid(format!("invalid execution inspect input: {error}"))
    })?;
    let input = input_message
        .get_root_as_reader::<execution_capnp::inspect_input::Reader<'_>>()
        .map_err(|error| ExecutionError::invalid(format!("invalid inspect input root: {error}")))?;
    let run_id = input
        .get_run_id()
        .map_err(|error| ExecutionError::invalid(format!("invalid inspect runId: {error}")))?
        .to_str()
        .map_err(|error| ExecutionError::invalid(format!("inspect runId is not UTF-8: {error}")))?;
    let after_sequence = input.get_after_sequence();
    let inspection = service.inspect(run_id, after_sequence)?;

    let mut output_message = Builder::new_default();
    let mut output = output_message.init_root::<execution_capnp::inspect_output::Builder<'_>>();
    output.set_run_id(&inspection.run_id);
    output.set_state(to_schema_state(inspection.state));
    let mut events = output
        .reborrow()
        .init_events(inspection.events.len() as u32);
    for (index, event) in inspection.events.iter().enumerate() {
        let mut entry = events.reborrow().get(index as u32);
        entry.set_sequence(event.sequence);
        entry.set_run_id(&inspection.run_id);
        entry.set_state(to_schema_state(event.state));
        entry.set_timestamp_ms(event.timestamp_ms);
    }
    encode_inspect(output_message)
}

fn serialize_root<T: capnp::traits::Owned>(
    reader: T::Reader<'_>,
) -> Result<Vec<u8>, ExecutionError> {
    let mut message = Builder::new_default();
    message
        .set_root(reader)
        .map_err(|error| ExecutionError::invalid(format!("copy schema message: {error}")))?;
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message)
        .map_err(|error| ExecutionError::invalid(format!("serialize schema message: {error}")))?;
    Ok(bytes)
}

fn encode_capabilities(message: Builder<HeapAllocator>) -> Result<String, ExecutionError> {
    let reader = message
        .get_root_as_reader::<execution_capnp::capabilities_output::Reader<'_>>()
        .map_err(|error| {
            ExecutionError::internal(format!("read capabilities response: {error}"))
        })?;
    capnp_json::to_json(reader)
        .map_err(|error| ExecutionError::internal(format!("encode capabilities response: {error}")))
}

fn encode_start(message: Builder<HeapAllocator>) -> Result<String, ExecutionError> {
    let reader = message
        .get_root_as_reader::<execution_capnp::start_output::Reader<'_>>()
        .map_err(|error| ExecutionError::internal(format!("read start response: {error}")))?;
    capnp_json::to_json(reader)
        .map_err(|error| ExecutionError::internal(format!("encode start response: {error}")))
}

fn encode_status(message: Builder<HeapAllocator>) -> Result<String, ExecutionError> {
    let reader = message
        .get_root_as_reader::<execution_capnp::status_output::Reader<'_>>()
        .map_err(|error| ExecutionError::internal(format!("read status response: {error}")))?;
    capnp_json::to_json(reader)
        .map_err(|error| ExecutionError::internal(format!("encode status response: {error}")))
}

fn encode_cancel(message: Builder<HeapAllocator>) -> Result<String, ExecutionError> {
    let reader = message
        .get_root_as_reader::<execution_capnp::cancel_output::Reader<'_>>()
        .map_err(|error| ExecutionError::internal(format!("read cancel response: {error}")))?;
    capnp_json::to_json(reader)
        .map_err(|error| ExecutionError::internal(format!("encode execution response: {error}")))
}

fn encode_inspect(message: Builder<HeapAllocator>) -> Result<String, ExecutionError> {
    let reader = message
        .get_root_as_reader::<execution_capnp::inspect_output::Reader<'_>>()
        .map_err(|error| ExecutionError::internal(format!("read inspect response: {error}")))?;
    capnp_json::to_json(reader)
        .map_err(|error| ExecutionError::internal(format!("encode inspect response: {error}")))
}

fn to_schema_state(state: RunState) -> execution_capnp::RunState {
    match state {
        RunState::Accepted => execution_capnp::RunState::Accepted,
        RunState::Provisioning => execution_capnp::RunState::Provisioning,
        RunState::Ready => execution_capnp::RunState::Ready,
        RunState::Running => execution_capnp::RunState::Running,
        RunState::Succeeded => execution_capnp::RunState::Succeeded,
        RunState::Failed => execution_capnp::RunState::Failed,
        RunState::Cancelled => execution_capnp::RunState::Cancelled,
        RunState::Cleaning => execution_capnp::RunState::Cleaning,
        RunState::Cleaned => execution_capnp::RunState::Cleaned,
    }
}
