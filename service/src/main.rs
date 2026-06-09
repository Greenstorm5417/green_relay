
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::process::ExitCode;

use green_relay::{RunError, create_admin, run};

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    let args: Vec<String> = std::env::args().collect();
    if let Some(command) = args.get(1) {
        return match command.as_str() {
            "create-admin" => create_admin_command(&runtime, &args),
            "help" | "-h" | "--help" => {
                print_usage();
                ExitCode::SUCCESS
            }
            other => {
                eprintln!("unknown command `{other}`");
                print_usage();
                ExitCode::FAILURE
            }
        };
    }

    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let RunError::Config(_) = &error
                && let Some(key) = error.config_key()
            {
                eprintln!("configuration key in error: {key}");
            }
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// Print top-level usage, including the admin bootstrap subcommand.
fn print_usage() {
    eprintln!("usage:");
    eprintln!("  green_relay                          run the service");
    eprintln!("  green_relay create-admin <user> <password>");
    eprintln!("                                             create or reset an admin user");
}

/// Handle `create-admin <username> <password>`: bootstrap or reset an admin.
fn create_admin_command(runtime: &tokio::runtime::Runtime, args: &[String]) -> ExitCode {
    let (Some(username), Some(password)) = (args.get(2), args.get(3)) else {
        eprintln!("usage: green_relay create-admin <username> <password>");
        return ExitCode::FAILURE;
    };

    match runtime.block_on(create_admin(username, password)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let RunError::Config(_) = &error
                && let Some(key) = error.config_key()
            {
                eprintln!("configuration key in error: {key}");
            }
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
