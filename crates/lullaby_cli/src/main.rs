//! The `lullaby` command-line driver.
//!
//! This file is the dispatcher and nothing else: it parses `argv` into an
//! [`Invocation`](args::Invocation) and routes it to the matching handler in
//! [`commands`]. The work lives in the siblings:
//!
//! * [`args`] — argument parsing, flag/mode enums, per-command usage strings.
//! * [`compile`] — target resolution, module loading, semantic validation.
//! * [`diagnostics`] — per-phase `DiagnosticReport` construction and rendering.
//! * [`help`] — the `--help` text and the `--version` string.
//! * [`commands`] — one module per subcommand.

mod args;
mod commands;
mod compile;
mod diagnostics;
mod help;

use std::{env, process::ExitCode};

use args::{CommandName, parse_invocation};
use help::{display_version, print_help};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            // An empty message is a bare non-zero exit request (e.g. `lullaby
            // test` after it already printed per-test results); don't emit a
            // spurious blank stderr line.
            if !message.is_empty() {
                eprintln!("{message}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let Some(invocation) = parse_invocation(env::args().skip(1).collect())? else {
        print_help();
        return Ok(());
    };

    match invocation.command {
        CommandName::Check => commands::build::check(invocation.path, invocation.mode),
        CommandName::Build | CommandName::Compile => commands::build::compile_file(
            invocation.path,
            invocation.output,
            invocation.mode,
            invocation.optimization,
        ),
        CommandName::Docs => commands::project::docs(),
        CommandName::Examples => commands::project::examples(),
        CommandName::New => commands::project::new_project(invocation.path),
        CommandName::Fmt => commands::fmt::fmt_file(invocation.path, invocation.fmt_mode),
        CommandName::Inspect => {
            commands::inspect::inspect_bytecode_artifact(invocation.path, invocation.mode)
        }
        CommandName::Run => commands::run::run_file(
            invocation.path,
            invocation.mode,
            invocation.backend,
            invocation.optimization,
            invocation.program_args,
        ),
        CommandName::Test => {
            commands::test::test_file(invocation.path, invocation.mode, invocation.filter)
        }
        CommandName::Wasm => {
            commands::wasm::wasm_file(invocation.path, invocation.output, invocation.mode)
        }
        CommandName::Native => commands::native::native_file(
            invocation.path,
            invocation.output,
            invocation.mode,
            invocation.freestanding,
            invocation.debug,
            invocation.native_target,
            invocation.fast_math,
        ),
        CommandName::Lsp => commands::project::lsp(),
        CommandName::Version => {
            println!("lullaby {}", display_version());
            Ok(())
        }
        CommandName::Help => {
            print_help();
            Ok(())
        }
    }
}
