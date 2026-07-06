use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub line: usize,
    pub column: usize,
}

impl Span {
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticPhase {
    Source,
    Lexer,
    Parser,
    Semantic,
    Ir,
    Optimizer,
    Bytecode,
    Runtime,
    Resource,
}

impl fmt::Display for DiagnosticPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source => write!(formatter, "source"),
            Self::Lexer => write!(formatter, "lexer"),
            Self::Parser => write!(formatter, "parser"),
            Self::Semantic => write!(formatter, "semantic"),
            Self::Ir => write!(formatter, "ir"),
            Self::Optimizer => write!(formatter, "optimizer"),
            Self::Bytecode => write!(formatter, "bytecode"),
            Self::Runtime => write!(formatter, "runtime"),
            Self::Resource => write!(formatter, "resource"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => write!(formatter, "error"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceFrame {
    pub function: String,
    pub span: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticReport {
    pub code: String,
    pub phase: DiagnosticPhase,
    pub severity: Severity,
    pub message: String,
    pub source_path: Option<String>,
    pub span: Option<Span>,
    pub function: Option<String>,
    pub explanation: Option<String>,
    pub root_cause: Option<String>,
    pub suggested_fix: Option<String>,
    pub notes: Vec<String>,
    pub traceback: Vec<TraceFrame>,
}

impl DiagnosticReport {
    pub fn new(
        code: impl Into<String>,
        phase: DiagnosticPhase,
        message: impl Into<String>,
    ) -> Self {
        let mut report = Self {
            code: code.into(),
            phase,
            severity: Severity::Error,
            message: message.into(),
            source_path: None,
            span: None,
            function: None,
            explanation: None,
            root_cause: None,
            suggested_fix: None,
            notes: Vec::new(),
            traceback: Vec::new(),
        };
        report.apply_registry_guidance();
        report
    }

    pub fn with_source_path(mut self, path: impl Into<String>) -> Self {
        self.source_path = Some(path.into());
        self
    }

    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    pub fn with_function(mut self, function: impl Into<String>) -> Self {
        self.function = Some(function.into());
        self
    }

    pub fn with_traceback(mut self, traceback: Vec<TraceFrame>) -> Self {
        self.traceback = traceback;
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    fn apply_registry_guidance(&mut self) {
        if let Some(entry) = diagnostic_entry(&self.code) {
            self.explanation = Some(entry.explanation.to_string());
            self.root_cause = Some(entry.root_cause.to_string());
            self.suggested_fix = Some(entry.suggested_fix.to_string());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticEntry {
    pub code: &'static str,
    pub phase: DiagnosticPhase,
    pub explanation: &'static str,
    pub root_cause: &'static str,
    pub suggested_fix: &'static str,
}

pub fn diagnostic_catalog() -> &'static [DiagnosticEntry] {
    DIAGNOSTIC_CATALOG
}

pub fn diagnostic_entry(code: &str) -> Option<&'static DiagnosticEntry> {
    DIAGNOSTIC_CATALOG.iter().find(|entry| entry.code == code)
}

const DIAGNOSTIC_CATALOG: &[DiagnosticEntry] = &[
    DiagnosticEntry {
        code: "N0001",
        phase: DiagnosticPhase::Source,
        explanation: "Lullaby currently accepts only canonical .lullaby source files.",
        root_cause: "The provided path does not use the required .lullaby extension.",
        suggested_fix: "Rename the file to use the .lullaby extension or pass the intended .lullaby source file.",
    },
    DiagnosticEntry {
        code: "N0002",
        phase: DiagnosticPhase::Resource,
        explanation: "The CLI could not read the source file before lexing.",
        root_cause: "The path may be missing, inaccessible, or blocked by host permissions.",
        suggested_fix: "Check that the file exists and that the current user can read it.",
    },
    DiagnosticEntry {
        code: "N0003",
        phase: DiagnosticPhase::Resource,
        explanation: "The CLI could not write a compiled artifact.",
        root_cause: "The output path may be unwritable, or its parent directory may be missing.",
        suggested_fix: "Choose a writable output path or create the parent directory before compiling.",
    },
    DiagnosticEntry {
        code: "N0101",
        phase: DiagnosticPhase::Lexer,
        explanation: "Indentation must return to one of the active indentation levels.",
        root_cause: "A line is indented to a column that does not match any open block.",
        suggested_fix: "Align the line with an existing block level or indent it one level deeper than the parent.",
    },
    DiagnosticEntry {
        code: "N0102",
        phase: DiagnosticPhase::Lexer,
        explanation: "Lullaby uses indentation-only blocks.",
        root_cause: "The source contains a curly brace, which is not a block delimiter.",
        suggested_fix: "Remove the brace and express the block with indentation.",
    },
    DiagnosticEntry {
        code: "N0103",
        phase: DiagnosticPhase::Lexer,
        explanation: "Statements are separated by newlines, not semicolons.",
        root_cause: "The source contains a semicolon terminator.",
        suggested_fix: "Remove the semicolon and put each statement on its own line.",
    },
    DiagnosticEntry {
        code: "N0104",
        phase: DiagnosticPhase::Lexer,
        explanation: "A string literal must close on the same line in the current alpha.",
        root_cause: "The lexer reached the end of the line before finding a closing quote.",
        suggested_fix: "Add the missing closing quote or split the text into supported string literals.",
    },
    DiagnosticEntry {
        code: "N0205",
        phase: DiagnosticPhase::Parser,
        explanation: "The parser expected a required structural token such as a newline, indent, or dedent.",
        root_cause: "The surrounding syntax does not match the current indentation-only grammar.",
        suggested_fix: "Check the previous line, indentation level, and required function/control-flow body.",
    },
    DiagnosticEntry {
        code: "N0210",
        phase: DiagnosticPhase::Parser,
        explanation: "A region declaration is malformed.",
        root_cause: "The `region NAME: size=N[, align=N][, kind=...][, mutable=...]` form has a missing colon, `=`, field value, or an unknown field.",
        suggested_fix: "Write `region NAME: size=N` with optional `align`, `kind`, and `mutable` fields separated by commas.",
    },
    DiagnosticEntry {
        code: "N0207",
        phase: DiagnosticPhase::Parser,
        explanation: "The parser could not build a valid expression from this line.",
        root_cause: "The expression contains unsupported syntax, missing delimiters, or tokens in the wrong order.",
        suggested_fix: "Use current alpha expression syntax: literals, variables, calls, arrays, indexing, arithmetic, comparisons, and logical operators.",
    },
    DiagnosticEntry {
        code: "N0211",
        phase: DiagnosticPhase::Parser,
        explanation: "The source uses syntax reserved for a planned language feature that is not implemented in Alpha 1.",
        root_cause: "The parser recognized a future construct such as modules, imports, structs, pattern matching, or try/catch.",
        suggested_fix: "Remove the planned construct or rewrite the program using the current Alpha 1 function, local binding, control-flow, and builtin surface.",
    },
    DiagnosticEntry {
        code: "N0212",
        phase: DiagnosticPhase::Parser,
        explanation: "A type alias declaration is malformed.",
        root_cause: "An `alias NAME = TYPE` declaration is missing its `=` or target type.",
        suggested_fix: "Write `alias NAME = TYPE`, for example `alias Count = i64`.",
    },
    DiagnosticEntry {
        code: "N0213",
        phase: DiagnosticPhase::Parser,
        explanation: "A `try` block is missing its `catch` handler.",
        root_cause: "A `try` block must be followed by a `catch NAME` handler block.",
        suggested_fix: "Add a `catch NAME` block after the `try` body.",
    },
    DiagnosticEntry {
        code: "N0301",
        phase: DiagnosticPhase::Semantic,
        explanation: "A non-void function must produce a final value of its declared return type.",
        root_cause: "Control reaches the end of the function without a final expression or return value of the declared type.",
        suggested_fix: "Add a final expression with the declared type, return the correct value explicitly, or change the function return type.",
    },
    DiagnosticEntry {
        code: "N0303",
        phase: DiagnosticPhase::Semantic,
        explanation: "A local binding initializer must match the binding's declared type.",
        root_cause: "The declared type and the initializer expression type differ.",
        suggested_fix: "Change the declared type or change the initializer so both types match.",
    },
    DiagnosticEntry {
        code: "N0304",
        phase: DiagnosticPhase::Semantic,
        explanation: "A return statement must match the function's declared return type.",
        root_cause: "The returned expression type is different from the function return type.",
        suggested_fix: "Return the declared type, change the function return type, or use bare return only in void functions.",
    },
    DiagnosticEntry {
        code: "N0305",
        phase: DiagnosticPhase::Semantic,
        explanation: "Conditions must evaluate to bool.",
        root_cause: "A condition expression has a non-bool type.",
        suggested_fix: "Use a comparison, boolean literal, bool variable, or logical expression.",
    },
    DiagnosticEntry {
        code: "N0306",
        phase: DiagnosticPhase::Semantic,
        explanation: "Every variable must be declared before it is used.",
        root_cause: "The name is not visible in the current scope.",
        suggested_fix: "Add a let binding, pass the value as a parameter, or fix the variable name.",
    },
    DiagnosticEntry {
        code: "N0313",
        phase: DiagnosticPhase::Semantic,
        explanation: "Function and builtin arguments are statically type checked.",
        root_cause: "The argument expression type does not match the parameter type.",
        suggested_fix: "Pass a value of the expected type or change the called function signature.",
    },
    DiagnosticEntry {
        code: "N0317",
        phase: DiagnosticPhase::Semantic,
        explanation: "break only has meaning inside loop bodies.",
        root_cause: "A break statement appears outside loop, while, or for.",
        suggested_fix: "Move the break into a loop body or remove it.",
    },
    DiagnosticEntry {
        code: "N0318",
        phase: DiagnosticPhase::Semantic,
        explanation: "continue only has meaning inside loop bodies.",
        root_cause: "A continue statement appears outside loop, while, or for.",
        suggested_fix: "Move the continue into a loop body or remove it.",
    },
    DiagnosticEntry {
        code: "N0324",
        phase: DiagnosticPhase::Semantic,
        explanation: "Array literals are homogeneous in the current alpha.",
        root_cause: "At least one array element has a different type from the first element.",
        suggested_fix: "Use values with the same type or split mixed values into separate arrays.",
    },
    DiagnosticEntry {
        code: "N0326",
        phase: DiagnosticPhase::Semantic,
        explanation: "Array indexes must be i64 expressions.",
        root_cause: "The index expression is not an i64.",
        suggested_fix: "Use an i64 literal, variable, or arithmetic expression as the index.",
    },
    DiagnosticEntry {
        code: "N0329",
        phase: DiagnosticPhase::Semantic,
        explanation: "Executable source files must expose a zero-argument main entry point.",
        root_cause: "The program passed to compile or run either has no main function or declares parameters on main.",
        suggested_fix: "Add `fn main -> Type` with no parameters, then call helper functions from inside main.",
    },
    DiagnosticEntry {
        code: "N0330",
        phase: DiagnosticPhase::Semantic,
        explanation: "Raw pointer operations may read or write arbitrary memory and must be explicitly opted into.",
        root_cause: "A raw-pointer operation such as `ptr_read` or `ptr_write` was used outside an `unsafe` block.",
        suggested_fix: "Wrap the raw pointer operation in an `unsafe` block, or use safe `rc<T>`/`ref<T>` references instead.",
    },
    DiagnosticEntry {
        code: "N0331",
        phase: DiagnosticPhase::Semantic,
        explanation: "A reference builtin received a value whose type is not the expected reference or pointer kind.",
        root_cause: "An `rc`/`ref`/raw-pointer builtin was called with a value of a different type.",
        suggested_fix: "Pass an `rc<T>` to rc builtins, a `ref<T>` to `ref_get`, or a raw pointer to `ptr_read`/`ptr_write`.",
    },
    DiagnosticEntry {
        code: "N0340",
        phase: DiagnosticPhase::Semantic,
        explanation: "A region declaration has an invalid size, alignment, or kind.",
        root_cause: "The region size is not positive, the alignment is not a positive power of two, or the kind is not `static`/`dynamic`.",
        suggested_fix: "Use a positive size, a power-of-two alignment, and a `static` or `dynamic` kind.",
    },
    DiagnosticEntry {
        code: "N0341",
        phase: DiagnosticPhase::Semantic,
        explanation: "Two regions in the same function share a name.",
        root_cause: "A region name was declared more than once.",
        suggested_fix: "Give each region a unique name.",
    },
    DiagnosticEntry {
        code: "N0350",
        phase: DiagnosticPhase::Semantic,
        explanation: "A resource is used after it was freed (use-after-free or double-free).",
        root_cause: "A binding was read, written, or freed again after a straight-line `dealloc`/`rc_release`.",
        suggested_fix: "Remove the later use, or reallocate/rebind before using the resource again.",
    },
    DiagnosticEntry {
        code: "N0351",
        phase: DiagnosticPhase::Semantic,
        explanation: "A borrowed `ref<T>` cannot escape the scope of the owner it points into.",
        root_cause: "A function declared a `ref<T>` return type, which would let a borrow outlive its owner.",
        suggested_fix: "Return an owning `rc<T>` (or a value) instead of a borrowed `ref<T>`.",
    },
    DiagnosticEntry {
        code: "N0360",
        phase: DiagnosticPhase::Semantic,
        explanation: "A type alias name is declared more than once.",
        root_cause: "Two `alias` declarations share the same name.",
        suggested_fix: "Give each type alias a unique name.",
    },
    DiagnosticEntry {
        code: "N0361",
        phase: DiagnosticPhase::Semantic,
        explanation: "A type alias is defined in terms of itself.",
        root_cause: "An alias chain forms a cycle, so it has no canonical underlying type.",
        suggested_fix: "Break the cycle so each alias resolves to a concrete type.",
    },
    DiagnosticEntry {
        code: "N0501",
        phase: DiagnosticPhase::Ir,
        explanation: "The checked source program could not be lowered into typed IR.",
        root_cause: "The semantic program and IR lowering rules disagree about a supported alpha construct.",
        suggested_fix: "Report the source as a compiler bug and try the AST backend as a temporary workaround.",
    },
    DiagnosticEntry {
        code: "N0502",
        phase: DiagnosticPhase::Optimizer,
        explanation: "Optimization currently applies only to IR and bytecode execution paths.",
        root_cause: "An optimizer mode was requested while using the default AST backend.",
        suggested_fix: "Add --backend ir or --backend bytecode, or use --optimize none.",
    },
    DiagnosticEntry {
        code: "N0601",
        phase: DiagnosticPhase::Bytecode,
        explanation: "The compiled bytecode artifact could not be loaded.",
        root_cause: "The .lbc file is malformed, has unsupported format/version/metadata, names an unsupported or missing entry point, contains duplicate functions, or has a mismatched function table.",
        suggested_fix: "Recompile the original .lullaby source with the current lullaby compile command.",
    },
    DiagnosticEntry {
        code: "N0404",
        phase: DiagnosticPhase::Runtime,
        explanation: "Integer division by zero is not defined.",
        root_cause: "The right-hand side of a division evaluated to 0.",
        suggested_fix: "Guard the divisor before dividing or ensure the expression cannot become zero.",
    },
    DiagnosticEntry {
        code: "N0413",
        phase: DiagnosticPhase::Runtime,
        explanation: "Array indexing is bounds checked at runtime.",
        root_cause: "The computed index is negative or outside the array length.",
        suggested_fix: "Check the index value before indexing or adjust the loop/range bounds.",
    },
    DiagnosticEntry {
        code: "N0414",
        phase: DiagnosticPhase::Resource,
        explanation: "The runtime could not read a host text file.",
        root_cause: "The path may not exist, may not be readable, or may not contain supported text.",
        suggested_fix: "Check the path, current working directory, and file permissions.",
    },
    DiagnosticEntry {
        code: "N0415",
        phase: DiagnosticPhase::Resource,
        explanation: "The runtime could not write or append a host text file.",
        root_cause: "The destination path or parent directory may be missing or unwritable.",
        suggested_fix: "Create the parent directory or choose a writable output path.",
    },
    DiagnosticEntry {
        code: "N0416",
        phase: DiagnosticPhase::Resource,
        explanation: "The runtime could not start a host command.",
        root_cause: "The program name may not exist on PATH or may be blocked by host permissions.",
        suggested_fix: "Pass an executable program name and an array<string> of arguments.",
    },
    DiagnosticEntry {
        code: "N0419",
        phase: DiagnosticPhase::Resource,
        explanation: "The runtime could not write to or flush a standard stream.",
        root_cause: "Writing to stdout/stderr failed, usually because the stream was closed or the pipe was broken.",
        suggested_fix: "Ensure the output stream stays open, or redirect it to a writable destination.",
    },
    DiagnosticEntry {
        code: "N0420",
        phase: DiagnosticPhase::Runtime,
        explanation: "A value was thrown with `throw` and not caught by an enclosing `try`/`catch`.",
        root_cause: "Execution reached a `throw` whose error propagated past every `try` block.",
        suggested_fix: "Wrap the throwing code in `try` / `catch NAME`, or avoid the condition that throws.",
    },
];

pub fn render_concise(report: &DiagnosticReport) -> String {
    let mut header = format!("{} [{} {}]", report.code, report.phase, report.severity);
    if let Some(path) = &report.source_path {
        header.push_str(&format!(" at {path}"));
        if let Some(span) = report.span {
            header.push_str(&format!(":{}:{}", span.line, span.column));
        }
    } else if let Some(span) = report.span {
        header.push_str(&format!(" at {}:{}", span.line, span.column));
    }
    if let Some(function) = &report.function {
        header.push_str(&format!(" in `{function}`"));
    }
    format!("{header}: {}", report.message)
}

pub fn render_verbose(report: &DiagnosticReport, source: Option<&str>) -> String {
    let mut output = render_concise(report);
    if let (Some(source), Some(span)) = (source, report.span)
        && let Some(line) = source.lines().nth(span.line.saturating_sub(1))
    {
        output.push_str("\n\nSource:");
        output.push_str(&format!("\n{:>4} | {}", span.line, line));
        let caret_column = span.column.saturating_sub(1);
        output.push_str(&format!("\n     | {}^", " ".repeat(caret_column)));
    }
    if !report.traceback.is_empty() {
        output.push_str("\n\nTraceback:");
        for frame in report.traceback.iter().rev() {
            output.push_str(&format!("\n  in `{}`", frame.function));
            if let Some(span) = frame.span {
                output.push_str(&format!(" at {}:{}", span.line, span.column));
            }
        }
    }
    if let Some(explanation) = &report.explanation {
        output.push_str(&format!("\n\nProblem:\n  {explanation}"));
    }
    if let Some(root_cause) = &report.root_cause {
        output.push_str(&format!("\n\nRoot cause:\n  {root_cause}"));
    }
    if let Some(suggested_fix) = &report.suggested_fix {
        output.push_str(&format!("\n\nSuggested fix:\n  {suggested_fix}"));
    }
    if !report.notes.is_empty() {
        output.push_str("\n\nNotes:");
        for note in &report.notes {
            output.push_str(&format!("\n  - {note}"));
        }
    }
    output
}

pub fn render_json(reports: &[DiagnosticReport]) -> String {
    let mut output = String::from("[");
    for (index, report) in reports.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(&report_json(report));
    }
    output.push(']');
    output
}

fn report_json(report: &DiagnosticReport) -> String {
    let mut fields = Vec::new();
    fields.push(json_field("code", &report.code));
    fields.push(json_field("phase", &report.phase.to_string()));
    fields.push(json_field("severity", &report.severity.to_string()));
    fields.push(json_field("message", &report.message));
    fields.push(json_option_field(
        "source_path",
        report.source_path.as_deref(),
    ));
    fields.push(format!("\"span\":{}", span_json(report.span)));
    fields.push(json_option_field("function", report.function.as_deref()));
    fields.push(json_option_field(
        "explanation",
        report.explanation.as_deref(),
    ));
    fields.push(json_option_field(
        "root_cause",
        report.root_cause.as_deref(),
    ));
    fields.push(json_option_field(
        "suggested_fix",
        report.suggested_fix.as_deref(),
    ));
    fields.push(format!("\"notes\":{}", string_array_json(&report.notes)));
    fields.push(format!(
        "\"traceback\":{}",
        traceback_json(&report.traceback)
    ));
    format!("{{{}}}", fields.join(","))
}

fn json_field(name: &str, value: &str) -> String {
    format!("\"{name}\":\"{}\"", escape_json(value))
}

fn json_option_field(name: &str, value: Option<&str>) -> String {
    match value {
        Some(value) => json_field(name, value),
        None => format!("\"{name}\":null"),
    }
}

fn span_json(span: Option<Span>) -> String {
    match span {
        Some(span) => format!("{{\"line\":{},\"column\":{}}}", span.line, span.column),
        None => "null".to_string(),
    }
}

fn string_array_json(values: &[String]) -> String {
    let values = values
        .iter()
        .map(|value| format!("\"{}\"", escape_json(value)))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{values}]")
}

fn traceback_json(traceback: &[TraceFrame]) -> String {
    let frames = traceback
        .iter()
        .map(|frame| {
            format!(
                "{{\"function\":\"{}\",\"span\":{}}}",
                escape_json(&frame.function),
                span_json(frame.span)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{frames}]")
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => escaped.push(ch),
        }
    }
    escaped
}
