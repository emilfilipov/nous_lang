use std::{
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use nous_diagnostics::{
    DiagnosticPhase, DiagnosticReport, render_concise, render_json, render_verbose,
};
use nous_ir::{lower, lower_to_bytecode, run_bytecode_main, run_main as run_ir_main};
use nous_lexer::{Diagnostic, lex, validate_source_path};
use nous_parser::{Program, parse};
use nous_runtime::{ErrorCategory, RuntimeError, Value, run_main};
use nous_semantics::{CheckedProgram, validate};

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
        CommandName::Run => run_file(invocation.path, invocation.mode, invocation.backend),
        CommandName::Version => {
            println!("nlang {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        CommandName::Help => {
            print_help();
            Ok(())
        }
    }
}

fn check(path: PathBuf, mode: OutputMode) -> Result<(), String> {
    match compile(&path) {
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

fn run_file(path: PathBuf, mode: OutputMode, backend: Backend) -> Result<(), String> {
    let compiled = match compile(&path) {
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

fn compile(path: &PathBuf) -> Result<CompiledSource, CompileFailure> {
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

    let checked = match validate(&program) {
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

fn ir_report(error: nous_ir::IrLoweringError, path: &Path) -> DiagnosticReport {
    let mut report = DiagnosticReport::new("N0501", DiagnosticPhase::Ir, error.message)
        .with_source_path(path.display().to_string());
    if let Some(span) = error.span {
        report = report.with_span(span);
    }
    report
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    mode: OutputMode,
    backend: Backend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandName {
    Check,
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
                    mode: OutputMode::Concise,
                    backend: Backend::Ast,
                }))
            } else {
                Err("usage: nlang --version".to_string())
            }
        }
        "--help" | "-h" | "help" => {
            if args.len() == 1 {
                Ok(Some(Invocation {
                    command: CommandName::Help,
                    path: PathBuf::new(),
                    mode: OutputMode::Concise,
                    backend: Backend::Ast,
                }))
            } else {
                Err("usage: nlang --help".to_string())
            }
        }
        "check" | "run" => parse_file_command(command, &args[1..]),
        other => Err(format!("unknown command `{other}`\n\nrun `nlang --help`")),
    }
}

fn parse_file_command(command: &str, args: &[String]) -> Result<Option<Invocation>, String> {
    let mut mode = OutputMode::Concise;
    let mut backend = Backend::Ast;
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
            _ => break,
        }
    }

    let Some(path) = args.get(cursor) else {
        return Err(usage);
    };
    if args.get(cursor + 1).is_some() {
        return Err(usage);
    }

    Ok(Some(Invocation {
        command: if command == "check" {
            CommandName::Check
        } else {
            CommandName::Run
        },
        path: PathBuf::from(path),
        mode,
        backend,
    }))
}

fn command_usage(command: &str) -> String {
    match command {
        "run" => "usage: nlang run [--backend ast|ir|bytecode] [--verbose|--format json] <file.nl>"
            .to_string(),
        _ => "usage: nlang check [--verbose|--format json] <file.nl>".to_string(),
    }
}

fn print_help() {
    println!(
        "nlang {}\n\nusage:\n  nlang check [--verbose|--format json] <file.nl>\n  nlang run [--backend ast|ir|bytecode] [--verbose|--format json] <file.nl>\n  nlang --version",
        env!("CARGO_PKG_VERSION")
    );
}
