//! `lullaby wasm` — compile the scalar subset to a binary WebAssembly module.

use std::{fs, path::PathBuf};

use lullaby_diagnostics::{DiagnosticPhase, DiagnosticReport};
use lullaby_ir::{OptimizationConfig, emit_wasm_module, lower, optimize};

use crate::args::OutputMode;
use crate::compile::{SourceMode, compile};
use crate::diagnostics::{format_reports, ir_report, write_failure_report};

/// Compile the scalar-subset functions of a `.lby` source file to a binary
/// `.wasm` module. Validates and lowers to IR exactly as `compile` does, then
/// runs the WASM emitter. `--verbose` prints which functions compiled and which
/// were skipped (and why).
pub(crate) fn wasm_file(
    path: PathBuf,
    output: Option<PathBuf>,
    mode: OutputMode,
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

    // A program that declares actors is not eligible for the WebAssembly
    // backend — actors run on the interpreter. Skip cleanly with `L0338` rather
    // than attempting (and failing) to lower `spawn`/`tell`, so there is never a
    // miscompile.
    if !compiled.checked.program.actors.is_empty() {
        let report = DiagnosticReport::new(
            "L0338",
            DiagnosticPhase::Ir,
            "no functions were eligible for the WebAssembly scalar subset: this program uses actors, which run on the interpreter",
        )
        .with_source_path(compiled.path.display().to_string())
        .with_note(
            "actors (`actor`/`spawn`/`tell`) are not compiled to WebAssembly; run the program with `lullaby run` (the default backend)",
        );
        return Err(format_reports(&[report], mode, Some(&compiled.source)));
    }

    let module = lower(&compiled.checked).map_err(|error| {
        format_reports(
            &[ir_report(error, &compiled.path)],
            mode,
            Some(&compiled.source),
        )
    })?;
    // Inline small leaf helpers before WASM codegen (see the native path).
    let (module, _report) = optimize(&module, &OptimizationConfig::inlining());

    let artifact = match emit_wasm_module(&module) {
        Ok(artifact) => artifact,
        Err(error) => {
            // No function was eligible for the scalar subset: report L0338 and,
            // in verbose mode, list every skipped function with its reason.
            let mut report = DiagnosticReport::new(error.code, DiagnosticPhase::Ir, error.message)
                .with_source_path(compiled.path.display().to_string());
            report = report.with_note(
                "the WebAssembly backend compiles only scalar functions (i64/f64/bool/char/byte)",
            );
            let mut rendered = format_reports(&[report], mode, Some(&compiled.source));
            if mode == OutputMode::Verbose {
                for skip in &error.skipped {
                    rendered.push_str(&format!("\nskipped {}: {}", skip.name, skip.reason));
                }
            }
            return Err(rendered);
        }
    };

    let output = output.unwrap_or_else(|| compiled.path.with_extension("wasm"));
    if let Err(error) = fs::write(&output, &artifact.bytes) {
        return Err(format_reports(
            &[write_failure_report(&output, error)],
            mode,
            None,
        ));
    }

    if mode == OutputMode::Json {
        println!("{{\"status\":\"ok\",\"diagnostics\":[]}}");
    } else {
        println!("wasm: {}", output.display());
        if mode == OutputMode::Verbose {
            for name in &artifact.compiled {
                println!("compiled {name}");
            }
            for skip in &artifact.skipped {
                println!("skipped {}: {}", skip.name, skip.reason);
            }
        }
    }
    Ok(())
}
