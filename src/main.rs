//! Thin binary entry point. All service logic lives in the library crate;
//! `main` builds the Tokio runtime, drives the library [`run`] entry point to
//! completion, and maps its outcome to a process exit code.
//!
//! On a configuration failure the offending key is written to standard error in
//! addition to the structured stdout record emitted inside `run` (Req 11.5),
//! and the process exits non-zero. Any other startup or shutdown failure also
//! exits non-zero (Req 11.3).

use std::process::ExitCode;

use sms_micro_service::{RunError, run};

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Name the offending configuration key explicitly on stderr so the
            // operator sees it even outside the structured log (Req 11.5).
            if let RunError::Config(_) = &error {
                if let Some(key) = error.config_key() {
                    eprintln!("configuration key in error: {key}");
                }
            }
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
