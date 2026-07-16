//! `lullaby inspect` — describe a compiled `.lbc` bytecode artifact.

use std::{
    fs,
    path::{Path, PathBuf},
};

use lullaby_ir::{
    BytecodeArtifact, IrCleanupRole, IrMemoryOperation, IrMemoryOperationKind,
    decode_bytecode_artifact,
};

use crate::args::OutputMode;
use crate::diagnostics::{bytecode_report, format_reports, read_failure_report};

pub(crate) fn inspect_bytecode_artifact(path: PathBuf, mode: OutputMode) -> Result<(), String> {
    let contents = fs::read_to_string(&path)
        .map_err(|error| format_reports(&[read_failure_report(&path, error)], mode, None))?;
    let artifact = decode_bytecode_artifact(&contents)
        .map_err(|error| format_reports(&[bytecode_report(error, &path)], mode, None))?;

    match mode {
        OutputMode::Json => println!("{}", inspect_json(&path, &artifact)),
        OutputMode::Concise | OutputMode::Verbose => {
            println!("artifact: {}", path.display());
            println!("format: {}", artifact.format);
            println!("version: {}", artifact.version);
            println!("entry: {}", artifact.entry);
            println!("target: {}", artifact.metadata.target);
            println!("payload: {}", artifact.metadata.payload);
            println!("functions: {}", artifact.function_table.len());
            println!("memory operations: {}", artifact.memory_operations.len());
            if mode == OutputMode::Verbose {
                for signature in &artifact.function_table {
                    println!("function: {}", format_signature(signature));
                }
                for operation in &artifact.memory_operations {
                    println!("memory operation: {}", format_memory_operation(operation));
                }
            }
        }
    }

    Ok(())
}

fn format_signature(signature: &lullaby_ir::BytecodeFunctionSignature) -> String {
    let params = signature
        .params
        .iter()
        .map(|param| format!("{}: {}", param.name, param.ty.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{}({}) -> {}",
        signature.name, params, signature.return_type.name
    )
}

fn inspect_json(path: &Path, artifact: &BytecodeArtifact) -> String {
    let functions = artifact
        .function_table
        .iter()
        .map(|signature| {
            let params = signature
                .params
                .iter()
                .map(|param| {
                    format!(
                        "{{\"name\":\"{}\",\"type\":\"{}\"}}",
                        json_escape(&param.name),
                        json_escape(&param.ty.name)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{{\"name\":\"{}\",\"params\":[{}],\"return_type\":\"{}\"}}",
                json_escape(&signature.name),
                params,
                json_escape(&signature.return_type.name)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let memory_operations = artifact
        .memory_operations
        .iter()
        .map(memory_operation_json)
        .collect::<Vec<_>>()
        .join(",");

    format!(
        "{{\"status\":\"ok\",\"artifact\":{{\"path\":\"{}\",\"format\":\"{}\",\"version\":{},\"entry\":\"{}\",\"metadata\":{{\"producer\":\"{}\",\"target\":\"{}\",\"payload\":\"{}\"}},\"functions\":[{}],\"memory_operations\":[{}]}}}}",
        json_escape(&path.display().to_string()),
        json_escape(&artifact.format),
        artifact.version,
        json_escape(&artifact.entry),
        json_escape(&artifact.metadata.producer),
        json_escape(&artifact.metadata.target),
        json_escape(&artifact.metadata.payload),
        functions,
        memory_operations
    )
}

fn format_memory_operation(operation: &IrMemoryOperation) -> String {
    format!(
        "#{} {} {} at {}:{} live={} bounds={} mutates={} cleanup={} unsafe={}",
        operation.sequence,
        operation.function,
        memory_operation_kind_label(&operation.kind),
        operation.span.line,
        operation.span.column,
        operation.safety.requires_live_resource,
        operation.safety.requires_bounds_check,
        operation.safety.mutates_memory,
        cleanup_role_label(operation.safety.cleanup_role),
        operation.safety.unsafe_boundary
    )
}

fn memory_operation_json(operation: &IrMemoryOperation) -> String {
    format!(
        "{{\"function\":\"{}\",\"sequence\":{},\"kind\":\"{}\",\"span\":{{\"line\":{},\"column\":{}}},\"safety\":{{\"requires_live_resource\":{},\"requires_bounds_check\":{},\"mutates_memory\":{},\"cleanup_role\":\"{}\",\"unsafe_boundary\":{}}}}}",
        json_escape(&operation.function),
        operation.sequence,
        json_escape(memory_operation_kind_label(&operation.kind)),
        operation.span.line,
        operation.span.column,
        operation.safety.requires_live_resource,
        operation.safety.requires_bounds_check,
        operation.safety.mutates_memory,
        json_escape(cleanup_role_label(operation.safety.cleanup_role)),
        operation.safety.unsafe_boundary
    )
}

fn memory_operation_kind_label(kind: &IrMemoryOperationKind) -> &'static str {
    match kind {
        IrMemoryOperationKind::Allocate { .. } => "allocate",
        IrMemoryOperationKind::Load { .. } => "load",
        IrMemoryOperationKind::Store { .. } => "store",
        IrMemoryOperationKind::Deallocate { .. } => "deallocate",
        IrMemoryOperationKind::BoundsCheck { .. } => "bounds-check",
        IrMemoryOperationKind::RegionCreate { .. } => "region-create",
        IrMemoryOperationKind::RegionResize { .. } => "region-resize",
        IrMemoryOperationKind::Copy { .. } => "copy",
        IrMemoryOperationKind::Cleanup { .. } => "cleanup",
    }
}

fn cleanup_role_label(role: IrCleanupRole) -> &'static str {
    match role {
        IrCleanupRole::None => "none",
        IrCleanupRole::CreatesResource => "creates-resource",
        IrCleanupRole::UsesResource => "uses-resource",
        IrCleanupRole::ReleasesResource => "releases-resource",
        IrCleanupRole::CheckedAccess => "checked-access",
    }
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped
}
