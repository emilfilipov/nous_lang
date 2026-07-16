//! The runtime error type and the scalar argument extractors shared by every
//! interpreter backend. Split out of `lib.rs` as a behavior-preserving code
//! move; `Value` (in `lib.rs`) is reached through a `crate::` path.

use std::fmt;

use lullaby_diagnostics::{Span, TraceFrame};

use crate::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    pub code: &'static str,
    pub category: ErrorCategory,
    pub message: String,
    pub span: Option<Span>,
    pub function: Option<String>,
    pub traceback: Vec<TraceFrame>,
}

impl RuntimeError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self::categorized(code, ErrorCategory::Runtime, message)
    }

    pub fn resource(code: &'static str, message: impl Into<String>) -> Self {
        Self::categorized(code, ErrorCategory::Resource, message)
    }

    pub fn categorized(
        code: &'static str,
        category: ErrorCategory,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            category,
            message: message.into(),
            span: None,
            function: None,
            traceback: Vec::new(),
        }
    }

    pub fn with_span(mut self, span: Span) -> Self {
        if self.span.is_none() {
            self.span = Some(span);
        }
        self
    }

    pub fn with_function(mut self, function: impl Into<String>) -> Self {
        if self.function.is_none() {
            self.function = Some(function.into());
        }
        self
    }

    pub fn with_traceback(mut self, traceback: Vec<TraceFrame>) -> Self {
        if self.traceback.is_empty() {
            self.traceback = traceback;
        }
        self
    }
}

/// The error raised when an `extern fn` (C-ABI) function is called on any
/// interpreter (AST, IR, or bytecode). The interpreters cannot execute real C
/// FFI — an extern function only has meaning after native codegen + linking —
/// so a call to one is `L0423` rather than a panic or a silent no-op. `check`
/// still validates the extern declaration and its call sites.
pub fn extern_call_error(name: &str) -> RuntimeError {
    RuntimeError::new(
        "L0423",
        format!(
            "cannot call extern (C-ABI) function `{name}` on an interpreter; compile with `lullaby native` to link and run it"
        ),
    )
}

/// The error raised when an interpreter encounters an `asm` inline-assembly
/// statement. Raw machine code can only run after native codegen + linking, so
/// every interpreter (AST, IR, bytecode) rejects it with `L0425`. `check` still
/// validates the byte range and the enclosing `unsafe` block.
pub fn asm_interpreter_error() -> RuntimeError {
    RuntimeError::new(
        "L0425",
        "cannot execute an `asm` (inline assembly) statement on an interpreter; compile with `lullaby native` to emit and link the machine code",
    )
}

/// The error raised when an interpreter encounters a port-mapped I/O builtin
/// (`port_in8`/`port_in16`/`port_in32`, `port_out8`/`port_out16`/`port_out32`).
///
/// `in`/`out` are **privileged x86 instructions**: they address the CPU's I/O
/// port space, fault with a general-protection fault at CPL 3 unless IOPL/the
/// TSS permission bitmap allows them, and are meaningless in a hosted process
/// that has no device behind the port anyway. There is no honest value for an
/// interpreter to return: the AST/IR/bytecode tiers model an abstract heap, not
/// a machine's I/O space, so a `port_in8(0x3F8u16)` has no defined answer.
///
/// So every interpreter **refuses** with `L0444` rather than invent one. This is
/// a deliberate, hard-won choice (the same reasoning as `asm`'s `L0425` and
/// `extern`'s `L0423`): a plausible-but-wrong device read — a fabricated `0`,
/// say — would silently mis-drive a PIC/PIT/UART and be far worse than a loud
/// refusal. The result is an honest **acceptance divergence**, not a parity
/// claim: native compiles port I/O and the interpreters decline to define it,
/// exactly as with the cross-frame `addr_of` case (`L0459`).
///
/// `check` still fully validates the call — arity, the `u16` port width, the
/// data width (`L0442`), and the enclosing `unsafe` block (`L0330`) — so a
/// mistyped port builtin is a *compile* error on every tier; only execution is
/// native-only.
pub fn port_io_interpreter_error(name: &str) -> RuntimeError {
    RuntimeError::new(
        "L0444",
        format!(
            "cannot execute the port-mapped I/O builtin `{name}` on an interpreter: `in`/`out` \
             are privileged x86 instructions addressing the CPU's I/O port space, which the \
             interpreters do not model — returning a fabricated value would silently mis-drive \
             the device. Compile with `lullaby native --freestanding` to emit the real \
             instruction, or use MMIO (`volatile_load`/`volatile_store`), which the \
             interpreters do define"
        ),
    )
}

