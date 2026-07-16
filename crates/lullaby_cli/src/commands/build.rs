//! `lullaby check` and `lullaby build` / `lullaby compile`.

use std::{fs, path::PathBuf};

use lullaby_ir::{BYTECODE_ARTIFACT_EXTENSION, encode_bytecode_artifact, lower, lower_to_bytecode};

use crate::args::{OptimizationMode, OutputMode};
use crate::compile::{SourceMode, compile, optimize_module};
use crate::diagnostics::{bytecode_report, format_reports, ir_report, write_failure_report};

pub(crate) fn check(path: PathBuf, mode: OutputMode) -> Result<(), String> {
    match compile(&path, SourceMode::Library) {
        Ok(compiled) => {
            if mode == OutputMode::Json {
                println!("{{\"status\":\"ok\",\"diagnostics\":[]}}");
            } else {
                println!("ok: {}", compiled.path.display());
            }
            Ok(())
        }
        Err(failure) => Err(format_reports(
            &failure.reports,
            mode,
            failure.source.as_deref(),
        )),
    }
}

pub(crate) fn compile_file(
    path: PathBuf,
    output: Option<PathBuf>,
    mode: OutputMode,
    optimization: OptimizationMode,
) -> Result<(), String> {
    let compiled = match compile(&path, SourceMode::Executable) {
        Ok(compiled) => compiled,
        Err(failure) => {
            return Err(format_reports(
                &failure.reports,
                mode,
                failure.source.as_deref(),
            ));
        }
    };

    let module = lower(&compiled.checked).map_err(|error| {
        format_reports(
            &[ir_report(error, &compiled.path)],
            mode,
            Some(&compiled.source),
        )
    })?;
    let module = optimize_module(module, optimization);
    let bytecode = lower_to_bytecode(&module);
    let artifact = encode_bytecode_artifact(&bytecode).map_err(|error| {
        format_reports(
            &[bytecode_report(error, &compiled.path)],
            mode,
            Some(&compiled.source),
        )
    })?;

    let output =
        output.unwrap_or_else(|| compiled.path.with_extension(BYTECODE_ARTIFACT_EXTENSION));
    if let Err(error) = fs::write(&output, artifact) {
        return Err(format_reports(
            &[write_failure_report(&output, error)],
            mode,
            None,
        ));
    }

    if mode == OutputMode::Json {
        println!("{{\"status\":\"ok\",\"diagnostics\":[]}}");
    } else {
        println!("compiled: {}", output.display());
    }
    Ok(())
}
