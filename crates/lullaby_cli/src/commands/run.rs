//! `lullaby run` — execute a `.lby` source program on a chosen interpreter tier,
//! or replay a precompiled `.lbc` bytecode artifact.

use std::{fs, path::PathBuf};

use lullaby_ir::{
    BYTECODE_ARTIFACT_EXTENSION, decode_bytecode_artifact, lower, lower_to_bytecode,
    run_bytecode_main, run_bytecode_main_with_args, run_main_with_args as run_ir_main_with_args,
};
use lullaby_runtime::{Value, run_main_with_args};

use crate::args::{Backend, OptimizationMode, OutputMode};
use crate::compile::{SourceMode, compile, optimize_module};
use crate::diagnostics::{
    bytecode_report, format_reports, ir_report, read_failure_report, runtime_report,
};

pub(crate) fn run_file(
    path: PathBuf,
    mode: OutputMode,
    backend: Backend,
    optimization: OptimizationMode,
    program_args: Vec<String>,
) -> Result<(), String> {
    if path.extension().and_then(|value| value.to_str()) == Some(BYTECODE_ARTIFACT_EXTENSION) {
        if backend != Backend::Ast || optimization != OptimizationMode::None {
            return Err("usage: lullaby run [--verbose|--format json] <file.lbc>".to_string());
        }
        return run_bytecode_artifact(path, mode);
    }

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

    let result = match backend {
        Backend::Ast => run_main_with_args(&compiled.checked.program, program_args),
        Backend::Ir => {
            let module = lower(&compiled.checked).map_err(|error| {
                format_reports(
                    &[ir_report(error, &compiled.path)],
                    mode,
                    Some(&compiled.source),
                )
            })?;
            let module = optimize_module(module, optimization);
            run_ir_main_with_args(&module, program_args)
        }
        Backend::Bytecode => {
            let module = lower(&compiled.checked).map_err(|error| {
                format_reports(
                    &[ir_report(error, &compiled.path)],
                    mode,
                    Some(&compiled.source),
                )
            })?;
            let module = optimize_module(module, optimization);
            let bytecode = lower_to_bytecode(&module);
            run_bytecode_main_with_args(&bytecode, program_args)
        }
    };

    match result {
        Ok(value) => {
            if mode == OutputMode::Json {
                println!("{{\"status\":\"ok\",\"diagnostics\":[]}}");
            } else if value != Value::Void {
                println!("{value}");
            }
            Ok(())
        }
        Err(error) => {
            let report = runtime_report(error, &compiled.path);
            Err(format_reports(&[report], mode, Some(&compiled.source)))
        }
    }
}

fn run_bytecode_artifact(path: PathBuf, mode: OutputMode) -> Result<(), String> {
    let contents = fs::read_to_string(&path)
        .map_err(|error| format_reports(&[read_failure_report(&path, error)], mode, None))?;
    let artifact = decode_bytecode_artifact(&contents)
        .map_err(|error| format_reports(&[bytecode_report(error, &path)], mode, None))?;

    match run_bytecode_main(&artifact.module) {
        Ok(value) => {
            if mode == OutputMode::Json {
                println!("{{\"status\":\"ok\",\"diagnostics\":[]}}");
            } else if value != Value::Void {
                println!("{value}");
            }
            Ok(())
        }
        Err(error) => {
            let report = runtime_report(error, &path);
            Err(format_reports(&[report], mode, None))
        }
    }
}
