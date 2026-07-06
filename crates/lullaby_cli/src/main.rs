use std::{
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use lullaby_diagnostics::{
    DiagnosticPhase, DiagnosticReport, render_concise, render_json, render_verbose,
};
use lullaby_ir::{
    BYTECODE_ARTIFACT_EXTENSION, BytecodeArtifact, BytecodeArtifactError, IrCleanupRole,
    IrMemoryOperation, IrMemoryOperationKind, OptimizationConfig, decode_bytecode_artifact,
    encode_bytecode_artifact, lower, lower_to_bytecode, optimize, run_bytecode_main,
    run_main as run_ir_main,
};
use lullaby_lexer::{Diagnostic, lex, validate_source_path};
use lullaby_parser::{Program, parse};
use lullaby_runtime::{ErrorCategory, RuntimeError, Value, run_main};
use lullaby_semantics::{CheckedProgram, validate, validate_executable};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
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
        CommandName::Check => check(invocation.path, invocation.mode),
        CommandName::Build | CommandName::Compile => compile_file(
            invocation.path,
            invocation.output,
            invocation.mode,
            invocation.optimization,
        ),
        CommandName::Docs => docs(),
        CommandName::Examples => examples(),
        CommandName::Inspect => inspect_bytecode_artifact(invocation.path, invocation.mode),
        CommandName::Run => run_file(
            invocation.path,
            invocation.mode,
            invocation.backend,
            invocation.optimization,
        ),
        CommandName::Version => {
            println!("lullaby {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        CommandName::Help => {
            print_help();
            Ok(())
        }
    }
}

fn examples() -> Result<(), String> {
    let path = locate_examples().ok_or_else(|| {
        "examples not found; expected examples/valid near the lullaby binary or tests/fixtures/valid in the repository".to_string()
    })?;
    println!("examples: {}", path.display());
    Ok(())
}

fn locate_examples() -> Option<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(exe) = env::current_exe()
        && let Some(bin_dir) = exe.parent()
    {
        candidates.push(bin_dir.join("examples/valid"));
        candidates.push(bin_dir.join("../examples/valid"));
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(manifest_dir.join("../../examples/valid"));
    candidates.push(manifest_dir.join("../../tests/fixtures/valid"));
    candidates.push(PathBuf::from("examples/valid"));
    candidates.push(PathBuf::from("tests/fixtures/valid"));

    candidates.into_iter().find(|path| path.is_dir())
}

fn docs() -> Result<(), String> {
    let path = locate_docs().ok_or_else(|| {
        "offline docs not found; expected docs/index.html near the lullaby binary or offline_docs/index.html in the repository".to_string()
    })?;
    println!("docs: {}", path.display());
    Ok(())
}

fn locate_docs() -> Option<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(exe) = env::current_exe()
        && let Some(bin_dir) = exe.parent()
    {
        candidates.push(bin_dir.join("docs/index.html"));
        candidates.push(bin_dir.join("../docs/index.html"));
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(manifest_dir.join("../../offline_docs/index.html"));
    candidates.push(PathBuf::from("offline_docs/index.html"));

    candidates.into_iter().find(|path| path.is_file())
}