/// A freestanding **static-buffer arena** allocation overflowed its backing buffer
/// (`documents/freestanding_tier_design.md` §5).
///
/// An arena never grows and never calls an allocator — its memory is the caller's
/// fixed buffer — so a bump past the end has to go somewhere defined. This is the
/// interpreters' half of that edge; natively it is a `ud2` trap. Both terminate
/// without producing a value, which is the same relationship the delivered
/// array-bounds failure already has (`L0413` on the interpreters, `ud2` natively),
/// and it is what decision **A5** requires: abort with a diagnostic, no unwinding.
///
/// §8's pluggable panic handler will route both edges to the program's own
/// `panic fn` with `kind = arena_overflow`.
///
/// **This code used to mean something else.** Until the arena was implemented on
/// the interpreters, `L0460` was a blanket refusal — "a static-buffer arena cannot
/// run here at all". That justification did not survive scrutiny: because the arena
/// bumps in whole 8-byte cells of an `array<i64>`, `arena_alloc(r, n)` is exactly
/// `addr_of(buf[cursor])` plus an integer cursor, and the interpreters define both
/// halves. There was nothing to reinterpret and therefore nothing to refuse. The
/// refusal was work not done, not an honest limitation, so it was replaced by a
/// real implementation and the code retargeted to the failure that genuinely
/// exists.
pub fn arena_overflow_error(region: &str, requested: i64, remaining: i64) -> RuntimeError {
    RuntimeError::new(
        "L0460",
        format!(
            "static-buffer arena `{region}` overflowed: requested {requested} cell(s) but only \
             {remaining} remain in its backing buffer. An arena never grows and never calls an \
             allocator — its memory is the fixed buffer you gave it — so this allocation has \
             nowhere to come from. Give the buffer more cells, or allocate less from it \
             (natively this same edge traps with `ud2`)"
        ),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    Runtime,
    Resource,
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime => write!(formatter, "runtime"),
            Self::Resource => write!(formatter, "resource"),
        }
    }
}

/// Unwrap a runtime `Value` expected to be a string, reporting `L0417` otherwise.
pub fn expect_string(name: &str, value: Value) -> Result<String, RuntimeError> {
    match value {
        Value::String(text) => Ok((text).into()),
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a string but got `{other}`"),
        )),
    }
}

pub fn expect_i64(name: &str, value: Value) -> Result<i64, RuntimeError> {
    match value {
        Value::I64(number) => Ok(number),
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects an i64 but got `{other}`"),
        )),
    }
}

/// Unwrap a runtime `Value` expected to be a `bool`, reporting `L0417`
/// otherwise. Shared by the AST interpreter and the IR interpreter so every
/// backend extracts boolean builtin arguments identically.
pub fn expect_bool(name: &str, value: Value) -> Result<bool, RuntimeError> {
    match value {
        Value::Bool(flag) => Ok(flag),
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a bool but got `{other}`"),
        )),
    }
}

/// Unwrap a runtime `Value` expected to be a list (an array), reporting `L0417`
/// otherwise. A `list<T>` is represented at runtime as a `Value::Array`.
pub fn expect_list(name: &str, value: Value) -> Result<Vec<Value>, RuntimeError> {
    match value {
        Value::Array(values) => Ok((values).into()),
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a list but got `{other}`"),
        )),
    }
}
