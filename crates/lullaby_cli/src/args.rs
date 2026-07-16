//! Command-line parsing: the [`Invocation`] every command handler consumes, the
//! flag/mode enums, and the per-command usage strings.

use std::path::{Path, PathBuf};

use lullaby_diagnostics::{DiagnosticPhase, DiagnosticReport};
use lullaby_lexer::CANONICAL_EXTENSION;
use lullaby_loader::manifest;

use crate::commands::test_isolate::RUN_BATCH_COMMAND;
use crate::diagnostics::format_reports;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invocation {
    pub(crate) command: CommandName,
    pub(crate) path: PathBuf,
    pub(crate) output: Option<PathBuf>,
    pub(crate) mode: OutputMode,
    pub(crate) backend: Backend,
    pub(crate) optimization: OptimizationMode,
    /// How `fmt` emits its result (ignored by other commands).
    pub(crate) fmt_mode: FmtMode,
    /// Trailing arguments passed to the running program by `run <file.lby>`.
    /// Exposed to the program through the `args()` builtin. Empty otherwise.
    pub(crate) program_args: Vec<String>,
    /// Freestanding / no-std native build (`lullaby native --freestanding`).
    /// Guarantees the emitted executable links no C runtime — only the minimal
    /// OS import (`kernel32!ExitProcess`) needed to terminate. Ignored by every
    /// command other than `native`.
    pub(crate) freestanding: bool,
    /// Emit native source-line debug info (`lullaby native --debug` / `-g`),
    /// mapping each function's entry offset to its `.lby` source line. The format
    /// follows the target's object container: a CodeView `.debug$S` section on
    /// COFF, DWARF on ELF/Mach-O. Ignored by every command other than `native`.
    pub(crate) debug: bool,
    /// The native object-file target triple (`lullaby native --target <triple>`).
    /// `None` selects the default `x86_64-pc-windows-msvc` (COFF). The other
    /// accepted triples are `x86_64-unknown-linux-gnu` (ELF) and
    /// `x86_64-apple-darwin` (Mach-O). Ignored by every command other than
    /// `native`.
    pub(crate) native_target: Option<String>,
    /// Opt-in fast-math for the native backend (`lullaby native --fast-math`).
    /// Permits parity-BREAKING float optimizations — currently f64 sum/dot
    /// reductions vectorized with a 2-lane packed accumulator, which reorders the
    /// additions (float `+` is not associative) so the result can differ from the
    /// scalar fold in the last ULP. Off by default (bit-exact parity preserved);
    /// ignored by every command other than `native`.
    pub(crate) fast_math: bool,
    /// Case-sensitive substring filter for `lullaby test --filter <substring>`.
    /// Only discovered `test_*` functions whose name contains the substring run;
    /// the rest are not reported at all (they are filtered out, not skipped).
    /// `None` runs every discovered test. Ignored by every command other than
    /// `test`.
    pub(crate) filter: Option<String>,
    /// Per-test wall-clock deadline in seconds for `lullaby test --timeout <secs>`
    /// (default [`DEFAULT_TEST_TIMEOUT_SECS`]). A test whose isolated process
    /// outlives the deadline is killed and reported as a timeout failure — this is
    /// what contains a non-terminating test. `0` disables the deadline, so a hang
    /// then hangs the runner (opt-in only). Ignored by every command other than
    /// `test`.
    pub(crate) timeout_secs: u64,
    /// Internal (`__run-test-batch` only): the index in the discovered test list
    /// to resume the batch at, and whether to render tracebacks. Unused by every
    /// user-facing command.
    pub(crate) batch: Option<BatchArgs>,
}

/// The internal `__run-test-batch` parameters. See
/// [`crate::commands::test_isolate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BatchArgs {
    pub(crate) start: usize,
    pub(crate) verbose: bool,
}

/// The default per-test deadline for `lullaby test`, in seconds. Generous enough
/// that no honest test trips it on a loaded CI machine, short enough that a
/// non-terminating test is caught in a minute rather than stalling a pipeline
/// until someone kills it.
pub(crate) const DEFAULT_TEST_TIMEOUT_SECS: u64 = 60;

impl Default for Invocation {
    fn default() -> Self {
        Self {
            command: CommandName::Help,
            path: PathBuf::new(),
            output: None,
            mode: OutputMode::Concise,
            backend: Backend::Ast,
            optimization: OptimizationMode::None,
            fmt_mode: FmtMode::Print,
            program_args: Vec::new(),
            freestanding: false,
            debug: false,
            native_target: None,
            fast_math: false,
            filter: None,
            timeout_secs: DEFAULT_TEST_TIMEOUT_SECS,
            batch: None,
        }
    }
}

