use std::io::{self, Write};
use std::process::ExitCode;

use leyline_krun_worker::worker::execute_from_reader_with_events;
use leyline_runtime::ExecutionError;
use leyline_runtime::backends::libkrun::worker::{
    LIFELINE_EXIT_STATUS, LIFELINE_FD_ARG, WorkerEvent, WorkerOptions,
};

fn main() -> ExitCode {
    let result = WorkerOptions::parse(std::env::args_os().skip(1)).and_then(|options| {
        // The lifeline first, before the request is read: from here on, the
        // end of the process that spawned this worker ends it, and the VM
        // it enters with it (bead ley-line-open-f963e6). A number that is
        // not an open descriptor is refused, not read as an end.
        leyline_runtime::lifeline::exit_when_ended(options.lifeline_fd, LIFELINE_EXIT_STATUS)
            .map_err(|error| {
                ExecutionError::invalid(format!(
                    "{LIFELINE_FD_ARG} {} is not an open descriptor: {error}",
                    options.lifeline_fd
                ))
            })?;
        execute_from_reader_with_events(options, io::stdin().lock(), io::stderr().lock())
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut stderr = io::stderr().lock();
            let _ = serde_json::to_writer(&mut stderr, &WorkerEvent::Failed { error });
            let _ = stderr.write_all(b"\n");
            ExitCode::FAILURE
        }
    }
}