fn compile_file(
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
            &[DiagnosticReport::new(
                "N0003",
                DiagnosticPhase::Resource,
                format!("failed to write `{}`: {error}", output.display()),
            )
            .with_source_path(output.display().to_string())],
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

fn check(path: PathBuf, mode: OutputMode) -> Result<(), String> {
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

fn run_file(
    path: PathBuf,
    mode: OutputMode,
    backend: Backend,
    optimization: OptimizationMode,
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
        Backend::Ast => run_main(&compiled.program),
        Backend::Ir => {
            let module = lower(&compiled.checked).map_err(|error| {
                format_reports(
                    &[ir_report(error, &compiled.path)],
                    mode,
                    Some(&compiled.source),
                )
            })?;
            let module = optimize_module(module, optimization);
            run_ir_main(&module)
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
            run_bytecode_main(&bytecode)
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
    let contents = fs::read_to_string(&path).map_err(|error| {
        format_reports(
            &[DiagnosticReport::new(
                "N0002",
                DiagnosticPhase::Resource,
                format!("failed to read `{}`: {error}", path.display()),
            )
            .with_source_path(path.display().to_string())],
            mode,
            None,
        )
    })?;
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

fn inspect_bytecode_artifact(path: PathBuf, mode: OutputMode) -> Result<(), String> {
    let contents = fs::read_to_string(&path).map_err(|error| {
        format_reports(
            &[DiagnosticReport::new(
                "N0002",
                DiagnosticPhase::Resource,
                format!("failed to read `{}`: {error}", path.display()),
            )
            .with_source_path(path.display().to_string())],
            mode,
            None,
        )
    })?;
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

fn optimize_module(
    module: lullaby_ir::IrModule,
    optimization: OptimizationMode,
) -> lullaby_ir::IrModule {
    match optimization {
        OptimizationMode::None => module,
        OptimizationMode::ConstantFold => {
            let (module, _report) = optimize(&module, &OptimizationConfig::constant_folding());
            module
        }
        OptimizationMode::DeadCode => {
            let (module, _report) = optimize(&module, &OptimizationConfig::dead_code_elimination());
            module
        }
        OptimizationMode::Alpha => {
            let (module, _report) = optimize(&module, &OptimizationConfig::alpha_default());
            module
        }
    }
}

fn compile(path: &PathBuf, source_mode: SourceMode) -> Result<CompiledSource, CompileFailure> {
    if let Err(diagnostic) = validate_source_path(path) {
        return Err(CompileFailure::without_source(vec![frontend_report(
            diagnostic,
            DiagnosticPhase::Source,
            path,
        )]));
    }

    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            return Err(CompileFailure::without_source(vec![
                DiagnosticReport::new(
                    "N0002",
                    DiagnosticPhase::Resource,
                    format!("failed to read `{}`: {error}", path.display()),
                )
                .with_source_path(path.display().to_string()),
            ]));
        }
    };

    let tokens = match lex(&source) {
        Ok(tokens) => tokens,
        Err(diagnostics) => {
            return Err(CompileFailure::with_source(
                diagnostics
                    .into_iter()
                    .map(|diagnostic| frontend_report(diagnostic, DiagnosticPhase::Lexer, path))
                    .collect(),
                source,
            ));
        }
    };

    let program = match parse(&tokens) {
        Ok(program) => program,
        Err(diagnostics) => {
            return Err(CompileFailure::with_source(
                diagnostics
                    .into_iter()
                    .map(|diagnostic| frontend_report(diagnostic, DiagnosticPhase::Parser, path))
                    .collect(),
                source,
            ));
        }
    };

    let checked = match match source_mode {
        SourceMode::Library => validate(&program),
        SourceMode::Executable => validate_executable(&program),
    } {
        Ok(checked) => checked,
        Err(diagnostics) => {
            return Err(CompileFailure::with_source(
                diagnostics
                    .into_iter()
                    .map(|diagnostic| {
                        let mut report = DiagnosticReport::new(
                            diagnostic.code,
                            DiagnosticPhase::Semantic,
                            diagnostic.message,
                        )
                        .with_source_path(path.display().to_string());
                        if let Some(span) = diagnostic.span {
                            report = report.with_span(span);
                        }
                        if let Some(function) = diagnostic.function {
                            report = report.with_function(function);
                        }
                        report
                    })
                    .collect(),
                source,
            ));
        }
    };

    Ok(CompiledSource {
        path: path.clone(),
        source,
        program,
        checked,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceMode {
    Library,
    Executable,
}

fn frontend_report(
    diagnostic: Diagnostic,
    phase: DiagnosticPhase,
    path: &Path,
) -> DiagnosticReport {
    DiagnosticReport::new(diagnostic.code, phase, diagnostic.message)
        .with_source_path(path.display().to_string())
        .with_span(diagnostic.span)
}

fn runtime_report(error: RuntimeError, path: &Path) -> DiagnosticReport {
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

fn ir_report(error: lullaby_ir::IrLoweringError, path: &Path) -> DiagnosticReport {
    let mut report = DiagnosticReport::new("N0501", DiagnosticPhase::Ir, error.message)
        .with_source_path(path.display().to_string());
    if let Some(span) = error.span {
        report = report.with_span(span);
    }
    report
}

fn bytecode_report(error: BytecodeArtifactError, path: &Path) -> DiagnosticReport {
    DiagnosticReport::new("N0601", DiagnosticPhase::Bytecode, error.message)
        .with_source_path(path.display().to_string())
}

fn format_reports(reports: &[DiagnosticReport], mode: OutputMode, source: Option<&str>) -> String {
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

#[derive(Debug, Clone, PartialEq)]
struct CompiledSource {
    path: PathBuf,
    source: String,
    program: Program,
    checked: CheckedProgram,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompileFailure {
    reports: Vec<DiagnosticReport>,
    source: Option<String>,
}

impl CompileFailure {
    fn with_source(reports: Vec<DiagnosticReport>, source: String) -> Self {
        Self {
            reports,
            source: Some(source),
        }
    }

    fn without_source(reports: Vec<DiagnosticReport>) -> Self {
        Self {
            reports,
            source: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Invocation {
    command: CommandName,
    path: PathBuf,
    output: Option<PathBuf>,
    mode: OutputMode,
    backend: Backend,
    optimization: OptimizationMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandName {
    Build,
    Check,
    Compile,
    Docs,
    Examples,
    Inspect,
    Run,
    Version,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Concise,
    Verbose,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Ast,
    Ir,
    Bytecode,
}

impl Backend {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "ast" => Some(Self::Ast),
            "ir" => Some(Self::Ir),
            "bytecode" => Some(Self::Bytecode),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptimizationMode {
    None,
    ConstantFold,
    DeadCode,
    Alpha,
}

impl OptimizationMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "constant-fold" => Some(Self::ConstantFold),
            "dead-code" => Some(Self::DeadCode),
            "alpha" => Some(Self::Alpha),
            _ => None,
        }
    }
}

fn parse_invocation(args: Vec<String>) -> Result<Option<Invocation>, String> {
    let Some(command) = args.first() else {
        return Ok(None);
    };

    match command.as_str() {
        "--version" | "-V" => {
            if args.len() == 1 {
                Ok(Some(Invocation {
                    command: CommandName::Version,
                    path: PathBuf::new(),
                    output: None,
                    mode: OutputMode::Concise,
                    backend: Backend::Ast,
                    optimization: OptimizationMode::None,
                }))
            } else {
                Err("usage: lullaby --version".to_string())
            }
        }
        "--help" | "-h" | "help" => {
            if args.len() == 1 {
                Ok(Some(Invocation {
                    command: CommandName::Help,
                    path: PathBuf::new(),
                    output: None,
                    mode: OutputMode::Concise,
                    backend: Backend::Ast,
                    optimization: OptimizationMode::None,
                }))
            } else {
                Err("usage: lullaby --help".to_string())
            }
        }
        "docs" => {
            if args.len() == 1 {
                Ok(Some(Invocation {
                    command: CommandName::Docs,
                    path: PathBuf::new(),
                    output: None,
                    mode: OutputMode::Concise,
                    backend: Backend::Ast,
                    optimization: OptimizationMode::None,
                }))
            } else {
                Err("usage: lullaby docs".to_string())
            }
        }
        "examples" => {
            if args.len() == 1 {
                Ok(Some(Invocation {
                    command: CommandName::Examples,
                    path: PathBuf::new(),
                    output: None,
                    mode: OutputMode::Concise,
                    backend: Backend::Ast,
                    optimization: OptimizationMode::None,
                }))
            } else {
                Err("usage: lullaby examples".to_string())
            }
        }
        "build" | "check" | "compile" | "inspect" | "run" => {
            parse_file_command(command, &args[1..])
        }
        other => Err(format!("unknown command `{other}`\n\nrun `lullaby --help`")),
    }
}

fn parse_file_command(command: &str, args: &[String]) -> Result<Option<Invocation>, String> {
    let mut mode = OutputMode::Concise;
    let mut backend = Backend::Ast;
    let mut optimization = OptimizationMode::None;
    let mut output = None;
    let mut cursor = 0;
    let usage = command_usage(command);

    while let Some(arg) = args.get(cursor) {
        match arg.as_str() {
            "--verbose" => {
                if mode != OutputMode::Concise {
                    return Err(usage);
                }
                mode = OutputMode::Verbose;
                cursor += 1;
            }
            "--format" | "--diagnostic-format" => {
                if mode != OutputMode::Concise
                    || args.get(cursor + 1).map(String::as_str) != Some("json")
                {
                    return Err(usage);
                }
                mode = OutputMode::Json;
                cursor += 2;
            }
            "--backend" => {
                if command != "run" {
                    return Err(usage);
                }
                let Some(value) = args.get(cursor + 1).and_then(|value| Backend::parse(value))
                else {
                    return Err(usage);
                };
                backend = value;
                cursor += 2;
            }
            "--optimize" => {
                if command != "run" && command != "compile" && command != "build" {
                    return Err(usage);
                }
                let Some(value) = args
                    .get(cursor + 1)
                    .and_then(|value| OptimizationMode::parse(value))
                else {
                    return Err(usage);
                };
                optimization = value;
                cursor += 2;
            }
            "--output" | "-o" => {
                if (command != "compile" && command != "build") || output.is_some() {
                    return Err(usage);
                }
                let Some(value) = args.get(cursor + 1) else {
                    return Err(usage);
                };
                output = Some(PathBuf::from(value));
                cursor += 2;
            }
            _ => break,
        }
    }

    let Some(path) = args.get(cursor) else {
        return Err(usage);
    };
    if args.get(cursor + 1).is_some() {
        return Err(usage);
    }
    if command == "run" && backend == Backend::Ast && optimization != OptimizationMode::None {
        return Err(format_reports(
            &[DiagnosticReport::new(
                "N0502",
                DiagnosticPhase::Optimizer,
                "--optimize requires --backend ir or --backend bytecode",
            )
            .with_note(
                "usage: lullaby run --backend ir|bytecode --optimize none|constant-fold|dead-code|alpha <file.lby>",
            )],
            mode,
            None,
        ));
    }
    if (command == "compile" || command == "build") && backend != Backend::Ast {
        return Err(usage);
    }

    Ok(Some(Invocation {
        command: match command {
            "build" => CommandName::Build,
            "check" => CommandName::Check,
            "compile" => CommandName::Compile,
            "inspect" => CommandName::Inspect,
            _ => CommandName::Run,
        },
        path: PathBuf::from(path),
        output,
        mode,
        backend,
        optimization,
    }))
}

fn command_usage(command: &str) -> String {
    match command {
        "build" => "usage: lullaby build [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>".to_string(),
        "compile" => "usage: lullaby compile [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>".to_string(),
        "inspect" => "usage: lullaby inspect [--verbose|--format json] <file.lbc>".to_string(),
        "run" => "usage: lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] [--verbose|--format json] <file.lby>\n       lullaby run [--verbose|--format json] <file.lbc>".to_string(),
        _ => "usage: lullaby check [--verbose|--format json] <file.lby>".to_string(),
    }
}

fn print_help() {
    println!(
        "lullaby {}\n\nusage:\n  lullaby check [--verbose|--format json] <file.lby>\n  lullaby compile [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>\n  lullaby build [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>\n  lullaby inspect [--verbose|--format json] <file.lbc>\n  lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] [--verbose|--format json] <file.lby>\n  lullaby run [--verbose|--format json] <file.lbc>\n  lullaby docs\n  lullaby examples\n  lullaby --version",
        env!("CARGO_PKG_VERSION")
    );
}