impl Invocation {
    /// An invocation of a command that takes no path and no flags.
    fn bare(command: CommandName) -> Self {
        Self {
            command,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FmtMode {
    /// Print the canonical formatting to stdout (default).
    Print,
    /// Rewrite the file in place.
    Write,
    /// Do not write; exit non-zero if the file is not already canonical.
    Check,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandName {
    Build,
    Check,
    Compile,
    Docs,
    Examples,
    Fmt,
    Inspect,
    New,
    Run,
    Test,
    /// Internal: run a contiguous batch of the discovered tests in this process,
    /// reporting each result on a private pipe the parent supplies in the stdin
    /// slot (NOT on stdout/stderr, which stay the test's own — see
    /// [`crate::commands::test_isolate`]). `lullaby test` re-invokes itself with
    /// this to isolate the suite. Not public; deliberately absent from `--help`.
    RunTestBatch,
    Wasm,
    Native,
    Lsp,
    Version,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputMode {
    Concise,
    Verbose,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
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
pub(crate) enum OptimizationMode {
    None,
    ConstantFold,
    DeadCode,
    Full,
}

impl OptimizationMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "constant-fold" => Some(Self::ConstantFold),
            "dead-code" => Some(Self::DeadCode),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

pub(crate) fn parse_invocation(args: Vec<String>) -> Result<Option<Invocation>, String> {
    let Some(command) = args.first() else {
        return Ok(None);
    };

    match command.as_str() {
        "--version" | "-V" => {
            bare_or_usage(&args, CommandName::Version, "usage: lullaby --version")
        }
        "--help" | "-h" | "help" => {
            bare_or_usage(&args, CommandName::Help, "usage: lullaby --help")
        }
        "docs" => bare_or_usage(&args, CommandName::Docs, "usage: lullaby docs"),
        "examples" => bare_or_usage(&args, CommandName::Examples, "usage: lullaby examples"),
        "lsp" => bare_or_usage(&args, CommandName::Lsp, "usage: lullaby lsp"),
        "new" => {
            // `lullaby new <name>` — exactly one argument, the project name,
            // carried through the `path` field to the `New` handler.
            match &args[1..] {
                [name] => Ok(Some(Invocation {
                    path: PathBuf::from(name),
                    ..Invocation::bare(CommandName::New)
                })),
                _ => Err("usage: lullaby new <name>".to_string()),
            }
        }
        // Internal, not user-facing:
        // `__run-test-batch <path> <start> <verbose:0|1> [filter]`. There is no
        // nonce: the protocol travels on a private pipe a test cannot reach, so
        // there is no secret to pass (and argv could never have kept one — a
        // process may read its own command line).
        value if value == RUN_BATCH_COMMAND => match &args[1..] {
            [path, start, verbose, rest @ ..] if rest.len() <= 1 => {
                let Ok(start) = start.parse::<usize>() else {
                    return Err(format!(
                        "usage: lullaby {RUN_BATCH_COMMAND} <path> <start> <verbose> [filter]"
                    ));
                };
                Ok(Some(Invocation {
                    path: PathBuf::from(path),
                    filter: rest.first().cloned(),
                    batch: Some(BatchArgs {
                        start,
                        verbose: verbose == "1",
                    }),
                    ..Invocation::bare(CommandName::RunTestBatch)
                }))
            }
            _ => Err(format!(
                "usage: lullaby {RUN_BATCH_COMMAND} <path> <start> <verbose> [filter]"
            )),
        },
        "build" | "check" | "compile" | "inspect" | "run" | "test" | "wasm" | "native" => {
            parse_file_command(command, &args[1..])
        }
        "fmt" => parse_fmt_command(&args[1..]),
        other => Err(format!("unknown command `{other}`\n\nrun `lullaby --help`")),
    }
}

/// A command that takes no arguments at all: accept it bare, reject anything else
/// with its usage line.
fn bare_or_usage(
    args: &[String],
    command: CommandName,
    usage: &str,
) -> Result<Option<Invocation>, String> {
    if args.len() == 1 {
        Ok(Some(Invocation::bare(command)))
    } else {
        Err(usage.to_string())
    }
}

fn parse_file_command(command: &str, args: &[String]) -> Result<Option<Invocation>, String> {
    let mut mode = OutputMode::Concise;
    let mut backend = Backend::Ast;
    let mut optimization = OptimizationMode::None;
    let mut output = None;
    let mut freestanding = false;
    let mut debug = false;
    let mut native_target: Option<String> = None;
    let mut fast_math = false;
    let mut filter: Option<String> = None;
    let mut timeout_secs: Option<u64> = None;
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
                // `test` has a fixed textual report and does not offer JSON.
                if command == "test"
                    || mode != OutputMode::Concise
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
            "--freestanding" | "--no-std" => {
                // Freestanding / no-std native builds only. Guarantees the emitted
                // executable links no C runtime (only kernel32!ExitProcess).
                if command != "native" {
                    return Err(usage);
                }
                freestanding = true;
                cursor += 1;
            }
            "--fast-math" => {
                // Opt-in parity-breaking float optimizations (native only).
                if command != "native" {
                    return Err(usage);
                }
                fast_math = true;
                cursor += 1;
            }
            "--debug" | "-g" => {
                // Native source-line debug info only: a CodeView `.debug$S` section
                // on COFF, DWARF on ELF/Mach-O. Without it the object bytes are
                // unchanged on every target.
                if command != "native" {
                    return Err(usage);
                }
                debug = true;
                cursor += 1;
            }
            "--filter" => {
                // `test` name-substring filter. Requires a value, rejects a
                // repeat, and rejects an empty substring (which would be a
                // no-op filter that reads as an intent to select something).
                if command != "test" || filter.is_some() {
                    return Err(usage);
                }
                let Some(value) = args.get(cursor + 1) else {
                    return Err(usage);
                };
                if value.is_empty() {
                    return Err(usage);
                }
                filter = Some(value.clone());
                cursor += 2;
            }
            "--timeout" => {
                // `test` per-test deadline in seconds. Requires a value, rejects a
                // repeat, and rejects a non-numeric value. `0` disables it.
                if command != "test" || timeout_secs.is_some() {
                    return Err(usage);
                }
                let Some(value) = args
                    .get(cursor + 1)
                    .and_then(|value| value.parse::<u64>().ok())
                else {
                    return Err(usage);
                };
                timeout_secs = Some(value);
                cursor += 2;
            }
            "--target" => {
                // Native object-file target triple only. Selects the container
                // format: COFF (default), ELF, or Mach-O.
                if command != "native" || native_target.is_some() {
                    return Err(usage);
                }
                let Some(value) = args.get(cursor + 1) else {
                    return Err(usage);
                };
                native_target = Some(value.clone());
                cursor += 2;
            }
            _ => break,
        }
    }

    let Some(path) = args.get(cursor) else {
        return Err(usage);
    };
    // `lullaby run <file.lby | project-dir | lullaby.json> [program args...]`
    // passes trailing tokens to the running program, exposed through the `args()`
    // builtin. Every other command (and `.lbc` run) keeps the strict "no trailing
    // token" behavior.
    let target_path = Path::new(path);
    let is_source =
        target_path.extension().and_then(|value| value.to_str()) == Some(CANONICAL_EXTENSION);
    let is_project = manifest::manifest_path_for(target_path).is_some();
    let runs_source = command == "run" && (is_source || is_project);
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
                "usage: lullaby run --backend ir|bytecode --optimize none|constant-fold|dead-code|full <file.lby>",
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
            "test" => CommandName::Test,
            "wasm" => CommandName::Wasm,
            "native" => CommandName::Native,
            _ => CommandName::Run,
        },
        path: PathBuf::from(path),
        output,
        mode,
        backend,
        optimization,
        program_args,
        freestanding,
        debug,
        native_target,
        fast_math,
        filter,
        timeout_secs: timeout_secs.unwrap_or(DEFAULT_TEST_TIMEOUT_SECS),
        ..Invocation::default()
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
        path: PathBuf::from(path),
        fmt_mode,
        ..Invocation::bare(CommandName::Fmt)
    }))
}

fn command_usage(command: &str) -> String {
    match command {
        "build" => "usage: lullaby build [--optimize none|constant-fold|dead-code|full] [-o output.lbc] [--verbose|--format json] <file.lby>".to_string(),
        "compile" => "usage: lullaby compile [--optimize none|constant-fold|dead-code|full] [-o output.lbc] [--verbose|--format json] <file.lby>".to_string(),
        "inspect" => "usage: lullaby inspect [--verbose|--format json] <file.lbc>".to_string(),
        "test" => "usage: lullaby test [--verbose] [--filter <substring>] [--timeout <seconds>] <file.lby>".to_string(),
        "wasm" => "usage: lullaby wasm [--verbose] [-o out.wasm] <file.lby>".to_string(),
        "native" => "usage: lullaby native [--verbose] [--freestanding|--no-std] [--debug|-g] [--fast-math] [--target x86_64-pc-windows-msvc|x86_64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-unknown-linux-gnu] [-o out] <file.lby>".to_string(),
        "run" => "usage: lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|full] [--verbose|--format json] <file.lby> [args...]\n       lullaby run [--verbose|--format json] <file.lbc>".to_string(),
        _ => "usage: lullaby check [--verbose|--format json] <file.lby>".to_string(),
    }
}
