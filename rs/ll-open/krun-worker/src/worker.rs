//! The worker's run: `leyline-runtime` prepares it, this crate builds the VM
//! and enters it.

use std::io::{Read, Write};
use std::sync::Arc;

use leyline_runtime::backends::libkrun::share::RemoteFs;
use leyline_runtime::backends::libkrun::worker::{
    WORKSPACE_TAG, WorkerEvent, WorkerOptions, prepare, read_request,
};
use leyline_runtime::{ExecutionError, ExecutionRequest};

use crate::api::{LinkedKrunApi, PreparedVm, prepare_vm};
use crate::device::ShareFs;

/// Run the request read from `reader`, reporting readiness and failure as
/// JSON lines on `writer` (the worker's standard error).
pub fn execute_from_reader_with_events(
    options: WorkerOptions,
    reader: impl Read,
    mut writer: impl Write,
) -> Result<(), ExecutionError> {
    let request = read_request(reader)?;
    execute_with_ready(options, &request, |event| {
        serde_json::to_writer(&mut writer, event)
            .map_err(|error| ExecutionError::backend(format!("write worker event: {error}")))?;
        writer
            .write_all(b"\n")
            .map_err(|error| ExecutionError::backend(format!("write worker event: {error}")))?;
        writer
            .flush()
            .map_err(|error| ExecutionError::backend(format!("flush worker event: {error}")))
    })
}

/// Prepare the run (checked, planned, confined), build its VM with the
/// workspace share attached, report readiness through `on_ready`, and enter
/// the VM. A successful entry does not return: libkrun exits the process
/// with the guest's status.
pub fn execute_with_ready(
    options: WorkerOptions,
    request: &ExecutionRequest,
    on_ready: impl FnOnce(&WorkerEvent) -> Result<(), ExecutionError>,
) -> Result<(), ExecutionError> {
    let run = prepare(options, request)?;
    let vm: PreparedVm<'_> = prepare_vm(&LinkedKrunApi, &run.config)?;
    let ready = run.ready();
    if let Some(stream) = run.share {
        vm.add_virtiofs_backend(
            WORKSPACE_TAG,
            0,
            Arc::new(ShareFs::new(RemoteFs::new(stream))),
        )?;
    }
    on_ready(&ready)?;
    vm.enter()
}
