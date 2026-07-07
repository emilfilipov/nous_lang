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
    emit_alpha1_native_program, emit_wasm_module, encode_bytecode_artifact, lower,
    lower_to_bytecode, optimize, run_bytecode_main, run_bytecode_main_with_args,
    run_main_with_args as run_ir_main_with_args,
};
use lullaby_lexer::{CANONICAL_EXTENSION, Diagnostic, lex, validate_source_path};
use lullaby_parser::{Program, format_program, parse};
use lullaby_runtime::{ErrorCategory, RuntimeError, Value, run_main_with_args};
use lullaby_semantics::{CheckedProgram, validate, validate_executable};

mod loader;

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
        CommandName::Fmt => fmt_file(invocation.path, invocation.fmt_mode),
        CommandName::Inspect => inspect_bytecode_artifact(invocation.path, invocation.mode),
        CommandName::Run => run_file(
            invocation.path,
            invocation.mode,
            invocation.backend,
            invocation.optimization,
            invocation.program_args,
        ),
        CommandName::Wasm => wasm_file(invocation.path, invocation.output, invocation.mode),
        CommandName::Native => native_file(invocation.path, invocation.output, invocation.mode),
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

/// Format a `.lby` source file: print the canonical rendering to stdout
/// (default), rewrite it in place (`--write`), or verify it is already
/// canonical and exit non-zero otherwise (`--check`).
fn fmt_file(path: PathBuf, fmt_mode: FmtMode) -> Result<(), String> {
    if let Err(diagnostic) = validate_source_path(&path) {
        return Err(format_reports(
            &[frontend_report(diagnostic, DiagnosticPhase::Source, &path)],
            OutputMode::Concise,
            None,
        ));
    }
    let source = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read `{}`: {error}", path.display()))?;
    let tokens = lex(&source).map_err(|diagnostics| {
        format_reports(
            &diagnostics
                .into_iter()
                .map(|diagnostic| frontend_report(diagnostic, DiagnosticPhase::Lexer, &path))
                .collect::<Vec<_>>(),
            OutputMode::Concise,
            Some(&source),
        )
    })?;
    let program = parse(&tokens).map_err(|diagnostics| {
        format_reports(
            &diagnostics
                .into_iter()
                .map(|diagnostic| frontend_report(diagnostic, DiagnosticPhase::Parser, &path))
                .collect::<Vec<_>>(),
            OutputMode::Concise,
            Some(&source),
        )
    })?;
    let formatted = format_program(&program);
    match fmt_mode {
        FmtMode::Print => print!("{formatted}"),
        FmtMode::Write => {
            if formatted != source {
                fs::write(&path, &formatted)
                    .map_err(|error| format!("failed to write `{}`: {error}", path.display()))?;
                println!("formatted {}", path.display());
            }
        }
        FmtMode::Check => {
            if formatted != source {
                return Err(format!(
                    "{} is not canonically formatted; run `lullaby fmt --write {}`",
                    path.display(),
                    path.display()
                ));
            }
        }
    }
    Ok(())
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
                "L0003",
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

/// Compile the scalar-subset functions of a `.lby` source file to a binary
/// `.wasm` module. Validates and lowers to IR exactly as `compile` does, then
/// runs the WASM emitter. `--verbose` prints which functions compiled and which
/// were skipped (and why).
fn wasm_file(path: PathBuf, output: Option<PathBuf>, mode: OutputMode) -> Result<(), String> {
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
            &[DiagnosticReport::new(
                "L0003",
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

/// Compile the i64-scalar-subset functions of a `.lby` source file to a native
/// x86-64 COFF object and, best-effort, link it into a runnable Windows `.exe`.
///
/// The object is always written (the reliable floor). Linking is attempted with
/// `rust-lld` when it and `kernel32.lib` can be located; if either is missing,
/// the command reports the object it produced and explains that linking was
/// unavailable rather than failing. `--verbose` lists compiled/skipped functions
/// and the linker command.
fn native_file(path: PathBuf, output: Option<PathBuf>, mode: OutputMode) -> Result<(), String> {
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
    let bytecode = lower_to_bytecode(&module);

    let program = match emit_alpha1_native_program(&bytecode) {
        Ok(program) => program,
        Err(error) => {
            let mut report = DiagnosticReport::new(error.code, DiagnosticPhase::Ir, error.message)
                .with_source_path(compiled.path.display().to_string());
            report = report.with_note(
                "the native backend compiles only i64-scalar functions (params and return all i64)",
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

    let exe_output = output.unwrap_or_else(|| compiled.path.with_extension("exe"));
    let obj_output = exe_output.with_extension("obj");
    if let Err(error) = fs::write(&obj_output, &program.bytes) {
        return Err(format_reports(
            &[DiagnosticReport::new(
                "L0003",
                DiagnosticPhase::Resource,
                format!("failed to write `{}`: {error}", obj_output.display()),
            )
            .with_source_path(obj_output.display().to_string())],
            mode,
            None,
        ));
    }

    // Best-effort link.
    let link = link_native_object(&obj_output, &exe_output, &program.entry_symbol);

    if mode == OutputMode::Json {
        println!("{{\"status\":\"ok\",\"diagnostics\":[]}}");
        return Ok(());
    }

    println!("native object: {}", obj_output.display());
    match &link {
        LinkOutcome::Linked => println!("native exe: {}", exe_output.display()),
        LinkOutcome::Unavailable(reason) => {
            println!("linking unavailable: {reason}");
            println!("emitted object only (link manually with rust-lld + kernel32.lib)");
        }
        LinkOutcome::Failed(reason) => {
            println!("linking failed: {reason}");
        }
    }
    if mode == OutputMode::Verbose {
        for name in &program.compiled {
            println!("compiled {name}");
        }
        for skip in &program.skipped {
            println!("skipped {}: {}", skip.name, skip.reason);
        }
    }
    Ok(())
}

/// The result of the best-effort link step.
enum LinkOutcome {
    /// A `.exe` was produced.
    Linked,
    /// The toolchain (rust-lld or kernel32.lib) could not be located.
    Unavailable(String),
    /// The linker ran but returned an error.
    Failed(String),
}

/// Discover `rust-lld` and the library search paths, then invoke it in lld-link
/// mode to produce a console `.exe`. Only `kernel32.lib` (for `ExitProcess`) is
/// required; the entry stub bypasses the CRT via `/entry:`.
fn link_native_object(obj: &Path, exe: &Path, entry_symbol: &str) -> LinkOutcome {
    let Some(lld) = find_rust_lld() else {
        return LinkOutcome::Unavailable("rust-lld not found in the rustc sysroot".to_string());
    };
    let lib_paths = discover_lib_paths();
    if !lib_paths
        .iter()
        .any(|dir| dir.join("kernel32.lib").is_file())
    {
        return LinkOutcome::Unavailable(
            "kernel32.lib not found (set the MSVC `LIB` environment variable, e.g. run from a Developer Command Prompt)"
                .to_string(),
        );
    }

    let mut command = std::process::Command::new(&lld);
    command.args(["-flavor", "link", "/nologo", "/subsystem:console"]);
    command.arg(format!("/entry:{entry_symbol}"));
    command.arg(format!("/out:{}", exe.display()));
    for dir in &lib_paths {
        command.arg(format!("/libpath:{}", dir.display()));
    }
    command.arg(obj);
    command.arg("kernel32.lib");

    match command.output() {
        Ok(out) if out.status.success() => LinkOutcome::Linked,
        Ok(out) => {
            let mut detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if detail.is_empty() {
                detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
            LinkOutcome::Failed(detail)
        }
        Err(error) => LinkOutcome::Failed(format!("could not run rust-lld: {error}")),
    }
}

/// Locate `rust-lld.exe` under `<rustc --print sysroot>/lib/rustlib/...`.
fn find_rust_lld() -> Option<PathBuf> {
    let output = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let candidate = PathBuf::from(sysroot)
        .join("lib")
        .join("rustlib")
        .join("x86_64-pc-windows-msvc")
        .join("bin")
        .join("rust-lld.exe");
    candidate.is_file().then_some(candidate)
}

/// Collect library search directories: the MSVC `LIB` environment variable (set
/// in a Developer Command Prompt) split on `;`, plus any it names that exist.
fn discover_lib_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(lib) = env::var("LIB") {
        for entry in lib.split(';') {
            let entry = entry.trim();
            if !entry.is_empty() {
                let dir = PathBuf::from(entry);
                if dir.is_dir() {
                    paths.push(dir);
                }
            }
        }
    }
    paths
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
        Backend::Ast => run_main_with_args(&compiled.program, program_args),
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
    let contents = fs::read_to_string(&path).map_err(|error| {
        format_reports(
            &[DiagnosticReport::new(
                "L0002",
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
                "L0002",
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
    // The module loader lexes, parses, resolves imports, enforces visibility and
    // no-shadowing, and merges every module into one flat program. A single-file
    // program with no imports behaves exactly as a direct lex+parse would.
    let loaded = match loader::load_program(path) {
        Ok(loaded) => loaded,
        Err(reports) => {
            // Attach the entry source when we have it so verbose rendering can
            // show source context for entry-file diagnostics.
            let source = fs::read_to_string(path).ok();
            return Err(CompileFailure { reports, source });
        }
    };
    let program = loaded.program;
    let source = loaded.entry_source;

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
    let mut report = DiagnosticReport::new("L0501", DiagnosticPhase::Ir, error.message)
        .with_source_path(path.display().to_string());
    if let Some(span) = error.span {
        report = report.with_span(span);
    }
    report
}

fn bytecode_report(error: BytecodeArtifactError, path: &Path) -> DiagnosticReport {
    DiagnosticReport::new("L0601", DiagnosticPhase::Bytecode, error.message)
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Invocation {
    command: CommandName,
    path: PathBuf,
    output: Option<PathBuf>,
    mode: OutputMode,
    backend: Backend,
    optimization: OptimizationMode,
    /// How `fmt` emits its result (ignored by other commands).
    fmt_mode: FmtMode,
    /// Trailing arguments passed to the running program by `run <file.lby>`.
    /// Exposed to the program through the `args()` builtin. Empty otherwise.
    program_args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FmtMode {
    /// Print the canonical formatting to stdout (default).
    Print,
    /// Rewrite the file in place.
    Write,
    /// Do not write; exit non-zero if the file is not already canonical.
    Check,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandName {
    Build,
    Check,
    Compile,
    Docs,
    Examples,
    Fmt,
    Inspect,
    Run,
    Wasm,
    Native,
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
                    fmt_mode: FmtMode::Print,
                    program_args: Vec::new(),
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
                    fmt_mode: FmtMode::Print,
                    program_args: Vec::new(),
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
                    fmt_mode: FmtMode::Print,
                    program_args: Vec::new(),
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
                    fmt_mode: FmtMode::Print,
                    program_args: Vec::new(),
                }))
            } else {
                Err("usage: lullaby examples".to_string())
            }
        }
        "build" | "check" | "compile" | "inspect" | "run" | "wasm" | "native" => {
            parse_file_command(command, &args[1..])
        }
        "fmt" => parse_fmt_command(&args[1..]),
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
                if (command != "compile"
                    && command != "build"
                    && command != "wasm"
                    && command != "native")
                    || output.is_some()
                {
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
    // `lullaby run <file.lby> [program args...]` passes trailing tokens to the
    // running program, exposed through the `args()` builtin. Every other command
    // (and `.lbc` run) keeps the strict "no trailing token" behavior.
    let runs_source = command == "run"
        && Path::new(path).extension().and_then(|value| value.to_str())
            == Some(CANONICAL_EXTENSION);
    let program_args: Vec<String> = if runs_source {
        args[cursor + 1..].to_vec()
    } else {
        if args.get(cursor + 1).is_some() {
            return Err(usage);
        }
        Vec::new()
    };
    if command == "run" && backend == Backend::Ast && optimization != OptimizationMode::None {
        return Err(format_reports(
            &[DiagnosticReport::new(
                "L0502",
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
            "wasm" => CommandName::Wasm,
            "native" => CommandName::Native,
            _ => CommandName::Run,
        },
        path: PathBuf::from(path),
        output,
        mode,
        backend,
        optimization,
        fmt_mode: FmtMode::Print,
        program_args,
    }))
}

fn parse_fmt_command(args: &[String]) -> Result<Option<Invocation>, String> {
    let usage = "usage: lullaby fmt [--write|--check] <file.lby>".to_string();
    let mut fmt_mode = FmtMode::Print;
    let mut cursor = 0;
    while let Some(arg) = args.get(cursor) {
        let next = match arg.as_str() {
            "--write" | "-w" => FmtMode::Write,
            "--check" => FmtMode::Check,
            _ => break,
        };
        // `--write` and `--check` are mutually exclusive and each allowed once.
        if fmt_mode != FmtMode::Print {
            return Err(usage);
        }
        fmt_mode = next;
        cursor += 1;
    }
    let Some(path) = args.get(cursor) else {
        return Err(usage);
    };
    if args.get(cursor + 1).is_some() {
        return Err(usage);
    }
    Ok(Some(Invocation {
        command: CommandName::Fmt,
        path: PathBuf::from(path),
        output: None,
        mode: OutputMode::Concise,
        backend: Backend::Ast,
        optimization: OptimizationMode::None,
        fmt_mode,
        program_args: Vec::new(),
    }))
}

fn command_usage(command: &str) -> String {
    match command {
        "build" => "usage: lullaby build [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>".to_string(),
        "compile" => "usage: lullaby compile [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>".to_string(),
        "inspect" => "usage: lullaby inspect [--verbose|--format json] <file.lbc>".to_string(),
        "wasm" => "usage: lullaby wasm [--verbose] [-o out.wasm] <file.lby>".to_string(),
        "native" => "usage: lullaby native [--verbose] [-o out.exe] <file.lby>".to_string(),
        "run" => "usage: lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] [--verbose|--format json] <file.lby> [args...]\n       lullaby run [--verbose|--format json] <file.lbc>".to_string(),
        _ => "usage: lullaby check [--verbose|--format json] <file.lby>".to_string(),
    }
}

fn print_help() {
    println!(
        "lullaby {}\n\nusage:\n  lullaby check [--verbose|--format json] <file.lby>\n  lullaby compile [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>\n  lullaby build [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>\n  lullaby inspect [--verbose|--format json] <file.lbc>\n  lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] [--verbose|--format json] <file.lby> [args...]\n  lullaby run [--verbose|--format json] <file.lbc>\n  lullaby wasm [--verbose] [-o out.wasm] <file.lby>\n  lullaby native [--verbose] [-o out.exe] <file.lby>\n  lullaby fmt [--write|--check] <file.lby>\n  lullaby docs\n  lullaby examples\n  lullaby --version",
        env!("CARGO_PKG_VERSION")
    );
}
