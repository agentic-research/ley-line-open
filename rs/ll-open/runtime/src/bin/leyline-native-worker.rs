use std::io::{self, Write};
use std::process::ExitCode;

use leyline_runtime::backends::native::{
    WorkerEvent, WorkerOptions, end_with_lifeline, execute_request_with_events, read_worker_request,
};

fn main() -> ExitCode {
    let result = WorkerOptions::parse(std::env::args_os().skip(1)).and_then(|options| {
        // The request first: the backend's lifeline is this worker's standard
        // input, which carries the request and nothing after it. A
        // supervisor gone before the request arrives ends the read too.
        let request = read_worker_request(io::stdin().lock())?;
        // From here on, the end of the process that spawned this worker ends
        // it and its workload (bead ley-line-open-62e3b1). A descriptor that
        // cannot be a lifeline is refused, not read as an end.
        end_with_lifeline(options.lifeline_fd)?;
        execute_request_with_events(options, &request, io::stderr().lock())
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
