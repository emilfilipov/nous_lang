//! Diagnostic construction and rendering for the CLI.
//!
//! Every compiler phase reports through `lullaby_diagnostics::DiagnosticReport`;
//! this module adapts each phase's own error type into one, and renders a batch
//! of them in whichever [`OutputMode`](crate::args::OutputMode) the invocation
//! selected.

use std::path::Path;

use lullaby_diagnostics::{
    DiagnosticPhase, DiagnosticReport, render_concise, render_json, render_verbose,
};
use lullaby_ir::BytecodeArtifactError;
use lullaby_lexer::Diagnostic;
use lullaby_runtime::{ErrorCategory, RuntimeError};

use crate::args::OutputMode;

pub(crate) fn frontend_report(
    diagnostic: Diagnostic,
    phase: DiagnosticPhase,
    path: &Path,
) -> DiagnosticReport {
    DiagnosticReport::new(diagnostic.code, phase, diagnostic.message)
        .with_source_path(path.display().to_string())
        .with_span(diagnostic.span)
}

pub(crate) fn runtime_report(error: RuntimeError, path: &Path) -> DiagnosticReport {
    let phase = match error.category {
        ErrorCategory::Runtime => DiagnosticPhase::Runtime,
        ErrorCategory::Resource => DiagnosticPhase::Resource,
    };
    let mut report = DiagnosticReport::new(error.code, phase, error.message)
        .with_source_path(path.display().to_string())
        .with_traceback(error.traceback);
    if let Some(span) = error.span {
        report = report.with_span(span);
    }
    if let Some(function) = error.function {
        report = report.with_function(function);
    }
    report
}

pub(crate) fn ir_report(error: lullaby_ir::IrLoweringError, path: &Path) -> DiagnosticReport {
    let code = error.code.unwrap_or("L0501");
    let mut report = DiagnosticReport::new(code, DiagnosticPhase::Ir, error.message)
        .with_source_path(path.display().to_string());
    if let Some(span) = error.span {
        report = report.with_span(span);
    }
    report
}

pub(crate) fn bytecode_report(error: BytecodeArtifactError, path: &Path) -> DiagnosticReport {
    DiagnosticReport::new("L0601", DiagnosticPhase::Bytecode, error.message)
        .with_source_path(path.display().to_string())
}

/// A `L0003` resource report for a failed artifact write, used by every command
/// that emits a file.
pub(crate) fn write_failure_report(path: &Path, error: std::io::Error) -> DiagnosticReport {
    DiagnosticReport::new(
        "L0003",
        DiagnosticPhase::Resource,
        format!("failed to write `{}`: {error}", path.display()),
    )
    .with_source_path(path.display().to_string())
}

/// A `L0002` resource report for a failed artifact read.
pub(crate) fn read_failure_report(path: &Path, error: std::io::Error) -> DiagnosticReport {
    DiagnosticReport::new(
        "L0002",
        DiagnosticPhase::Resource,
        format!("failed to read `{}`: {error}", path.display()),
    )
    .with_source_path(path.display().to_string())
}

pub(crate) fn format_reports(
    reports: &[DiagnosticReport],
    mode: OutputMode,
    source: Option<&str>,
) -> String {
    match mode {
        OutputMode::Concise => reports
            .iter()
            .map(render_concise)
            .collect::<Vec<_>>()
            .join("\n"),
        OutputMode::Verbose => reports
            .iter()
            .map(|report| render_verbose(report, source))
            .collect::<Vec<_>>()
            .join("\n\n"),
        OutputMode::Json => format!(
            "{{\"status\":\"error\",\"diagnostics\":{}}}",
            render_json(reports)
        ),
    }
}
