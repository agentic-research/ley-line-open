use std::io::{self, Write};
use std::process::ExitCode;

use leyline_runtime::backends::native::{
    WorkerEvent, WorkerOptions, execute_from_reader_with_events,
};

fn main() -> ExitCode {
    let result = WorkerOptions::parse(std::env::args_os().skip(1)).and_then(|options| {
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
