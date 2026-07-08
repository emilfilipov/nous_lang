use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use lullaby_diagnostics::{Span, TraceFrame};
use lullaby_parser::{
    AssignOp, BinaryOp, Expr, ExprKind, Function, MatchArm, MatchPattern, Place, Program, Stmt,
    UnaryOp,
};

/// The runtime type name of a value, used for trait-method dispatch. Structs and
/// enums carry their declared type name; scalars map to their primitive name.
pub fn value_type_name(value: &Value) -> String {
    match value {
        Value::I64(_) => "i64".to_string(),
        Value::F64(_) => "f64".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::String(_) => "string".to_string(),
        Value::Char(_) => "char".to_string(),
        Value::Byte(_) => "byte".to_string(),
        Value::Array(_) => "array".to_string(),
        Value::Struct { name, .. } => name.clone(),
        Value::Enum { enum_name, .. } => enum_name.clone(),
        Value::Map(_) => "map".to_string(),
        Value::Func(_) => "fn".to_string(),
        Value::Ptr(_) => "ptr".to_string(),
        Value::Socket(_) => "Socket".to_string(),
        Value::Chan(_) => "Chan".to_string(),
        Value::Task(_) => "Task".to_string(),
        Value::Future(_) => "Future".to_string(),
        Value::Mutex(_) => "Mutex".to_string(),
        Value::Void => "void".to_string(),
    }
}

/// An unbounded `i64` message-passing channel handle. Built over `std::sync::mpsc`:
/// a cloneable `Sender` and a `Receiver` shared behind an `Arc<Mutex<_>>` so the
/// value's `Clone` shares the same underlying queue (reference semantics, like a
/// socket handle). `Send` because every field is `Send`.
#[derive(Debug, Clone)]
pub struct Chan {
    pub sender: Sender<Value>,
    pub receiver: Arc<Mutex<Receiver<Value>>>,
}

impl PartialEq for Chan {
    /// Two channel handles are equal when they refer to the same queue (pointer
    /// identity of the shared receiver), mirroring how sockets compare by handle.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.receiver, &other.receiver)
    }
}

/// The shared, take-once join slot behind a `Task`. `JoinHandle` is not `Clone`,
/// so it lives behind an `Arc<Mutex<Option<_>>>`: `task_join` takes the handle
/// out (leaving `None`) so a second `task_join` is a harmless no-op.
pub type TaskHandle = Arc<Mutex<Option<JoinHandle<Result<Value, RuntimeError>>>>>;

/// A spawned-thread handle. `Send` because a `JoinHandle` is `Send`.
#[derive(Debug, Clone)]
pub struct Task {
    pub handle: TaskHandle,
}

impl PartialEq for Task {
    /// Task handles compare by identity of the shared join slot.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.handle, &other.handle)
    }
}

/// The shared, take-once join slot behind a `Future`. Structurally identical to a
/// `TaskHandle` (an `Arc<Mutex<Option<JoinHandle<...>>>>`), but the joined thread
/// PRODUCES a `Value`: `await` takes the handle out and returns that value,
/// whereas `task_join` discards it. Behind an `Arc<Mutex<Option<_>>>` so a
/// future can be moved/cloned as a value and `await`ed exactly once (a second
/// `await` on the same handle finds `None`).
pub type FutureHandle = Arc<Mutex<Option<JoinHandle<Result<Value, RuntimeError>>>>>;

/// A handle to an `async fn` call running on a spawned OS thread that will
/// produce a `T`. `await`ing it blocks until the thread completes and yields its
/// `T`. `Send` because a `JoinHandle` is `Send`; shared on clone.
#[derive(Debug, Clone)]
pub struct Future {
    pub handle: FutureHandle,
}

impl PartialEq for Future {
    /// Future handles compare by identity of the shared join slot.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.handle, &other.handle)
    }
}

/// A shared mutex over one `i64`. `Arc<Mutex<i64>>`, so the value's `Clone`
/// shares the same lock and cell across threads (reference semantics). `Send`.
#[derive(Debug, Clone)]
pub struct SharedMutex {
    pub cell: Arc<Mutex<i64>>,
}

impl PartialEq for SharedMutex {
    /// Mutex handles compare by identity of the shared cell.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cell, &other.cell)
    }
}

// `Eq` is intentionally omitted: `Value::F64` holds an `f64`, which is not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    I64(i64),
    F64(f64),
    Bool(bool),
    String(String),
    /// A Unicode scalar value.
    Char(char),
    /// An 8-bit unsigned integer (0-255).
    Byte(u8),
    Array(Vec<Value>),
    Ptr(usize),
    Struct {
        name: String,
        fields: Vec<(String, Value)>,
    },
    Enum {
        enum_name: String,
        variant: String,
        payload: Vec<Value>,
    },
    /// A `map<K, V>`: an insertion-ordered association list. Lookup and insert
    /// are linear scans using `Value` equality.
    Map(Vec<(Value, Value)>),
    /// A first-class function value: a handle to a top-level function by name.
    /// No environment is captured in this increment.
    Func(String),
    /// A network socket handle: an index into the interpreter's per-runtime
    /// `sockets` table. The underlying OS resource (a `TcpListener`,
    /// `TcpStream`, or `UdpSocket`) is not `Clone`, so sockets are represented
    /// as opaque integer handles, mirroring how `Ptr` indexes the heap.
    Socket(usize),
    /// An unbounded `i64` message-passing channel; shared on clone.
    Chan(Chan),
    /// A one-shot handle to a spawned detached thread; `join`ed once.
    Task(Task),
    /// A handle to an `async fn` call running on a spawned OS thread; `await`ed
    /// once to retrieve the produced value.
    Future(Future),
    /// A shared mutex over one `i64`; shared on clone.
    Mutex(SharedMutex),
    Void,
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::I64(value) => write!(formatter, "{value}"),
            Self::F64(value) => write!(formatter, "{value}"),
            Self::Bool(value) => write!(formatter, "{value}"),
            Self::String(value) => write!(formatter, "{value}"),
            Self::Char(value) => write!(formatter, "{value}"),
            Self::Byte(value) => write!(formatter, "{value}"),
            Self::Array(values) => {
                let values = values
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(formatter, "[{values}]")
            }
            Self::Ptr(slot) => write!(formatter, "ptr({slot})"),
            Self::Struct { name, fields } => {
                let rendered = fields
                    .iter()
                    .map(|(field, value)| format!("{field}: {value}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(formatter, "{name}({rendered})")
            }
            Self::Enum {
                variant, payload, ..
            } => {
                if payload.is_empty() {
                    write!(formatter, "{variant}")
                } else {
                    let rendered = payload
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    write!(formatter, "{variant}({rendered})")
                }
            }
            Self::Map(entries) => {
                let rendered = entries
                    .iter()
                    .map(|(key, value)| format!("{key}: {value}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(formatter, "{{{rendered}}}")
            }
            Self::Func(name) => write!(formatter, "fn {name}"),
            Self::Socket(handle) => write!(formatter, "socket({handle})"),
            Self::Chan(_) => write!(formatter, "chan"),
            Self::Task(_) => write!(formatter, "task"),
            Self::Future(_) => write!(formatter, "future"),
            Self::Mutex(_) => write!(formatter, "mutex"),
            Self::Void => write!(formatter, "void"),
        }
    }
}

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
        Value::String(text) => Ok(text),
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a string but got `{other}`"),
        )),
    }
}

/// Unwrap a runtime `Value` expected to be an `i64`, reporting `L0417` otherwise.
/// The process-global monotonic baseline for `mono_now`. It is initialized on
/// the first call to [`monotonic_now_nanos`] and never re-initialized, so the
/// clock is non-decreasing for the whole process. Both interpreters and the
/// bytecode VM route through this single function, so they observe one shared
/// baseline regardless of which backend is active.
static MONOTONIC_BASELINE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Nanoseconds elapsed since the process-global monotonic baseline. The first
/// call establishes the baseline (returning `0` or a tiny value); every later
/// call returns a value `>=` all previous ones. Backs the `mono_now` builtin on
/// every interpreter backend.
pub fn monotonic_now_nanos() -> i64 {
    let baseline = MONOTONIC_BASELINE.get_or_init(std::time::Instant::now);
    baseline.elapsed().as_nanos() as i64
}

/// Milliseconds since the Unix epoch (wall-clock time). Backs the `wall_now`
/// builtin. A pre-epoch system clock (rare, misconfigured host) yields the
/// negated pre-epoch offset so the value stays total and never panics.
pub fn wall_now_millis() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(delta) => delta.as_millis() as i64,
        Err(err) => -(err.duration().as_millis() as i64),
    }
}

/// Sleep the current thread for `ms` milliseconds. A negative `ms` is treated as
/// `0` (no sleep, no error), keeping the builtin total. Backs `sleep_millis`.
pub fn sleep_millis(ms: i64) {
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
    }
}

/// Fill a fresh buffer of `len` bytes with cryptographically-secure randomness
/// straight from the operating-system CSPRNG (`getrandom`/`getentropy` on
/// Unix-likes, `BCryptGenRandom` on Windows, `/dev/urandom` as a fallback).
/// This is a real OS randomness source — never a seeded or deterministic PRNG,
/// so callers may use it for keys, nonces, and tokens.
///
/// Backs the `os_random` builtin on every interpreter backend (AST runtime, IR
/// interpreter, bytecode VM) so all three exhibit identical behavior:
///
/// - `len < 0` returns `Err("os_random length must be non-negative")` and never
///   panics.
/// - `len == 0` returns `Ok(Vec::new())` (no syscall, an empty buffer).
/// - a genuine OS RNG failure is surfaced as `Err(message)` rather than a panic.
pub fn os_random_bytes(len: i64) -> Result<Vec<u8>, String> {
    if len < 0 {
        return Err("os_random length must be non-negative".to_string());
    }
    let len = len as usize;
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut buffer = vec![0u8; len];
    match getrandom::fill(&mut buffer) {
        Ok(()) => Ok(buffer),
        Err(error) => Err(format!("os_random failed: {error}")),
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

/// Map a matched pair of `Char`/`Byte` values to comparable `u32` order keys
/// (a char's code point, a byte's numeric value). Returns `None` for any other
/// pair so ordering can fall through to the `i64` path.
pub fn scalar_order_keys(left: &Value, right: &Value) -> Option<(u32, u32)> {
    match (left, right) {
        (Value::Char(l), Value::Char(r)) => Some((*l as u32, *r as u32)),
        (Value::Byte(l), Value::Byte(r)) => Some((u32::from(*l), u32::from(*r))),
        _ => None,
    }
}

/// Left shift of an `i64` with a total, deterministic shift amount: the amount
/// is masked to its low 6 bits (`amount & 63`), matching x86/Java `long`
/// semantics, so a large or negative amount never panics or errors. Every
/// backend (AST, IR interpreter, bytecode VM) must use this exact rule.
pub fn shift_left(value: i64, amount: i64) -> i64 {
    value.wrapping_shl(((amount as u64) & 63) as u32)
}

/// Arithmetic (sign-preserving) right shift of an `i64` with the same masked,
/// deterministic shift amount as [`shift_left`].
pub fn shift_right(value: i64, amount: i64) -> i64 {
    value.wrapping_shr(((amount as u64) & 63) as u32)
}

/// Unwrap a runtime `Value` expected to be a list (an array), reporting `L0417`
/// otherwise. A `list<T>` is represented at runtime as a `Value::Array`.
pub fn expect_list(name: &str, value: Value) -> Result<Vec<Value>, RuntimeError> {
    match value {
        Value::Array(values) => Ok(values),
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a list but got `{other}`"),
        )),
    }
}

/// Unwrap a runtime `Value` expected to be a channel handle, reporting `L0417`
/// otherwise.
pub fn expect_chan(name: &str, value: Value) -> Result<Chan, RuntimeError> {
    match value {
        Value::Chan(chan) => Ok(chan),
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a Chan but got `{other}`"),
        )),
    }
}

/// Unwrap a runtime `Value` expected to be a task handle, reporting `L0417`
/// otherwise.
pub fn expect_task(name: &str, value: Value) -> Result<Task, RuntimeError> {
    match value {
        Value::Task(task) => Ok(task),
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a Task but got `{other}`"),
        )),
    }
}

/// Unwrap a runtime `Value` expected to be a future handle, reporting `L0417`
/// otherwise. The semantic checker (`L0344`) normally prevents awaiting a
/// non-future, so this is a defensive runtime guard.
pub fn expect_future(name: &str, value: Value) -> Result<Future, RuntimeError> {
    match value {
        Value::Future(future) => Ok(future),
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a Future but got `{other}`"),
        )),
    }
}

/// Unwrap a runtime `Value` expected to be a mutex handle, reporting `L0417`
/// otherwise.
pub fn expect_mutex(name: &str, value: Value) -> Result<SharedMutex, RuntimeError> {
    match value {
        Value::Mutex(mutex) => Ok(mutex),
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a Mutex but got `{other}`"),
        )),
    }
}

/// Unwrap a runtime `Value` expected to be a map, reporting `L0417` otherwise.
pub fn expect_map(name: &str, value: Value) -> Result<Vec<(Value, Value)>, RuntimeError> {
    match value {
        Value::Map(entries) => Ok(entries),
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a map but got `{other}`"),
        )),
    }
}

/// Build an `option<V>` runtime value using the shared `Value::Enum` option
/// representation (`some(v)` or `none`).
pub fn option_value(payload: Option<Value>) -> Value {
    match payload {
        Some(value) => Value::Enum {
            enum_name: "option".to_string(),
            variant: "some".to_string(),
            payload: vec![value],
        },
        None => Value::Enum {
            enum_name: "option".to_string(),
            variant: "none".to_string(),
            payload: Vec::new(),
        },
    }
}

/// Build a `result<T, E>` runtime value using the shared `Value::Enum` result
/// representation (`ok(v)` or `err(e)`).
pub fn result_value(payload: Result<Value, Value>) -> Value {
    match payload {
        Ok(value) => Value::Enum {
            enum_name: "result".to_string(),
            variant: "ok".to_string(),
            payload: vec![value],
        },
        Err(error) => Value::Enum {
            enum_name: "result".to_string(),
            variant: "err".to_string(),
            payload: vec![error],
        },
    }
}

/// Create a fresh unbounded `i64` channel as a `Value::Chan`. Shared by the AST
/// and IR interpreters so both keep identical channel semantics.
pub fn new_chan() -> Value {
    let (sender, receiver) = std::sync::mpsc::channel::<Value>();
    Value::Chan(Chan {
        sender,
        receiver: Arc::new(Mutex::new(receiver)),
    })
}

/// Join a spawned thread once: take the `JoinHandle` out of the shared slot and
/// wait for it, propagating a worker error or panic. A second `join` (the slot is
/// already `None`) is a no-op that returns `void`. Shared by both interpreters.
pub fn join_task(task: &Task) -> Result<Value, RuntimeError> {
    let handle = {
        let mut slot = task
            .handle
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "join on a poisoned task handle"))?;
        slot.take()
    };
    match handle {
        // Already joined: joining again is a harmless no-op.
        None => Ok(Value::Void),
        Some(handle) => match handle.join() {
            Ok(result) => result.map(|_| Value::Void),
            Err(_) => Err(RuntimeError::new("L0401", "spawned thread panicked")),
        },
    }
}

/// Await a future once: take the `JoinHandle` out of the shared slot, wait for
/// the spawned thread, and return the `Value` it produced (unlike `join_task`,
/// which discards the value). A worker error propagates; a panic is `L0401`. A
/// second `await` on the same handle (the slot is already `None`) returns
/// `void` — a defensive no-op, though semantics binds each future to one
/// `await`. Shared by both interpreters so `await` has identical semantics.
pub fn await_future(future: &Future) -> Result<Value, RuntimeError> {
    let handle = {
        let mut slot = future
            .handle
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "await on a poisoned future handle"))?;
        slot.take()
    };
    match handle {
        None => Ok(Value::Void),
        Some(handle) => match handle.join() {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::new("L0401", "awaited thread panicked")),
        },
    }
}

/// Build an `err(string)` result whose payload is the display form of an I/O or
/// network error. Network builtins report failures as runtime `result` values
/// rather than propagating a `RuntimeError`.
pub fn net_err(error: &std::io::Error) -> Value {
    result_value(Err(Value::String(error.to_string())))
}

/// Perform one HTTP/1.1 exchange over a fresh `TcpStream` and return the
/// response body as a `result<string, string>` runtime value.
///
/// `method` is `"GET"` or `"POST"`; `body` is `None` for GET and `Some(text)`
/// for POST (sent as `Content-Type: text/plain`). Only the `http` scheme is
/// supported — an `https://` URL yields `err("https not supported")`. Chunked
/// transfer decoding is not implemented; the response body is read to EOF via
/// the `Connection: close` header. A read timeout keeps a hung server from
/// stalling the caller. A 2xx/3xx status yields `ok(body)`; any 4xx/5xx status
/// yields `err("http {code}: {first-body-line}")`. All connection/parse/HTTP
/// failures are `err(...)` results, never a propagated `RuntimeError`.
pub fn http_exchange(method: &str, url: &str, body: Option<&str>) -> Value {
    use std::io::{Read, Write};
    use std::time::Duration;

    let (scheme, rest) = match url.split_once("://") {
        Some(parts) => parts,
        None => return result_value(Err(Value::String("invalid url".to_string()))),
    };
    if scheme.eq_ignore_ascii_case("https") {
        return result_value(Err(Value::String("https not supported".to_string())));
    }
    if !scheme.eq_ignore_ascii_case("http") {
        return result_value(Err(Value::String(format!("unsupported scheme `{scheme}`"))));
    }

    // Split `host[:port]` from the path (default `/`).
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return result_value(Err(Value::String("missing host".to_string())));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port_text)) => match port_text.parse::<u16>() {
            Ok(port) => (host, port),
            Err(_) => {
                return result_value(Err(Value::String(format!("invalid port `{port_text}`"))));
            }
        },
        None => (authority, 80u16),
    };
    let path = if path.is_empty() { "/" } else { path };

    let mut stream = match TcpStream::connect((host, port)) {
        Ok(stream) => stream,
        Err(error) => return net_err(&error),
    };
    if let Err(error) = stream.set_read_timeout(Some(Duration::from_secs(10))) {
        return net_err(&error);
    }

    let request = match body {
        None => format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: lullaby\r\nConnection: close\r\n\r\n"
        ),
        Some(body) => format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: lullaby\r\nConnection: close\r\nContent-Type: text/plain\r\nContent-Length: {len}\r\n\r\n{body}",
            len = body.len()
        ),
    };
    if let Err(error) = stream.write_all(request.as_bytes()) {
        return net_err(&error);
    }
    if let Err(error) = stream.flush() {
        return net_err(&error);
    }

    let mut response = Vec::new();
    if let Err(error) = stream.read_to_end(&mut response) {
        return net_err(&error);
    }

    let split = response.windows(4).position(|window| window == b"\r\n\r\n");
    let (head, resp_body) = match split {
        Some(index) => (&response[..index], &response[index + 4..]),
        None => {
            return result_value(Err(Value::String(
                "malformed response: no header terminator".to_string(),
            )));
        }
    };
    let head = String::from_utf8_lossy(head);
    let status_line = head.lines().next().unwrap_or("");
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|token| token.parse::<u16>().ok());
    let body_text = String::from_utf8_lossy(resp_body).into_owned();
    match code {
        Some(code) if (200..400).contains(&code) => result_value(Ok(Value::String(body_text))),
        Some(code) => {
            let first_line = body_text.lines().next().unwrap_or("");
            result_value(Err(Value::String(format!("http {code}: {first_line}"))))
        }
        None => result_value(Err(Value::String(format!(
            "malformed status line `{status_line}`"
        )))),
    }
}

/// An open network resource held behind a socket handle. Not `Clone`, which is
/// why sockets are surfaced to Lullaby as opaque integer handles. Shared by the
/// AST interpreter and the IR interpreter so both keep identical socket
/// semantics.
pub enum SocketResource {
    Listener(TcpListener),
    Stream(TcpStream),
    Udp(UdpSocket),
}

/// First character index of `needle` in `text`, or `-1` when absent.
pub fn char_find(text: &str, needle: &str) -> i64 {
    match text.find(needle) {
        Some(byte_index) => text[..byte_index].chars().count() as i64,
        None => -1,
    }
}

pub fn run_main(program: &Program) -> Result<Value, RuntimeError> {
    run_main_with_args(program, Vec::new())
}

/// Run `main` with the running program's CLI arguments, which the `args()`
/// builtin exposes. `run_main` is the zero-argument wrapper.
///
/// The program is wrapped in an `Arc<Program>` here so a detached thread created
/// by `spawn` can own a share of the program and run Lullaby independently. The
/// interpreter keeps its usual `&Program` borrow (from `&*arc`, which the `Arc`
/// outlives) for normal use, and ALSO holds an owned `Arc<Program>` clone purely
/// to hand to spawned threads. These are two separate handles to the same shared
/// data — not a self-referential struct.
pub fn run_main_with_args(program: &Program, args: Vec<String>) -> Result<Value, RuntimeError> {
    let arc = Arc::new(program.clone());
    run_main_shared(arc, args)
}

/// Shared-program entry: build an interpreter borrowing `&*arc` while retaining
/// an owned `Arc<Program>` clone for detached-thread spawning.
fn run_main_shared(arc: Arc<Program>, args: Vec<String>) -> Result<Value, RuntimeError> {
    if !arc.functions.iter().any(|function| function.name == "main") {
        return Err(RuntimeError::new("L0422", "missing `main` function"));
    }
    let mut runtime = Runtime::new(&arc, Arc::clone(&arc))?;
    runtime.program_args = args;
    runtime.call_function("main", Vec::new())
}

/// Run a single named zero-argument function against `program` through the AST
/// interpreter, mirroring `run_main` but for an arbitrary entry point (used by
/// the `lullaby test` runner). The program need not define `main`. Returns the
/// function's value on success, or the propagated `RuntimeError` — including a
/// user `throw` / failed `assert` (code `L0420`) — on failure.
pub fn run_named_function(program: &Program, name: &str) -> Result<Value, RuntimeError> {
    let arc = Arc::new(program.clone());
    let mut runtime = Runtime::new(&arc, Arc::clone(&arc))?;
    runtime.call_function(name, Vec::new())
}

struct Runtime<'a> {
    /// The whole program, borrowed so a builtin can spawn sibling interpreters
    /// over the same shared `&Program` (used by `parallel_map`'s scoped threads).
    program: &'a Program,
    /// An owned share of the same program, handed by `.clone()` to detached
    /// threads created by `spawn` so they can build their own interpreter over
    /// `&*arc` and outlive the `spawn` call. Separate handle, not self-referential.
    program_arc: Arc<Program>,
    functions: HashMap<&'a str, &'a Function>,
    /// The running program's CLI arguments, exposed by the `args()` builtin.
    program_args: Vec<String>,
    /// Declared struct types: name -> ordered field names, used to build struct
    /// values from positional construction arguments.
    structs: HashMap<&'a str, Vec<String>>,
    /// Enum variant name -> owning enum name. Variant names are globally unique,
    /// so this resolves both unit and payload construction.
    variants: HashMap<&'a str, &'a str>,
    heap: Vec<Option<Value>>,
    /// Ownership counts for reference-counted (`rc<T>`) heap slots, keyed by
    /// slot index. Slots not present here are raw pointers / plain allocations.
    refcounts: HashMap<usize, usize>,
    /// Per-runtime table of open network sockets. A `Value::Socket(i)` indexes
    /// this vector; closing a socket sets its slot to `None`, mirroring the heap.
    sockets: Vec<Option<SocketResource>>,
    call_stack: Vec<TraceFrame>,
    /// Trait-method dispatch table: `(receiver type name, method name)` -> the
    /// impl function. Built once from every `impl Trait for Type` block.
    impl_methods: HashMap<(String, String), &'a Function>,
    /// Names that are trait methods (declared in some `trait`). A call to one of
    /// these dispatches on the receiver's runtime type via `impl_methods`.
    trait_method_names: HashSet<String>,
    /// Names of `async fn` functions. Calling one spawns an OS thread running its
    /// body and yields a `Value::Future` that `await` resolves.
    async_functions: HashSet<&'a str>,
    /// Names of `extern fn` (C-ABI) functions. The interpreter cannot execute C,
    /// so a call to one raises `L0423` before any builtin/user dispatch.
    extern_functions: HashSet<&'a str>,
}

impl<'a> Runtime<'a> {
    /// Build an interpreter over the borrowed program `program` while retaining an
    /// owned `Arc<Program>` (`program_arc`) that points at the same data, used
    /// only to hand a share to detached `spawn`ed threads. The caller passes both
    /// handles (e.g. `Runtime::new(&arc, Arc::clone(&arc))`).
    fn new(program: &'a Program, program_arc: Arc<Program>) -> Result<Self, RuntimeError> {
        let functions = program
            .functions
            .iter()
            .map(|function| (function.name.as_str(), function))
            .collect::<HashMap<_, _>>();

        // Build the trait-method dispatch table from all impl blocks and record
        // the set of trait method names so calls can be recognized.
        let mut impl_methods = HashMap::new();
        let mut trait_method_names = HashSet::new();
        for decl in &program.traits {
            for method in &decl.methods {
                trait_method_names.insert(method.name.clone());
            }
        }
        for decl in &program.impls {
            for method in &decl.methods {
                impl_methods.insert((decl.type_name.clone(), method.name.clone()), method);
            }
        }

        let structs = program
            .structs
            .iter()
            .map(|declaration| {
                (
                    declaration.name.as_str(),
                    declaration
                        .fields
                        .iter()
                        .map(|field| field.name.clone())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();

        let mut variants = HashMap::new();
        // Built-in `option`/`result` generic-enum variants. Registered like user
        // variants so construction and `match` reuse the same `Value::Enum` path.
        variants.insert("some", "option");
        variants.insert("none", "option");
        variants.insert("ok", "result");
        variants.insert("err", "result");
        for declaration in &program.enums {
            for variant in &declaration.variants {
                variants.insert(variant.name.as_str(), declaration.name.as_str());
            }
        }

        let async_functions = program
            .functions
            .iter()
            .filter(|function| function.is_async)
            .map(|function| function.name.as_str())
            .collect::<HashSet<_>>();

        let extern_functions = program
            .functions
            .iter()
            .filter(|function| function.is_extern)
            .map(|function| function.name.as_str())
            .collect::<HashSet<_>>();

        Ok(Self {
            program,
            program_arc,
            functions,
            program_args: Vec::new(),
            structs,
            variants,
            heap: Vec::new(),
            refcounts: HashMap::new(),
            sockets: Vec::new(),
            call_stack: Vec::new(),
            impl_methods,
            trait_method_names,
            async_functions,
            extern_functions,
        })
    }

    /// Spawn an `async fn` call on a new OS thread that owns a share of the
    /// program (an `Arc<Program>` clone) and builds its own interpreter, then
    /// return a `Value::Future` handle so `await` retrieves the produced value.
    /// The argument values are already evaluated and are `Send`, so they cross
    /// the thread boundary safely; heaps are per-thread.
    fn spawn_async(&self, name: &str, args: Vec<Value>) -> Value {
        let arc = Arc::clone(&self.program_arc);
        let func_name = name.to_string();
        let handle = std::thread::spawn(move || {
            let mut runtime = Runtime::new(&arc, Arc::clone(&arc))?;
            runtime.call_function(&func_name, args)
        });
        Value::Future(Future {
            handle: Arc::new(Mutex::new(Some(handle))),
        })
    }

    fn call_function(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // Trait-method dispatch: when `name` is a trait method, select the impl
        // by the receiver `args[0]`'s runtime type and invoke it. Because
        // generics are erased, a bounded-generic `v.show()` is the same lookup.
        if self.trait_method_names.contains(name) {
            let receiver_type = args.first().map(value_type_name).ok_or_else(|| {
                RuntimeError::new(
                    "L0401",
                    format!("trait method `{name}` called without a receiver"),
                )
            })?;
            let method = *self
                .impl_methods
                .get(&(receiver_type.clone(), name.to_string()))
                .ok_or_else(|| {
                    RuntimeError::new(
                        "L0401",
                        format!("type `{receiver_type}` does not implement trait method `{name}`"),
                    )
                })?;
            return self.invoke_function(method, args);
        }
        if let Some(enum_name) = self.variants.get(name) {
            return Ok(Value::Enum {
                enum_name: enum_name.to_string(),
                variant: name.to_string(),
                payload: args,
            });
        }
        if let Some(field_names) = self.structs.get(name) {
            return Ok(Value::Struct {
                name: name.to_string(),
                fields: field_names.iter().cloned().zip(args).collect(),
            });
        }
        match name {
            "alloc" => self.builtin_alloc(args),
            "load" => self.builtin_load(args),
            "store" => self.builtin_store(args),
            "dealloc" => self.builtin_dealloc(args),
            "read_file" => self.builtin_read_file(args),
            "write_file" => self.builtin_write_file(args),
            "append_file" => self.builtin_append_file(args),
            "file_exists" => self.builtin_file_exists(args),
            "read_lines" => self.builtin_read_lines(args),
            "read_bytes" => self.builtin_read_bytes(args),
            "write_bytes" => self.builtin_write_bytes(args),
            "file_size" => self.builtin_file_size(args),
            "is_file" => self.builtin_is_file(args),
            "is_dir" => self.builtin_is_dir(args),
            "list_dir" => self.builtin_list_dir(args),
            "make_dir" => self.builtin_make_dir(args),
            "remove_file" => self.builtin_remove_file(args),
            "remove_dir" => self.builtin_remove_dir(args),
            "sys_status" => self.builtin_sys_status(args),
            "sys_output" => self.builtin_sys_output(args),
            "print" => self.builtin_print("print", args, false),
            "println" => self.builtin_print("println", args, true),
            "warn" => self.builtin_warn(args),
            "wasm_log" => self.builtin_wasm_log(args),
            "console_log" => self.builtin_console_log(args),
            "dom_set_text" => self.builtin_dom_set_text(args),
            "flush" => self.builtin_flush(args),
            "mono_now" => Self::builtin_mono_now(args),
            "wall_now" => Self::builtin_wall_now(args),
            "sleep_millis" => Self::builtin_sleep_millis(args),
            "assert" => Self::builtin_assert(args),
            "to_string" => Self::builtin_to_string(args),
            "char_code" => Self::builtin_char_code(args),
            "char_from" => Self::builtin_char_from(args),
            "byte" => Self::builtin_byte(args),
            "byte_val" => Self::builtin_byte_val(args),
            "len" => Self::builtin_len(args),
            "list_new" => Self::builtin_list_new(args),
            "push" => Self::builtin_push(args),
            "get" => Self::builtin_get(args),
            "set" => Self::builtin_set(args),
            "pop" => Self::builtin_pop(args),
            "reverse" => Self::builtin_reverse(args),
            "concat" => Self::builtin_concat(args),
            "slice" => Self::builtin_slice(args),
            "map_new" => Self::builtin_map_new(args),
            "map_set" => Self::builtin_map_set(args),
            "map_get" => Self::builtin_map_get(args),
            "map_has" => Self::builtin_map_has(args),
            "map_len" => Self::builtin_map_len(args),
            "map_del" => Self::builtin_map_del(args),
            "substring" => Self::builtin_substring(args),
            "find" => Self::builtin_find(args),
            "contains" => Self::builtin_contains(args),
            "starts_with" => Self::builtin_starts_with(args),
            "ends_with" => Self::builtin_ends_with(args),
            "repeat" => Self::builtin_repeat(args),
            "split" => Self::builtin_split(args),
            "join" => Self::builtin_join(args),
            "trim" => Self::builtin_trim(args),
            "replace" => Self::builtin_replace(args),
            "upper" => Self::builtin_upper(args),
            "lower" => Self::builtin_lower(args),
            "to_bytes" => Self::builtin_to_bytes(args),
            "from_bytes" => Self::builtin_from_bytes(args),
            "byte_len" => Self::builtin_byte_len(args),
            "abs" => Self::builtin_abs(args),
            "min" => Self::builtin_min(args),
            "max" => Self::builtin_max(args),
            "pow" => Self::builtin_pow(args),
            "sqrt" => Self::builtin_sqrt(args),
            "floor" => Self::builtin_floor(args),
            "ceil" => Self::builtin_ceil(args),
            "round" => Self::builtin_round(args),
            "sin" => Self::builtin_unary_f64("sin", args, f64::sin),
            "cos" => Self::builtin_unary_f64("cos", args, f64::cos),
            "tan" => Self::builtin_unary_f64("tan", args, f64::tan),
            "atan" => Self::builtin_unary_f64("atan", args, f64::atan),
            "exp" => Self::builtin_unary_f64("exp", args, f64::exp),
            "ln" => Self::builtin_unary_f64("ln", args, f64::ln),
            "log10" => Self::builtin_unary_f64("log10", args, f64::log10),
            "atan2" => Self::builtin_atan2(args),
            "rotate_left" => Self::builtin_rotate_left(args),
            "rotate_right" => Self::builtin_rotate_right(args),
            "count_ones" => Self::builtin_count_ones(args),
            "leading_zeros" => Self::builtin_leading_zeros(args),
            "trailing_zeros" => Self::builtin_trailing_zeros(args),
            "reverse_bytes" => Self::builtin_reverse_bytes(args),
            "rc_new" => self.builtin_rc_new(args),
            "rc_clone" => self.builtin_rc_clone(args),
            "rc_release" => self.builtin_rc_release(args),
            "rc_get" | "ref_get" | "ptr_read" => self.builtin_ref_get(name, args),
            "rc_borrow" => self.builtin_rc_borrow(args),
            "ptr_write" => self.builtin_store(args),
            "env" => Self::builtin_env(args),
            "os_random" => Self::builtin_os_random(args),
            "args" => self.builtin_args(args),
            "parallel_map" => self.builtin_parallel_map(args),
            "chan_new" => Self::builtin_chan_new(args),
            "send" => Self::builtin_send(args),
            "recv" => Self::builtin_recv(args),
            "try_recv" => Self::builtin_try_recv(args),
            "spawn" => self.builtin_spawn(args),
            "task_join" => Self::builtin_task_join(args),
            "mutex_new" => Self::builtin_mutex_new(args),
            "mutex_get" => Self::builtin_mutex_get(args),
            "mutex_set" => Self::builtin_mutex_set(args),
            "mutex_add" => Self::builtin_mutex_add(args),
            "tcp_connect" => self.builtin_tcp_connect(args),
            "tcp_listen" => self.builtin_tcp_listen(args),
            "tcp_accept" => self.builtin_tcp_accept(args),
            "tcp_read" => self.builtin_tcp_read(args),
            "tcp_write" => self.builtin_tcp_write(args),
            "tcp_shutdown" => self.builtin_tcp_shutdown(args),
            "tcp_close" => self.builtin_socket_close(args),
            "udp_bind" => self.builtin_udp_bind(args),
            "udp_send_to" => self.builtin_udp_send_to(args),
            "udp_recv" => self.builtin_udp_recv(args),
            "http_get" => Self::builtin_http_get(args),
            "http_post" => Self::builtin_http_post(args),
            _ => {
                let function = *self.functions.get(name).ok_or_else(|| {
                    RuntimeError::new("L0401", format!("unknown function `{name}`"))
                })?;
                self.invoke_function(function, args)
            }
        }
    }

    /// `env(name string) -> option<string>`: `some(value)` when the environment
    /// variable is set, `none` otherwise.
    fn builtin_env(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [name]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("env", 1, args.len()))?;
        let name = expect_string("env", name)?;
        Ok(option_value(std::env::var(&name).ok().map(Value::String)))
    }

    /// `os_random(len i64) -> result<list<byte>, string>`: `len`
    /// cryptographically-secure random bytes from the operating-system CSPRNG as
    /// `ok(list<byte>)`, or `err(message)` if the OS RNG fails. `len == 0`
    /// returns `ok([])`; `len < 0` returns `err("os_random length must be
    /// non-negative")`. Never a seeded/deterministic PRNG and never a panic.
    /// Routes through the shared [`os_random_bytes`] helper so every backend
    /// agrees on behavior.
    fn builtin_os_random(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [len]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("os_random", 1, args.len()))?;
        let len = expect_i64("os_random", len)?;
        Ok(result_value(match os_random_bytes(len) {
            Ok(bytes) => Ok(Value::Array(bytes.into_iter().map(Value::Byte).collect())),
            Err(message) => Err(Value::String(message)),
        }))
    }

    /// `args() -> list<string>`: the running program's CLI arguments (an empty
    /// list when none were passed), represented as an array of strings.
    fn builtin_args(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("args", 0, args.len()))?;
        Ok(Value::Array(
            self.program_args
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ))
    }

    /// `parallel_map(f fn(i64) -> i64, args list<i64>) -> list<i64>`: evaluate
    /// `f(arg)` for every element of `args` concurrently on separate OS threads,
    /// returning the results in the SAME order as `args`. Each thread builds a
    /// fresh sibling interpreter over the shared `&Program` (heaps are per-thread,
    /// so there is no shared mutable state and no locking). Output order follows
    /// input order, so results are fully deterministic regardless of scheduling.
    fn builtin_parallel_map(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [callee, elements]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("parallel_map", 2, args.len()))?;
        let func_name = match callee {
            Value::Func(name) => name,
            other => {
                return Err(RuntimeError::new(
                    "L0417",
                    format!("parallel_map expects a function but got `{other}`"),
                ));
            }
        };
        let arg_values = expect_list("parallel_map", elements)?;

        let program = self.program;
        let program_arc = &self.program_arc;
        let results: Vec<Value> = std::thread::scope(|scope| {
            let handles: Vec<_> = arg_values
                .iter()
                .map(|value| {
                    let name = func_name.clone();
                    let value = value.clone();
                    let arc = Arc::clone(program_arc);
                    scope.spawn(move || {
                        let mut runtime = Runtime::new(program, arc)?;
                        runtime.call_function(&name, vec![value])
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|_| {
                        Err(RuntimeError::new(
                            "L0401",
                            "parallel_map worker thread panicked",
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        })?;

        Ok(Value::Array(results))
    }

    /// `chan_new() -> Chan`: create an unbounded `i64` message-passing channel.
    fn builtin_chan_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("chan_new", 0, args.len()))?;
        Ok(new_chan())
    }

    /// `send(ch Chan, v i64) -> void`: enqueue `v` (never blocks; unbounded).
    fn builtin_send(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [chan, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("send", 2, args.len()))?;
        let chan = expect_chan("send", chan)?;
        let value = expect_i64("send", value)?;
        // A send fails only if every receiver has been dropped. Because a channel
        // shares its receiver behind an `Arc`, the sender-side handle keeps it
        // alive; report a clear runtime error rather than panicking otherwise.
        chan.sender
            .send(Value::I64(value))
            .map_err(|_| RuntimeError::new("L0401", "send on a channel with no live receiver"))?;
        Ok(Value::Void)
    }

    /// `recv(ch Chan) -> i64`: dequeue, blocking until a value is available.
    fn builtin_recv(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [chan]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("recv", 1, args.len()))?;
        let chan = expect_chan("recv", chan)?;
        let receiver = chan
            .receiver
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "recv on a poisoned channel"))?;
        receiver
            .recv()
            .map_err(|_| RuntimeError::new("L0401", "recv on a closed, empty channel"))
    }

    /// `try_recv(ch Chan) -> option<i64>`: non-blocking; `some(v)` or `none`.
    fn builtin_try_recv(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [chan]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("try_recv", 1, args.len()))?;
        let chan = expect_chan("try_recv", chan)?;
        let receiver = chan
            .receiver
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "try_recv on a poisoned channel"))?;
        Ok(option_value(receiver.try_recv().ok()))
    }

    /// `spawn(f fn(Chan, i64) -> void, ch Chan, v i64) -> Task`: run `f(ch, v)` on
    /// a detached OS thread that owns a share of the program (an `Arc<Program>`
    /// clone) and builds its own interpreter over `&*arc`, then returns a one-shot
    /// `Task` handle so the thread is `join`ed exactly once.
    fn builtin_spawn(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [callee, chan, value]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("spawn", 3, args.len()))?;
        let func_name = match callee {
            Value::Func(name) => name,
            other => {
                return Err(RuntimeError::new(
                    "L0417",
                    format!("spawn expects a function but got `{other}`"),
                ));
            }
        };
        // `spawn`'s fixed arg shape is `(Chan, i64)`; validate before detaching.
        let chan = expect_chan("spawn", chan)?;
        let value = expect_i64("spawn", value)?;
        // Hand the detached thread an owned share of the program so it can outlive
        // this call and build its own interpreter over `&*arc` independently.
        let arc = Arc::clone(&self.program_arc);
        let handle = std::thread::spawn(move || {
            let mut runtime = Runtime::new(&arc, Arc::clone(&arc))?;
            runtime.call_function(&func_name, vec![Value::Chan(chan), Value::I64(value)])
        });
        Ok(Value::Task(Task {
            handle: Arc::new(Mutex::new(Some(handle))),
        }))
    }

    /// `task_join(t Task) -> void`: wait for the spawned thread. A second
    /// `task_join` on an already-joined handle is a harmless no-op. (Named
    /// `task_join` rather than `join` because `join` is already the string-list
    /// joiner builtin.)
    fn builtin_task_join(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [task]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("task_join", 1, args.len()))?;
        let task = expect_task("task_join", task)?;
        join_task(&task)
    }

    /// `mutex_new(v i64) -> Mutex`: a shared mutex over one `i64`.
    fn builtin_mutex_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_new", 1, args.len()))?;
        let value = expect_i64("mutex_new", value)?;
        Ok(Value::Mutex(SharedMutex {
            cell: Arc::new(Mutex::new(value)),
        }))
    }

    /// `mutex_get(m Mutex) -> i64`: lock, read, unlock.
    fn builtin_mutex_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [mutex]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_get", 1, args.len()))?;
        let mutex = expect_mutex("mutex_get", mutex)?;
        let guard = mutex
            .cell
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "mutex_get on a poisoned mutex"))?;
        Ok(Value::I64(*guard))
    }

    /// `mutex_set(m Mutex, v i64) -> void`: lock, write, unlock.
    fn builtin_mutex_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [mutex, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_set", 2, args.len()))?;
        let mutex = expect_mutex("mutex_set", mutex)?;
        let value = expect_i64("mutex_set", value)?;
        let mut guard = mutex
            .cell
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "mutex_set on a poisoned mutex"))?;
        *guard = value;
        Ok(Value::Void)
    }

    /// `mutex_add(m Mutex, delta i64) -> i64`: lock, `v += delta`, return the new
    /// value, unlock — an atomic read-modify-write so worker threads accumulate
    /// safely.
    fn builtin_mutex_add(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [mutex, delta]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_add", 2, args.len()))?;
        let mutex = expect_mutex("mutex_add", mutex)?;
        let delta = expect_i64("mutex_add", delta)?;
        let mut guard = mutex
            .cell
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "mutex_add on a poisoned mutex"))?;
        *guard = guard.wrapping_add(delta);
        Ok(Value::I64(*guard))
    }

    /// Push a freshly opened socket resource into the handle table, returning its
    /// index wrapped as a `Value::Socket`.
    fn register_socket(&mut self, resource: SocketResource) -> Value {
        self.sockets.push(Some(resource));
        Value::Socket(self.sockets.len() - 1)
    }

    /// Resolve a socket handle argument to its live slot index, reporting a
    /// wrong-argument-type error for a non-socket value and a stale-handle error
    /// for a closed or invalid slot.
    fn socket_slot(&self, name: &str, value: &Value) -> Result<usize, RuntimeError> {
        let Value::Socket(handle) = value else {
            return Err(RuntimeError::new(
                "L0417",
                format!("{name} expects a Socket but got `{value}`"),
            ));
        };
        match self.sockets.get(*handle) {
            Some(Some(_)) => Ok(*handle),
            _ => Err(RuntimeError::new(
                "L0406",
                format!("{name} received a closed or invalid socket `{handle}`"),
            )),
        }
    }

    /// `tcp_connect(host string, port i64) -> result<Socket, string>`.
    fn builtin_tcp_connect(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [host, port]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_connect", 2, args.len()))?;
        let host = expect_string("tcp_connect", host)?;
        let port = expect_i64("tcp_connect", port)?;
        match TcpStream::connect((host.as_str(), port as u16)) {
            Ok(stream) => {
                let socket = self.register_socket(SocketResource::Stream(stream));
                Ok(result_value(Ok(socket)))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_listen(host string, port i64) -> result<Socket, string>`.
    fn builtin_tcp_listen(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [host, port]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_listen", 2, args.len()))?;
        let host = expect_string("tcp_listen", host)?;
        let port = expect_i64("tcp_listen", port)?;
        match TcpListener::bind((host.as_str(), port as u16)) {
            Ok(listener) => {
                let socket = self.register_socket(SocketResource::Listener(listener));
                Ok(result_value(Ok(socket)))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_accept(listener Socket) -> result<Socket, string>`: block for a
    /// connection and register the accepted stream as a new handle.
    fn builtin_tcp_accept(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [listener]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_accept", 1, args.len()))?;
        let slot = self.socket_slot("tcp_accept", &listener)?;
        let accepted = match &self.sockets[slot] {
            Some(SocketResource::Listener(listener)) => listener.accept(),
            _ => {
                return Ok(result_value(Err(Value::String(
                    "tcp_accept requires a listening socket".to_string(),
                ))));
            }
        };
        match accepted {
            Ok((stream, _addr)) => {
                let socket = self.register_socket(SocketResource::Stream(stream));
                Ok(result_value(Ok(socket)))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_read(conn Socket) -> result<string, string>`: read up to 4096 bytes
    /// and return them as a lossy UTF-8 string (empty on clean EOF).
    fn builtin_tcp_read(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Read;
        let [conn]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_read", 1, args.len()))?;
        let slot = self.socket_slot("tcp_read", &conn)?;
        let mut buffer = [0u8; 4096];
        let read = match &mut self.sockets[slot] {
            Some(SocketResource::Stream(stream)) => stream.read(&mut buffer),
            _ => {
                return Ok(result_value(Err(Value::String(
                    "tcp_read requires a connected stream socket".to_string(),
                ))));
            }
        };
        match read {
            Ok(count) => Ok(result_value(Ok(Value::String(
                String::from_utf8_lossy(&buffer[..count]).into_owned(),
            )))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_write(conn Socket, data string) -> result<i64, string>`: write the
    /// string's bytes and return the number of bytes written.
    fn builtin_tcp_write(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [conn, data]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_write", 2, args.len()))?;
        let slot = self.socket_slot("tcp_write", &conn)?;
        let data = expect_string("tcp_write", data)?;
        let bytes = data.as_bytes();
        let written = match &mut self.sockets[slot] {
            Some(SocketResource::Stream(stream)) => {
                // Write the FULL buffer (short writes are possible) and flush.
                stream.write_all(bytes).and_then(|()| stream.flush())
            }
            _ => {
                return Ok(result_value(Err(Value::String(
                    "tcp_write requires a connected stream socket".to_string(),
                ))));
            }
        };
        match written {
            Ok(()) => Ok(result_value(Ok(Value::I64(bytes.len() as i64)))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_shutdown(conn Socket) -> void`: gracefully shut down the write half
    /// of the connection (`Shutdown::Write`), signaling EOF to the peer so any
    /// buffered response is delivered before the socket is dropped. Shutting down
    /// a non-stream or already-closed handle is a no-op.
    fn builtin_tcp_shutdown(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::net::Shutdown;
        let [socket]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_shutdown", 1, args.len()))?;
        if let Value::Socket(handle) = socket {
            if let Some(Some(SocketResource::Stream(stream))) = self.sockets.get(handle) {
                let _ = stream.shutdown(Shutdown::Write);
            }
            Ok(Value::Void)
        } else {
            Err(RuntimeError::new(
                "L0417",
                format!("tcp_shutdown expects a Socket but got `{socket}`"),
            ))
        }
    }

    /// `tcp_close(conn Socket) -> void` / `udp_close`: drop the handle, freeing
    /// its table slot. Closing an already-closed handle is a no-op.
    fn builtin_socket_close(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [socket]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_close", 1, args.len()))?;
        if let Value::Socket(handle) = socket {
            if let Some(slot) = self.sockets.get_mut(handle) {
                *slot = None;
            }
            Ok(Value::Void)
        } else {
            Err(RuntimeError::new(
                "L0417",
                format!("tcp_close expects a Socket but got `{socket}`"),
            ))
        }
    }

    /// `udp_bind(host string, port i64) -> result<Socket, string>`.
    fn builtin_udp_bind(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [host, port]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("udp_bind", 2, args.len()))?;
        let host = expect_string("udp_bind", host)?;
        let port = expect_i64("udp_bind", port)?;
        match UdpSocket::bind((host.as_str(), port as u16)) {
            Ok(socket) => {
                let handle = self.register_socket(SocketResource::Udp(socket));
                Ok(result_value(Ok(handle)))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `udp_send_to(sock Socket, data string, host string, port i64)
    /// -> result<i64, string>`: send one datagram, returning the byte count.
    fn builtin_udp_send_to(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [sock, data, host, port]: [Value; 4] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("udp_send_to", 4, args.len()))?;
        let slot = self.socket_slot("udp_send_to", &sock)?;
        let data = expect_string("udp_send_to", data)?;
        let host = expect_string("udp_send_to", host)?;
        let port = expect_i64("udp_send_to", port)?;
        let sent = match &self.sockets[slot] {
            Some(SocketResource::Udp(socket)) => {
                socket.send_to(data.as_bytes(), (host.as_str(), port as u16))
            }
            _ => {
                return Ok(result_value(Err(Value::String(
                    "udp_send_to requires a UDP socket".to_string(),
                ))));
            }
        };
        match sent {
            Ok(count) => Ok(result_value(Ok(Value::I64(count as i64)))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `udp_recv(sock Socket) -> result<string, string>`: receive one datagram,
    /// dropping the sender address, and return it as a lossy UTF-8 string.
    fn builtin_udp_recv(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [sock]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("udp_recv", 1, args.len()))?;
        let slot = self.socket_slot("udp_recv", &sock)?;
        let mut buffer = [0u8; 4096];
        let received = match &self.sockets[slot] {
            Some(SocketResource::Udp(socket)) => socket.recv_from(&mut buffer),
            _ => {
                return Ok(result_value(Err(Value::String(
                    "udp_recv requires a UDP socket".to_string(),
                ))));
            }
        };
        match received {
            Ok((count, _addr)) => Ok(result_value(Ok(Value::String(
                String::from_utf8_lossy(&buffer[..count]).into_owned(),
            )))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `http_get(url string) -> result<string, string>`: perform an HTTP/1.1
    /// GET and return the response body on a 2xx/3xx response, or `err(message)`
    /// on a connection/parse/HTTP error.
    fn builtin_http_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [url]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("http_get", 1, args.len()))?;
        let url = expect_string("http_get", url)?;
        Ok(http_exchange("GET", &url, None))
    }

    /// `http_post(url string, body string) -> result<string, string>`: perform
    /// an HTTP/1.1 POST with a `text/plain` body and return the response body on
    /// a 2xx/3xx response, or `err(message)` on a connection/parse/HTTP error.
    fn builtin_http_post(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [url, body]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("http_post", 2, args.len()))?;
        let url = expect_string("http_post", url)?;
        let body = expect_string("http_post", body)?;
        Ok(http_exchange("POST", &url, Some(&body)))
    }

    /// Execute a user function (or trait impl method) with the given argument
    /// values, threading the traceback and translating loop-control escape.
    fn invoke_function(
        &mut self,
        function: &'a Function,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        if function.params.len() != args.len() {
            return Err(RuntimeError::new(
                "L0402",
                format!(
                    "function `{}` expects {} arguments but got {}",
                    function.name,
                    function.params.len(),
                    args.len()
                ),
            ));
        }

        let mut env = Env::default();
        for (param, value) in function.params.iter().zip(args) {
            env.define(param.name.clone(), value);
        }

        self.call_stack.push(TraceFrame {
            function: function.name.clone(),
            span: Some(function.span),
        });
        let result = self.eval_block(&function.body, &mut env);
        let traceback = self.call_stack.clone();
        self.call_stack.pop();

        match result.map_err(|error| error.with_traceback(traceback))? {
            Control::Return(value) | Control::Value(value) => Ok(value),
            Control::Break | Control::Continue => Err(RuntimeError::new(
                "L0410",
                "loop control escaped function body",
            )),
        }
    }

    fn eval_block(&mut self, statements: &[Stmt], env: &mut Env) -> Result<Control, RuntimeError> {
        let mut last = Value::Void;

        for statement in statements {
            match self.eval_statement(statement, env)? {
                Control::Return(value) => return Ok(Control::Return(value)),
                Control::Break => return Ok(Control::Break),
                Control::Continue => return Ok(Control::Continue),
                Control::Value(value) => last = value,
            }
        }

        Ok(Control::Value(last))
    }

    fn eval_statement(&mut self, statement: &Stmt, env: &mut Env) -> Result<Control, RuntimeError> {
        let span = statement_span(statement);
        let result = match statement {
            Stmt::Let { name, value, .. } => {
                let value = self.eval_expr(value, env)?;
                env.define(name.clone(), value);
                Ok(Control::Value(Value::Void))
            }
            Stmt::Assign {
                name,
                path,
                op,
                value,
                ..
            } => {
                let rhs = self.eval_expr(value, env)?;
                if path.is_empty() {
                    let new = match op {
                        AssignOp::Replace => rhs,
                        _ => apply_compound(env.get(name)?, op, rhs)?,
                    };
                    env.assign(name, new)?;
                } else {
                    let resolved = self.resolve_places(path, env)?;
                    let mut root = env.get(name)?;
                    let new = match op {
                        AssignOp::Replace => rhs,
                        _ => apply_compound(get_place(&root, &resolved)?, op, rhs)?,
                    };
                    set_place(&mut root, &resolved, new)?;
                    env.assign(name, root)?;
                }
                Ok(Control::Value(Value::Void))
            }
            Stmt::Return(expr) => {
                let value = expr
                    .as_ref()
                    .map(|expr| self.eval_expr(expr, env))
                    .unwrap_or(Ok(Value::Void))?;
                Ok(Control::Return(value))
            }
            Stmt::Break(_) => Ok(Control::Break),
            Stmt::Continue(_) => Ok(Control::Continue),
            // A `match` arrives wrapped in a `Stmt::Expr`; evaluate it here so its
            // arm blocks propagate control flow and produce a value like
            // `if`/`try`.
            Stmt::Expr(Expr {
                kind: ExprKind::Match { scrutinee, arms },
                ..
            }) => self.eval_match(scrutinee, arms, env),
            Stmt::Expr(expr) => self.eval_expr(expr, env).map(Control::Value),
            Stmt::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    let condition = self.eval_expr(&branch.condition, env)?;
                    if condition.as_bool()? {
                        return self.eval_scoped_block(&branch.body, env);
                    }
                }

                self.eval_scoped_block(else_body, env)
            }
            Stmt::While {
                condition, body, ..
            } => {
                while self.eval_expr(condition, env)?.as_bool()? {
                    match self.eval_scoped_block(body, env)? {
                        Control::Return(value) => return Ok(Control::Return(value)),
                        Control::Break => break,
                        Control::Continue | Control::Value(_) => {}
                    }
                }
                Ok(Control::Value(Value::Void))
            }
            Stmt::For {
                name,
                start,
                end,
                step,
                body,
                ..
            } => {
                let mut current = self.eval_expr(start, env)?.as_i64()?;
                let end = self.eval_expr(end, env)?.as_i64()?;
                let step = step
                    .as_ref()
                    .map(|expr| self.eval_expr(expr, env))
                    .unwrap_or(Ok(Value::I64(1)))?
                    .as_i64()?;
                if step == 0 {
                    return Err(RuntimeError::new("L0411", "for loop step cannot be zero"));
                }

                while if step > 0 {
                    current <= end
                } else {
                    current >= end
                } {
                    env.push_scope();
                    env.define(name.clone(), Value::I64(current));
                    let result = self.eval_block(body, env);
                    env.pop_scope();

                    match result? {
                        Control::Return(value) => return Ok(Control::Return(value)),
                        Control::Break => break,
                        Control::Continue | Control::Value(_) => {}
                    }

                    current += step;
                }
                Ok(Control::Value(Value::Void))
            }
            Stmt::Loop { body, .. } => {
                loop {
                    match self.eval_scoped_block(body, env)? {
                        Control::Return(value) => return Ok(Control::Return(value)),
                        Control::Break => break,
                        Control::Continue | Control::Value(_) => {}
                    }
                }
                Ok(Control::Value(Value::Void))
            }
            // `unsafe` is a transparent gate: its body runs in the enclosing
            // scope, matching IR lowering, which inlines the body.
            Stmt::Unsafe { body, .. } => self.eval_block(body, env),
            // Inline assembly emits raw machine code and can only run after native
            // codegen + linking; the AST interpreter cannot execute it, so reject
            // it with `L0425` (like `extern`'s `L0423`) rather than no-op.
            Stmt::Asm { .. } => Err(asm_interpreter_error()),
            // A region declaration is compile-time metadata; it has no runtime
            // effect in the current analysis-only region model.
            Stmt::Region(_) => Ok(Control::Value(Value::Void)),
            Stmt::Throw { value, .. } => {
                let message = self.eval_expr(value, env)?.as_string()?;
                Err(RuntimeError::new("L0420", message))
            }
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                ..
            } => match self.eval_scoped_block(body, env) {
                // Only user-thrown errors are recoverable; system errors propagate.
                Err(error) if error.code == "L0420" => {
                    env.push_scope();
                    env.define(catch_name.clone(), Value::String(error.message.clone()));
                    let result = self.eval_block(catch_body, env);
                    env.pop_scope();
                    result
                }
                other => other,
            },
        };
        result.map_err(|error| self.annotate_error(error, span))
    }

    fn eval_scoped_block(
        &mut self,
        statements: &[Stmt],
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        env.push_scope();
        let result = self.eval_block(statements, env);
        env.pop_scope();
        result
    }

    /// Evaluate a `match`: select the arm whose variant matches the scrutinee's
    /// enum value (or the `_` wildcard), bind payload values to the arm's locals
    /// in a child scope, and evaluate the arm block. Exhaustiveness is enforced
    /// at compile time, so a valid program always selects an arm.
    fn eval_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        let value = self.eval_expr(scrutinee, env)?;
        let Value::Enum {
            variant, payload, ..
        } = value
        else {
            return Err(RuntimeError::new(
                "L0383",
                "match scrutinee did not evaluate to an enum value",
            ));
        };
        for arm in arms {
            match &arm.pattern {
                MatchPattern::Wildcard => {
                    return self.eval_scoped_block(&arm.body, env);
                }
                MatchPattern::Variant { name, bindings } if name == &variant => {
                    env.push_scope();
                    for (binding, value) in bindings.iter().zip(payload.iter()) {
                        env.define(binding.clone(), value.clone());
                    }
                    let result = self.eval_block(&arm.body, env);
                    env.pop_scope();
                    return result;
                }
                MatchPattern::Variant { .. } => {}
            }
        }
        Err(RuntimeError::new(
            "L0384",
            format!("no match arm covered variant `{variant}`"),
        ))
    }

    /// Resolve a parser assignment path into concrete places, evaluating each
    /// array index expression against the current environment.
    fn resolve_places(
        &mut self,
        path: &[Place],
        env: &Env,
    ) -> Result<Vec<ResolvedPlace>, RuntimeError> {
        path.iter()
            .map(|place| match place {
                Place::Field(field) => Ok(ResolvedPlace::Field(field.clone())),
                Place::Index(expr) => {
                    Ok(ResolvedPlace::Index(self.eval_expr(expr, env)?.as_i64()?))
                }
            })
            .collect()
    }

    fn eval_expr(&mut self, expr: &Expr, env: &Env) -> Result<Value, RuntimeError> {
        let result = match &expr.kind {
            ExprKind::Field { target, field } => {
                let target = self.eval_expr(target, env)?;
                match target {
                    Value::Struct { fields, .. } => fields
                        .into_iter()
                        .find(|(name, _)| name == field)
                        .map(|(_, value)| value)
                        .ok_or_else(|| RuntimeError::new("L0371", format!("no field `{field}`"))),
                    _ => Err(RuntimeError::new(
                        "L0371",
                        format!("cannot access field `{field}` on non-struct value"),
                    )),
                }
            }
            ExprKind::Integer(value) => Ok(Value::I64(*value)),
            ExprKind::Float(value) => Ok(Value::F64(*value)),
            ExprKind::Bool(value) => Ok(Value::Bool(*value)),
            ExprKind::String(value) => Ok(Value::String(value.clone())),
            ExprKind::Char(value) => Ok(Value::Char(*value)),
            ExprKind::Array(values) => values
                .iter()
                .map(|value| self.eval_expr(value, env))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            ExprKind::Variable(name) => match env.get(name) {
                Ok(value) => Ok(value),
                Err(error) => {
                    // A bare name that is not a local but is a known enum variant
                    // constructs a unit variant.
                    if let Some(enum_name) = self.variants.get(name.as_str()) {
                        Ok(Value::Enum {
                            enum_name: enum_name.to_string(),
                            variant: name.clone(),
                            payload: Vec::new(),
                        })
                    } else if self.functions.contains_key(name.as_str()) {
                        // A bare name that is a known top-level function evaluates
                        // to a first-class function value.
                        Ok(Value::Func(name.clone()))
                    } else {
                        Err(error)
                    }
                }
            },
            ExprKind::Index { target, index } => {
                let target = self.eval_expr(target, env)?;
                let index = self.eval_expr(index, env)?.as_i64()?;
                let Value::Array(values) = target else {
                    return Err(RuntimeError::new("L0412", "index target is not an array"));
                };
                if index < 0 {
                    return Err(RuntimeError::new(
                        "L0413",
                        format!("array index `{index}` is out of bounds"),
                    ));
                }
                values.get(index as usize).cloned().ok_or_else(|| {
                    RuntimeError::new("L0413", format!("array index `{index}` is out of bounds"))
                })
            }
            ExprKind::Unary { op, expr } => {
                let value = self.eval_expr(expr, env)?;
                match op {
                    UnaryOp::Not => Ok(Value::Bool(!value.as_bool()?)),
                    // Bitwise NOT (one's complement) on an i64.
                    UnaryOp::BitNot => Ok(Value::I64(!value.as_i64()?)),
                }
            }
            ExprKind::Binary { left, op, right } => {
                if *op == BinaryOp::And {
                    let left = self.eval_expr(left, env)?.as_bool()?;
                    if !left {
                        return Ok(Value::Bool(false));
                    }
                    let right = self.eval_expr(right, env)?.as_bool()?;
                    return Ok(Value::Bool(right));
                }
                if *op == BinaryOp::Or {
                    let left = self.eval_expr(left, env)?.as_bool()?;
                    if left {
                        return Ok(Value::Bool(true));
                    }
                    let right = self.eval_expr(right, env)?.as_bool()?;
                    return Ok(Value::Bool(right));
                }
                let left = self.eval_expr(left, env)?;
                let right = self.eval_expr(right, env)?;
                self.eval_binary(left, *op, right)
            }
            ExprKind::Call { name, args } => {
                let values = args
                    .iter()
                    .map(|arg| self.eval_expr(arg, env))
                    .collect::<Result<Vec<_>, _>>()?;
                // A call name that is a local holding a function value dispatches
                // through that value: invoke the referenced top-level function.
                let target = match env.get(name) {
                    Ok(Value::Func(target)) => target,
                    _ => name.clone(),
                };
                // An `extern fn` (C-ABI) cannot run on the interpreter; it only
                // has meaning after native codegen + linking.
                if self.extern_functions.contains(target.as_str()) {
                    return Err(extern_call_error(&target));
                }
                // Calling an `async fn` spawns its body on a new OS thread and
                // yields a `Future` handle; a synchronous call runs inline.
                if self.async_functions.contains(target.as_str()) {
                    Ok(self.spawn_async(&target, values))
                } else {
                    self.call_function(&target, values)
                }
            }
            ExprKind::Await { expr } => {
                let value = self.eval_expr(expr, env)?;
                let future = expect_future("await", value)?;
                await_future(&future)
            }
            ExprKind::StructLiteral { name, fields } => {
                // Evaluate in source order, then reorder to the declared field
                // order so the constructed value matches positional construction.
                let mut evaluated = Vec::with_capacity(fields.len());
                for (field_name, value) in fields {
                    evaluated.push((field_name.clone(), self.eval_expr(value, env)?));
                }
                let order = self.structs.get(name.as_str()).ok_or_else(|| {
                    RuntimeError::new("L0372", format!("`{name}` is not a struct type"))
                })?;
                let ordered = order
                    .iter()
                    .map(|declared| {
                        evaluated
                            .iter()
                            .find(|(n, _)| n == declared)
                            .map(|(_, v)| v.clone())
                            .ok_or_else(|| {
                                RuntimeError::new(
                                    "L0372",
                                    format!("missing field `{declared}` for `{name}`"),
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                self.call_function(name, ordered)
            }
            // `match` normally arrives as a statement and is handled in
            // `eval_statement`; this path covers a `match` nested as a value and
            // evaluates its selected arm to a plain value.
            ExprKind::Match { scrutinee, arms } => {
                let mut child = env.clone();
                match self.eval_match(scrutinee, arms, &mut child)? {
                    Control::Value(value) | Control::Return(value) => Ok(value),
                    Control::Break | Control::Continue => Err(RuntimeError::new(
                        "L0410",
                        "loop control escaped a match arm",
                    )),
                }
            }
        };
        result.map_err(|error| self.annotate_error(error, expr.span))
    }

    fn annotate_error(&self, error: RuntimeError, span: Span) -> RuntimeError {
        let error = error.with_span(span);
        match self.call_stack.last() {
            Some(frame) => error
                .with_function(frame.function.clone())
                .with_traceback(self.call_stack.clone()),
            None => error,
        }
    }

    fn eval_binary(&self, left: Value, op: BinaryOp, right: Value) -> Result<Value, RuntimeError> {
        // Float arithmetic/comparison when both operands are f64 (IEEE 754
        // semantics: division by zero yields infinity/NaN, not an error).
        if let (Value::F64(l), Value::F64(r)) = (&left, &right) {
            let (l, r) = (*l, *r);
            return Ok(match op {
                BinaryOp::Add => Value::F64(l + r),
                BinaryOp::Subtract => Value::F64(l - r),
                BinaryOp::Multiply => Value::F64(l * r),
                BinaryOp::Divide => Value::F64(l / r),
                BinaryOp::Equal => Value::Bool(l == r),
                BinaryOp::NotEqual => Value::Bool(l != r),
                BinaryOp::Less => Value::Bool(l < r),
                BinaryOp::LessEqual => Value::Bool(l <= r),
                BinaryOp::Greater => Value::Bool(l > r),
                BinaryOp::GreaterEqual => Value::Bool(l >= r),
                BinaryOp::And | BinaryOp::Or => {
                    unreachable!("logical ops short-circuit in eval_expr")
                }
                BinaryOp::BitAnd
                | BinaryOp::BitOr
                | BinaryOp::BitXor
                | BinaryOp::Shl
                | BinaryOp::Shr => {
                    unreachable!("bitwise ops require i64 operands (rejected by semantics)")
                }
            });
        }
        match op {
            // `+` concatenates when both operands are strings; otherwise it adds i64s.
            BinaryOp::Add if matches!((&left, &right), (Value::String(_), Value::String(_))) => {
                Ok(Value::String(left.as_string()? + &right.as_string()?))
            }
            BinaryOp::Add => Ok(Value::I64(left.as_i64()? + right.as_i64()?)),
            BinaryOp::Subtract => Ok(Value::I64(left.as_i64()? - right.as_i64()?)),
            BinaryOp::Multiply => Ok(Value::I64(left.as_i64()? * right.as_i64()?)),
            BinaryOp::Divide => {
                let divisor = right.as_i64()?;
                if divisor == 0 {
                    Err(RuntimeError::new("L0404", "division by zero"))
                } else {
                    Ok(Value::I64(left.as_i64()? / divisor))
                }
            }
            BinaryOp::Equal => Ok(Value::Bool(left == right)),
            BinaryOp::NotEqual => Ok(Value::Bool(left != right)),
            // Char ordering compares by Unicode code point; byte ordering is
            // numeric. Both fall through to i64 ordering otherwise.
            BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual
                if scalar_order_keys(&left, &right).is_some() =>
            {
                let (l, r) = scalar_order_keys(&left, &right)
                    .expect("guarded by the match arm condition above");
                Ok(Value::Bool(match op {
                    BinaryOp::Less => l < r,
                    BinaryOp::LessEqual => l <= r,
                    BinaryOp::Greater => l > r,
                    BinaryOp::GreaterEqual => l >= r,
                    _ => unreachable!("guarded to ordering operators"),
                }))
            }
            BinaryOp::Less => Ok(Value::Bool(left.as_i64()? < right.as_i64()?)),
            BinaryOp::LessEqual => Ok(Value::Bool(left.as_i64()? <= right.as_i64()?)),
            BinaryOp::Greater => Ok(Value::Bool(left.as_i64()? > right.as_i64()?)),
            BinaryOp::GreaterEqual => Ok(Value::Bool(left.as_i64()? >= right.as_i64()?)),
            // Integer bitwise ops on two i64s. Shift amounts are masked to the
            // low 6 bits (`amount & 63`), so large/negative shifts are total and
            // deterministic (x86/Java `long` semantics) — never a runtime error.
            // `>>` is an arithmetic (sign-preserving) shift on the signed i64.
            BinaryOp::BitAnd => Ok(Value::I64(left.as_i64()? & right.as_i64()?)),
            BinaryOp::BitOr => Ok(Value::I64(left.as_i64()? | right.as_i64()?)),
            BinaryOp::BitXor => Ok(Value::I64(left.as_i64()? ^ right.as_i64()?)),
            BinaryOp::Shl => Ok(Value::I64(shift_left(left.as_i64()?, right.as_i64()?))),
            BinaryOp::Shr => Ok(Value::I64(shift_right(left.as_i64()?, right.as_i64()?))),
            BinaryOp::And | BinaryOp::Or => unreachable!("logical ops short-circuit in eval_expr"),
        }
    }

    fn builtin_alloc(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("alloc", 1, args.len()))?;
        self.heap.push(Some(value));
        Ok(Value::Ptr(self.heap.len() - 1))
    }

    fn builtin_load(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("load", 1, args.len()))?;
        let slot = ptr.as_ptr()?;
        self.heap
            .get(slot)
            .and_then(|value| value.clone())
            .ok_or_else(|| RuntimeError::new("L0406", format!("invalid pointer `{slot}`")))
    }

    fn builtin_store(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("store", 2, args.len()))?;
        let slot = ptr.as_ptr()?;
        let Some(target) = self.heap.get_mut(slot) else {
            return Err(RuntimeError::new(
                "L0406",
                format!("invalid pointer `{slot}`"),
            ));
        };
        if target.is_none() {
            return Err(RuntimeError::new(
                "L0406",
                format!("invalid pointer `{slot}`"),
            ));
        }
        *target = Some(value);
        Ok(Value::Void)
    }

    fn builtin_dealloc(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("dealloc", 1, args.len()))?;
        let slot = ptr.as_ptr()?;
        let Some(value) = self.heap.get_mut(slot) else {
            return Err(RuntimeError::new(
                "L0406",
                format!("invalid pointer `{slot}`"),
            ));
        };
        if value.take().is_none() {
            return Err(RuntimeError::new(
                "L0406",
                format!("invalid pointer `{slot}`"),
            ));
        }
        Ok(Value::Void)
    }

    fn builtin_read_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_file", 1, args.len()))?;
        let path = path.as_string()?;
        fs::read_to_string(&path)
            .map(Value::String)
            .map_err(|error| {
                RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
            })
    }

    fn builtin_write_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path, contents]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("write_file", 2, args.len()))?;
        let path = path.as_string()?;
        let contents = contents.as_string()?;
        fs::write(&path, contents)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to write `{path}`: {error}"))
            })
    }

    fn builtin_append_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path, contents]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("append_file", 2, args.len()))?;
        let path = path.as_string()?;
        let contents = contents.as_string()?;
        use std::io::Write;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(contents.as_bytes()))
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to append `{path}`: {error}"))
            })
    }

    fn builtin_file_exists(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("file_exists", 1, args.len()))?;
        Ok(Value::Bool(fs::metadata(path.as_string()?).is_ok()))
    }

    fn builtin_read_lines(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_lines", 1, args.len()))?;
        let path = path.as_string()?;
        let contents = fs::read_to_string(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        Ok(Value::Array(
            contents
                .lines()
                .map(|line| Value::String(line.to_string()))
                .collect(),
        ))
    }

    fn builtin_read_bytes(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_bytes", 1, args.len()))?;
        let path = path.as_string()?;
        let bytes = fs::read(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        Ok(Value::Array(bytes.into_iter().map(Value::Byte).collect()))
    }

    fn builtin_write_bytes(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path, data]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("write_bytes", 2, args.len()))?;
        let path = path.as_string()?;
        let bytes = Self::value_to_bytes("write_bytes", data)?;
        fs::write(&path, bytes)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to write `{path}`: {error}"))
            })
    }

    /// Convert a `list<byte>` (`Value::Array` of `Value::Byte`) to raw bytes,
    /// erroring on a non-array or a non-byte element.
    fn value_to_bytes(name: &str, value: Value) -> Result<Vec<u8>, RuntimeError> {
        let Value::Array(values) = value else {
            return Err(RuntimeError::new(
                "L0418",
                format!("{name} expects a `list<byte>` value"),
            ));
        };
        values
            .into_iter()
            .map(|element| match element {
                Value::Byte(b) => Ok(b),
                other => Err(RuntimeError::new(
                    "L0418",
                    format!("{name} expects `list<byte>` but found `{other}`"),
                )),
            })
            .collect()
    }

    fn builtin_file_size(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("file_size", 1, args.len()))?;
        let path = path.as_string()?;
        let metadata = fs::metadata(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        Ok(Value::I64(metadata.len() as i64))
    }

    fn builtin_is_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("is_file", 1, args.len()))?;
        Ok(Value::Bool(
            fs::metadata(path.as_string()?)
                .map(|m| m.is_file())
                .unwrap_or(false),
        ))
    }

    fn builtin_is_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("is_dir", 1, args.len()))?;
        Ok(Value::Bool(
            fs::metadata(path.as_string()?)
                .map(|m| m.is_dir())
                .unwrap_or(false),
        ))
    }

    fn builtin_list_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_dir", 1, args.len()))?;
        let path = path.as_string()?;
        let entries = fs::read_dir(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
            })?;
            names.push(Value::String(
                entry.file_name().to_string_lossy().to_string(),
            ));
        }
        Ok(Value::Array(names))
    }

    fn builtin_make_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("make_dir", 1, args.len()))?;
        let path = path.as_string()?;
        fs::create_dir_all(&path)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to create `{path}`: {error}"))
            })
    }

    fn builtin_remove_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("remove_file", 1, args.len()))?;
        let path = path.as_string()?;
        fs::remove_file(&path)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to remove `{path}`: {error}"))
            })
    }

    fn builtin_remove_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("remove_dir", 1, args.len()))?;
        let path = path.as_string()?;
        fs::remove_dir(&path)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to remove `{path}`: {error}"))
            })
    }

    fn builtin_sys_status(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [program, command_args]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sys_status", 2, args.len()))?;
        let program = program.as_string()?;
        let command_args = command_args.as_string_array()?;
        let output = Command::new(&program)
            .args(command_args)
            .output()
            .map_err(|error| {
                RuntimeError::resource("L0416", format!("failed to run `{program}`: {error}"))
            })?;
        Ok(Value::I64(output.status.code().unwrap_or(-1).into()))
    }

    fn builtin_sys_output(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [program, command_args]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sys_output", 2, args.len()))?;
        let program = program.as_string()?;
        let command_args = command_args.as_string_array()?;
        let output = Command::new(&program)
            .args(command_args)
            .output()
            .map_err(|error| {
                RuntimeError::resource("L0416", format!("failed to run `{program}`: {error}"))
            })?;
        Ok(Value::String(
            String::from_utf8_lossy(&output.stdout).to_string(),
        ))
    }

    fn builtin_print(
        &self,
        name: &'static str,
        args: Vec<Value>,
        newline: bool,
    ) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        let text = text.as_string()?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let result = if newline {
            writeln!(handle, "{text}")
        } else {
            write!(handle, "{text}")
        };
        result.map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    fn builtin_warn(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("warn", 1, args.len()))?;
        let text = text.as_string()?;
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        writeln!(handle, "{text}").map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stderr: {error}"))
        })?;
        Ok(Value::Void)
    }

    fn builtin_flush(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        if !args.is_empty() {
            return Err(Self::wrong_arity("flush", 0, args.len()));
        }
        std::io::stdout().flush().map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to flush stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    /// `mono_now() -> i64`: nanoseconds since a fixed per-process monotonic
    /// baseline. Non-decreasing within a run. Shares the baseline with every
    /// other backend through [`monotonic_now_nanos`].
    fn builtin_mono_now(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mono_now", 0, args.len()))?;
        Ok(Value::I64(monotonic_now_nanos()))
    }

    /// `wall_now() -> i64`: milliseconds since the Unix epoch (wall-clock time).
    fn builtin_wall_now(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("wall_now", 0, args.len()))?;
        Ok(Value::I64(wall_now_millis()))
    }

    /// `sleep_millis(ms i64) -> void`: sleep the current thread for `ms`
    /// milliseconds; a negative `ms` sleeps for zero (no error).
    fn builtin_sleep_millis(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ms]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sleep_millis", 1, args.len()))?;
        let ms = expect_i64("sleep_millis", ms)?;
        sleep_millis(ms);
        Ok(Value::Void)
    }

    /// `wasm_log(x i64) -> void`: the host log builtin. On the WASM backend it
    /// lowers to a `call` of the imported `env.log_i64`; on the interpreters it
    /// prints the value as a stdout line so all backends observe the same side
    /// effect and the parity harness stays green.
    fn builtin_wasm_log(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("wasm_log", 1, args.len()))?;
        let value = expect_i64("wasm_log", value)?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{value}").map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    /// `console_log(s string) -> void`: the JS/DOM host console builtin. On the
    /// WASM backend it lowers to a `call` of the imported
    /// `env.console_log(ptr, len)` (a browser host implements it as
    /// `console.log`); on the interpreters it prints the string as a stdout line
    /// so all backends observe the same side effect and the parity harness stays
    /// green.
    fn builtin_console_log(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("console_log", 1, args.len()))?;
        let text = text.as_string()?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{text}").map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    /// `dom_set_text(id string, text string) -> void`: the DOM-write primitive. On
    /// the WASM backend it lowers to a `call` of the imported
    /// `env.dom_set_text(id_ptr, id_len, text_ptr, text_len)` (a browser host
    /// implements it as `document.getElementById(id).textContent = text`); on the
    /// interpreters it prints the deterministic line `id=text` so all backends
    /// observe the same side effect and the parity harness stays green.
    fn builtin_dom_set_text(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [id, text]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("dom_set_text", 2, args.len()))?;
        let id = id.as_string()?;
        let text = text.as_string()?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{id}={text}").map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    /// `assert(cond bool) -> void`: raises a catchable user-error (the same code
    /// `L0420` a `throw` produces, so `try`/`catch` recovers it) with the message
    /// `assertion failed` when `cond` is false; returns void when true.
    fn builtin_assert(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("assert", 1, args.len()))?;
        if value.as_bool()? {
            Ok(Value::Void)
        } else {
            Err(RuntimeError::new("L0420", "assertion failed"))
        }
    }

    fn builtin_to_string(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_string", 1, args.len()))?;
        match value {
            Value::I64(_)
            | Value::F64(_)
            | Value::Bool(_)
            | Value::String(_)
            | Value::Char(_)
            | Value::Byte(_) => Ok(Value::String(value.to_string())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("to_string cannot convert `{other}`"),
            )),
        }
    }

    /// `char_code(c char) -> i64`: the char's Unicode scalar value.
    fn builtin_char_code(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("char_code", 1, args.len()))?;
        match value {
            Value::Char(c) => Ok(Value::I64(c as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("char_code expects a char but got `{other}`"),
            )),
        }
    }

    /// `char_from(i i64) -> char`: the char for a Unicode scalar value; a runtime
    /// error when `i` is not a valid Unicode scalar.
    fn builtin_char_from(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("char_from", 1, args.len()))?;
        let code = expect_i64("char_from", value)?;
        u32::try_from(code)
            .ok()
            .and_then(char::from_u32)
            .map(Value::Char)
            .ok_or_else(|| {
                RuntimeError::new(
                    "L0417",
                    format!("char_from got `{code}`, which is not a valid Unicode scalar value"),
                )
            })
    }

    /// `byte(i i64) -> byte`: an 8-bit unsigned value; a runtime error outside 0-255.
    fn builtin_byte(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("byte", 1, args.len()))?;
        let number = expect_i64("byte", value)?;
        u8::try_from(number).map(Value::Byte).map_err(|_| {
            RuntimeError::new(
                "L0417",
                format!("byte got `{number}`, which is outside the 0-255 range"),
            )
        })
    }

    /// `byte_val(b byte) -> i64`: the numeric value of a byte.
    fn builtin_byte_val(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("byte_val", 1, args.len()))?;
        match value {
            Value::Byte(b) => Ok(Value::I64(b as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("byte_val expects a byte but got `{other}`"),
            )),
        }
    }

    fn builtin_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("len", 1, args.len()))?;
        match value {
            Value::Array(values) => Ok(Value::I64(values.len() as i64)),
            Value::String(text) => Ok(Value::I64(text.chars().count() as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("len expects a string or array but got `{other}`"),
            )),
        }
    }

    /// `list_new() -> list<T>`: a fresh empty list, represented as an array.
    fn builtin_list_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_new", 0, args.len()))?;
        Ok(Value::Array(Vec::new()))
    }

    /// `push(l, x) -> list<T>`: a new list with `x` appended.
    fn builtin_push(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("push", 2, args.len()))?;
        let mut values = expect_list("push", list)?;
        values.push(value);
        Ok(Value::Array(values))
    }

    /// `get(l, i) -> T`: bounds-checked element read.
    fn builtin_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, index]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("get", 2, args.len()))?;
        let values = expect_list("get", list)?;
        let index = expect_i64("get", index)?;
        if index < 0 || index as usize >= values.len() {
            return Err(RuntimeError::new(
                "L0413",
                format!("list index `{index}` is out of bounds"),
            ));
        }
        Ok(values[index as usize].clone())
    }

    /// `set(l, i, x) -> list<T>`: a new list with index `i` replaced by `x`.
    fn builtin_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, index, value]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("set", 3, args.len()))?;
        let mut values = expect_list("set", list)?;
        let index = expect_i64("set", index)?;
        if index < 0 || index as usize >= values.len() {
            return Err(RuntimeError::new(
                "L0413",
                format!("list index `{index}` is out of bounds"),
            ));
        }
        values[index as usize] = value;
        Ok(Value::Array(values))
    }

    /// `pop(l) -> list<T>`: a new list without the last element.
    fn builtin_pop(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("pop", 1, args.len()))?;
        let mut values = expect_list("pop", list)?;
        if values.pop().is_none() {
            return Err(RuntimeError::new("L0413", "cannot pop from an empty list"));
        }
        Ok(Value::Array(values))
    }

    /// `reverse(l) -> list<T>`: a new list with the elements reversed.
    fn builtin_reverse(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("reverse", 1, args.len()))?;
        let mut values = expect_list("reverse", list)?;
        values.reverse();
        Ok(Value::Array(values))
    }

    /// `concat(a, b) -> list<T>`: a new list with `b`'s elements appended to `a`.
    fn builtin_concat(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [a, b]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("concat", 2, args.len()))?;
        let mut values = expect_list("concat", a)?;
        let mut rest = expect_list("concat", b)?;
        values.append(&mut rest);
        Ok(Value::Array(values))
    }

    /// `slice(l, start, end) -> list<T>`: the half-open range `[start, end)`,
    /// with `start`/`end` clamped into `[0, len]` (so it is always total).
    fn builtin_slice(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, start, end]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("slice", 3, args.len()))?;
        let values = expect_list("slice", list)?;
        let start = expect_i64("slice", start)?;
        let end = expect_i64("slice", end)?;
        let len = values.len() as i64;
        let start = start.clamp(0, len) as usize;
        let end = end.clamp(0, len) as usize;
        if start >= end {
            return Ok(Value::Array(Vec::new()));
        }
        Ok(Value::Array(values[start..end].to_vec()))
    }

    /// `map_new() -> map<K, V>`: a fresh empty map.
    fn builtin_map_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_new", 0, args.len()))?;
        Ok(Value::Map(Vec::new()))
    }

    /// `map_set(m, k, v) -> map<K, V>`: a new map with `k` mapped to `v`.
    fn builtin_map_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key, value]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_set", 3, args.len()))?;
        let mut entries = expect_map("map_set", map)?;
        match entries.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = value,
            None => entries.push((key, value)),
        }
        Ok(Value::Map(entries))
    }

    /// `map_get(m, k) -> option<V>`: `some(v)` if present, else `none`.
    fn builtin_map_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_get", 2, args.len()))?;
        let entries = expect_map("map_get", map)?;
        let found = entries.into_iter().find(|(k, _)| *k == key).map(|(_, v)| v);
        Ok(option_value(found))
    }

    /// `map_has(m, k) -> bool`.
    fn builtin_map_has(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_has", 2, args.len()))?;
        let entries = expect_map("map_has", map)?;
        Ok(Value::Bool(entries.iter().any(|(k, _)| *k == key)))
    }

    /// `map_len(m) -> i64`.
    fn builtin_map_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_len", 1, args.len()))?;
        let entries = expect_map("map_len", map)?;
        Ok(Value::I64(entries.len() as i64))
    }

    /// `map_del(m, k) -> map<K, V>`: a new map without key `k`.
    fn builtin_map_del(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_del", 2, args.len()))?;
        let mut entries = expect_map("map_del", map)?;
        entries.retain(|(k, _)| *k != key);
        Ok(Value::Map(entries))
    }

    fn builtin_substring(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, start, end]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("substring", 3, args.len()))?;
        let text = expect_string("substring", text)?;
        let start = expect_i64("substring", start)?;
        let end = expect_i64("substring", end)?;
        let chars: Vec<char> = text.chars().collect();
        let count = chars.len() as i64;
        if start < 0 || end < 0 || start > end || end > count {
            return Err(RuntimeError::new(
                "L0413",
                format!(
                    "substring range [{start}, {end}) is out of bounds for a string of length {count}"
                ),
            ));
        }
        let slice: String = chars[start as usize..end as usize].iter().collect();
        Ok(Value::String(slice))
    }

    fn builtin_find(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, needle]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("find", 2, args.len()))?;
        let text = expect_string("find", text)?;
        let needle = expect_string("find", needle)?;
        Ok(Value::I64(char_find(&text, &needle)))
    }

    fn builtin_contains(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, needle]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("contains", 2, args.len()))?;
        let text = expect_string("contains", text)?;
        let needle = expect_string("contains", needle)?;
        Ok(Value::Bool(text.contains(&needle)))
    }

    fn builtin_starts_with(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, prefix]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("starts_with", 2, args.len()))?;
        let text = expect_string("starts_with", text)?;
        let prefix = expect_string("starts_with", prefix)?;
        Ok(Value::Bool(text.starts_with(&prefix)))
    }

    fn builtin_ends_with(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, suffix]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("ends_with", 2, args.len()))?;
        let text = expect_string("ends_with", text)?;
        let suffix = expect_string("ends_with", suffix)?;
        Ok(Value::Bool(text.ends_with(&suffix)))
    }

    fn builtin_repeat(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, count]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("repeat", 2, args.len()))?;
        let text = expect_string("repeat", text)?;
        let count = expect_i64("repeat", count)?;
        let result = if count <= 0 {
            String::new()
        } else {
            text.repeat(count as usize)
        };
        Ok(Value::String(result))
    }

    fn builtin_split(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, sep]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("split", 2, args.len()))?;
        let text = expect_string("split", text)?;
        let sep = expect_string("split", sep)?;
        if sep.is_empty() {
            return Err(RuntimeError::new(
                "L0417",
                "split requires a non-empty separator".to_string(),
            ));
        }
        let parts = text
            .split(sep.as_str())
            .map(|part| Value::String(part.to_string()))
            .collect();
        Ok(Value::Array(parts))
    }

    fn builtin_join(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [parts, sep]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("join", 2, args.len()))?;
        let Value::Array(parts) = parts else {
            return Err(RuntimeError::new(
                "L0417",
                format!("join expects an array of strings but got `{parts}`"),
            ));
        };
        let sep = expect_string("join", sep)?;
        let mut pieces = Vec::with_capacity(parts.len());
        for part in parts {
            pieces.push(expect_string("join", part)?);
        }
        Ok(Value::String(pieces.join(sep.as_str())))
    }

    fn builtin_trim(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("trim", 1, args.len()))?;
        let text = expect_string("trim", text)?;
        Ok(Value::String(
            text.trim_matches(|c: char| c.is_ascii_whitespace())
                .to_string(),
        ))
    }

    fn builtin_replace(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, from, to]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("replace", 3, args.len()))?;
        let text = expect_string("replace", text)?;
        let from = expect_string("replace", from)?;
        let to = expect_string("replace", to)?;
        if from.is_empty() {
            return Err(RuntimeError::new(
                "L0417",
                "replace requires a non-empty `from` pattern".to_string(),
            ));
        }
        Ok(Value::String(text.replace(from.as_str(), to.as_str())))
    }

    fn builtin_upper(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("upper", 1, args.len()))?;
        let text = expect_string("upper", text)?;
        Ok(Value::String(text.to_uppercase()))
    }

    fn builtin_lower(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("lower", 1, args.len()))?;
        let text = expect_string("lower", text)?;
        Ok(Value::String(text.to_lowercase()))
    }

    /// `to_bytes(s string) -> list<byte>`: the UTF-8 encoding of `s` as a
    /// `list<byte>` (a `Value::Array` of `Value::Byte`, matching `read_bytes`).
    fn builtin_to_bytes(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_bytes", 1, args.len()))?;
        let text = expect_string("to_bytes", text)?;
        Ok(Value::Array(
            text.into_bytes().into_iter().map(Value::Byte).collect(),
        ))
    }

    /// `from_bytes(b list<byte>) -> result<string, string>`: decode `b` as UTF-8,
    /// returning `ok(s)` on success and `err(message)` (never a panic, never a
    /// lossy replacement) on invalid UTF-8.
    fn builtin_from_bytes(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [data]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("from_bytes", 1, args.len()))?;
        let bytes = Self::value_to_bytes("from_bytes", data)?;
        Ok(result_value(match String::from_utf8(bytes) {
            Ok(text) => Ok(Value::String(text)),
            Err(error) => Err(Value::String(format!("invalid utf-8: {error}"))),
        }))
    }

    /// `byte_len(s string) -> i64`: the number of UTF-8 bytes in `s` (distinct
    /// from `len`, which counts characters for a string).
    fn builtin_byte_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("byte_len", 1, args.len()))?;
        let text = expect_string("byte_len", text)?;
        Ok(Value::I64(text.len() as i64))
    }

    fn builtin_abs(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("abs", 1, args.len()))?;
        match value {
            Value::I64(n) => Ok(Value::I64(n.abs())),
            Value::F64(n) => Ok(Value::F64(n.abs())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("abs expects an i64 or f64 but got `{other}`"),
            )),
        }
    }

    fn builtin_min(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [left, right]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("min", 2, args.len()))?;
        match (left, right) {
            (Value::I64(a), Value::I64(b)) => Ok(Value::I64(a.min(b))),
            (Value::F64(a), Value::F64(b)) => Ok(Value::F64(a.min(b))),
            (a, b) => Err(RuntimeError::new(
                "L0417",
                format!("min expects two matching i64 or f64 values but got `{a}` and `{b}`"),
            )),
        }
    }

    fn builtin_max(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [left, right]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("max", 2, args.len()))?;
        match (left, right) {
            (Value::I64(a), Value::I64(b)) => Ok(Value::I64(a.max(b))),
            (Value::F64(a), Value::F64(b)) => Ok(Value::F64(a.max(b))),
            (a, b) => Err(RuntimeError::new(
                "L0417",
                format!("max expects two matching i64 or f64 values but got `{a}` and `{b}`"),
            )),
        }
    }

    fn builtin_pow(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [base, exp]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("pow", 2, args.len()))?;
        match (base, exp) {
            (Value::I64(b), Value::I64(e)) => {
                if e < 0 {
                    return Err(RuntimeError::new(
                        "L0417",
                        format!("pow expects a non-negative integer exponent but got `{e}`"),
                    ));
                }
                Ok(Value::I64(b.pow(e as u32)))
            }
            (Value::F64(b), Value::F64(e)) => Ok(Value::F64(b.powf(e))),
            (b, e) => Err(RuntimeError::new(
                "L0417",
                format!("pow expects two matching i64 or f64 values but got `{b}` and `{e}`"),
            )),
        }
    }

    fn builtin_sqrt(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sqrt", 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(n.sqrt())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("sqrt expects an f64 but got `{other}`"),
            )),
        }
    }

    fn builtin_floor(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("floor", 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(n.floor())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("floor expects an f64 but got `{other}`"),
            )),
        }
    }

    fn builtin_ceil(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("ceil", 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(n.ceil())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("ceil expects an f64 but got `{other}`"),
            )),
        }
    }

    fn builtin_round(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("round", 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(n.round())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("round expects an f64 but got `{other}`"),
            )),
        }
    }

    /// Shared implementation for the unary `f64 -> f64` math builtins
    /// (`sin`/`cos`/`tan`/`atan`/`exp`/`ln`/`log10`). Undefined inputs follow
    /// the platform `f64` semantics (`NaN`/`inf`), identically on every backend.
    fn builtin_unary_f64(
        name: &str,
        args: Vec<Value>,
        op: fn(f64) -> f64,
    ) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(op(n))),
            other => Err(RuntimeError::new(
                "L0417",
                format!("{name} expects an f64 but got `{other}`"),
            )),
        }
    }

    /// `atan2(y, x)`: the angle of the vector `(x, y)` in radians.
    fn builtin_atan2(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [y, x]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atan2", 2, args.len()))?;
        match (y, x) {
            (Value::F64(y), Value::F64(x)) => Ok(Value::F64(y.atan2(x))),
            (y, x) => Err(RuntimeError::new(
                "L0417",
                format!("atan2 expects two f64 values but got `{y}` and `{x}`"),
            )),
        }
    }

    /// `rotate_left(x, n)`: rotate the 64 bits of `x` left by `(n & 63)`
    /// positions. The mask makes it total for any `n` (large or negative).
    fn builtin_rotate_left(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [x, n]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rotate_left", 2, args.len()))?;
        match (x, n) {
            (Value::I64(x), Value::I64(n)) => {
                Ok(Value::I64(x.rotate_left(((n as u64) & 63) as u32)))
            }
            (x, n) => Err(RuntimeError::new(
                "L0417",
                format!("rotate_left expects two i64 values but got `{x}` and `{n}`"),
            )),
        }
    }

    /// `rotate_right(x, n)`: rotate the 64 bits of `x` right by `(n & 63)`
    /// positions. Total for any `n` (the mask handles large/negative values).
    fn builtin_rotate_right(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [x, n]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rotate_right", 2, args.len()))?;
        match (x, n) {
            (Value::I64(x), Value::I64(n)) => {
                Ok(Value::I64(x.rotate_right(((n as u64) & 63) as u32)))
            }
            (x, n) => Err(RuntimeError::new(
                "L0417",
                format!("rotate_right expects two i64 values but got `{x}` and `{n}`"),
            )),
        }
    }

    /// `count_ones(x)`: population count of the 64-bit value `x` (0..=64).
    fn builtin_count_ones(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("count_ones", 1, args.len()))?;
        match value {
            Value::I64(x) => Ok(Value::I64(x.count_ones() as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("count_ones expects an i64 but got `{other}`"),
            )),
        }
    }

    /// `leading_zeros(x)`: number of leading zero bits in `x` (0..=64).
    fn builtin_leading_zeros(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("leading_zeros", 1, args.len()))?;
        match value {
            Value::I64(x) => Ok(Value::I64(x.leading_zeros() as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("leading_zeros expects an i64 but got `{other}`"),
            )),
        }
    }

    /// `trailing_zeros(x)`: number of trailing zero bits in `x` (0..=64).
    fn builtin_trailing_zeros(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("trailing_zeros", 1, args.len()))?;
        match value {
            Value::I64(x) => Ok(Value::I64(x.trailing_zeros() as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("trailing_zeros expects an i64 but got `{other}`"),
            )),
        }
    }

    /// `reverse_bytes(x)`: reverse the byte order of the 64-bit value `x`.
    fn builtin_reverse_bytes(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("reverse_bytes", 1, args.len()))?;
        match value {
            Value::I64(x) => Ok(Value::I64(x.swap_bytes())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("reverse_bytes expects an i64 but got `{other}`"),
            )),
        }
    }

    fn builtin_rc_new(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_new", 1, args.len()))?;
        self.heap.push(Some(value));
        let slot = self.heap.len() - 1;
        self.refcounts.insert(slot, 1);
        Ok(Value::Ptr(slot))
    }

    fn builtin_rc_clone(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_clone", 1, args.len()))?;
        let slot = handle.as_ptr()?;
        match self.refcounts.get_mut(&slot) {
            Some(count) => {
                *count += 1;
                Ok(Value::Ptr(slot))
            }
            None => Err(RuntimeError::new(
                "L0406",
                format!("invalid reference-counted handle `{slot}`"),
            )),
        }
    }

    fn builtin_rc_release(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_release", 1, args.len()))?;
        let slot = handle.as_ptr()?;
        match self.refcounts.get_mut(&slot) {
            Some(count) => {
                *count -= 1;
                if *count == 0 {
                    self.refcounts.remove(&slot);
                    if let Some(target) = self.heap.get_mut(slot) {
                        *target = None;
                    }
                }
                Ok(Value::Void)
            }
            None => Err(RuntimeError::new(
                "L0406",
                format!("invalid reference-counted handle `{slot}`"),
            )),
        }
    }

    fn builtin_rc_borrow(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_borrow", 1, args.len()))?;
        let slot = handle.as_ptr()?;
        if self.refcounts.contains_key(&slot) {
            // A borrow is a non-owning view of the same live slot.
            Ok(Value::Ptr(slot))
        } else {
            Err(RuntimeError::new(
                "L0406",
                format!("invalid reference-counted handle `{slot}`"),
            ))
        }
    }

    /// Shared dereference for `rc_get`, `ref_get`, and (unsafe) `ptr_read`.
    fn builtin_ref_get(&self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        let slot = handle.as_ptr()?;
        self.heap
            .get(slot)
            .and_then(|value| value.clone())
            .ok_or_else(|| RuntimeError::new("L0406", format!("invalid pointer `{slot}`")))
    }

    fn wrong_arity(name: &str, expected: usize, actual: usize) -> RuntimeError {
        RuntimeError::new(
            "L0405",
            format!("function `{name}` expects {expected} arguments but got {actual}"),
        )
    }
}

enum Control {
    Return(Value),
    Break,
    Continue,
    Value(Value),
}

/// Apply a compound assignment operator (`+=` etc.) to `current` and `rhs`,
/// supporting i64 and f64.
pub fn apply_compound(current: Value, op: &AssignOp, rhs: Value) -> Result<Value, RuntimeError> {
    if let (Value::F64(a), Value::F64(b)) = (&current, &rhs) {
        let (a, b) = (*a, *b);
        return Ok(Value::F64(match op {
            AssignOp::Add => a + b,
            AssignOp::Subtract => a - b,
            AssignOp::Multiply => a * b,
            AssignOp::Divide => a / b,
            AssignOp::Replace => b,
        }));
    }
    let a = current.as_i64()?;
    let b = rhs.as_i64()?;
    Ok(Value::I64(match op {
        AssignOp::Add => a + b,
        AssignOp::Subtract => a - b,
        AssignOp::Multiply => a * b,
        AssignOp::Divide => {
            if b == 0 {
                return Err(RuntimeError::new("L0404", "division by zero"));
            }
            a / b
        }
        AssignOp::Replace => b,
    }))
}

/// One hop of a resolved assignment target: a struct field name or an
/// already-evaluated array index. Index expressions are evaluated by each
/// backend's own evaluator before mutation, so these helpers stay shared.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedPlace {
    Field(String),
    Index(i64),
}

fn place_get<'a>(current: &'a Value, place: &ResolvedPlace) -> Result<&'a Value, RuntimeError> {
    match place {
        ResolvedPlace::Field(field) => {
            let Value::Struct { fields, .. } = current else {
                return Err(RuntimeError::new(
                    "L0371",
                    format!("cannot access field `{field}` on non-struct value"),
                ));
            };
            fields
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, value)| value)
                .ok_or_else(|| RuntimeError::new("L0371", format!("no field `{field}`")))
        }
        ResolvedPlace::Index(index) => {
            let Value::Array(values) = current else {
                return Err(RuntimeError::new("L0412", "index target is not an array"));
            };
            if *index < 0 {
                return Err(RuntimeError::new(
                    "L0413",
                    format!("array index `{index}` is out of bounds"),
                ));
            }
            values.get(*index as usize).ok_or_else(|| {
                RuntimeError::new("L0413", format!("array index `{index}` is out of bounds"))
            })
        }
    }
}

fn place_get_mut<'a>(
    current: &'a mut Value,
    place: &ResolvedPlace,
) -> Result<&'a mut Value, RuntimeError> {
    match place {
        ResolvedPlace::Field(field) => {
            let Value::Struct { fields, .. } = current else {
                return Err(RuntimeError::new(
                    "L0371",
                    format!("cannot access field `{field}` on non-struct value"),
                ));
            };
            fields
                .iter_mut()
                .find(|(name, _)| name == field)
                .map(|(_, value)| value)
                .ok_or_else(|| RuntimeError::new("L0371", format!("no field `{field}`")))
        }
        ResolvedPlace::Index(index) => {
            let Value::Array(values) = current else {
                return Err(RuntimeError::new("L0412", "index target is not an array"));
            };
            if *index < 0 {
                return Err(RuntimeError::new(
                    "L0413",
                    format!("array index `{index}` is out of bounds"),
                ));
            }
            let index = *index as usize;
            let len = values.len();
            values.get_mut(index).ok_or_else(|| {
                RuntimeError::new(
                    "L0413",
                    format!("array index `{index}` is out of bounds (len {len})"),
                )
            })
        }
    }
}

/// Read the value at a resolved place path (for compound assignment).
pub fn get_place(value: &Value, path: &[ResolvedPlace]) -> Result<Value, RuntimeError> {
    let mut current = value;
    for place in path {
        current = place_get(current, place)?;
    }
    Ok(current.clone())
}

/// Set the value at a resolved place path in place.
pub fn set_place(root: &mut Value, path: &[ResolvedPlace], new: Value) -> Result<(), RuntimeError> {
    let mut current = root;
    for (index, place) in path.iter().enumerate() {
        let slot = place_get_mut(current, place)?;
        if index + 1 == path.len() {
            *slot = new;
            return Ok(());
        }
        current = slot;
    }
    Ok(())
}

fn statement_span(statement: &Stmt) -> Span {
    match statement {
        Stmt::Let { span, .. }
        | Stmt::Assign { span, .. }
        | Stmt::Break(span)
        | Stmt::Continue(span)
        | Stmt::If { span, .. }
        | Stmt::While { span, .. }
        | Stmt::For { span, .. }
        | Stmt::Loop { span, .. }
        | Stmt::Unsafe { span, .. }
        | Stmt::Asm { span, .. }
        | Stmt::Throw { span, .. }
        | Stmt::Try { span, .. } => *span,
        Stmt::Region(decl) => decl.span,
        Stmt::Return(Some(expr)) | Stmt::Expr(expr) => expr.span,
        Stmt::Return(None) => Span::new(1, 1),
    }
}

#[derive(Debug, Clone)]
struct Env {
    scopes: Vec<HashMap<String, Value>>,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            scopes: vec![HashMap::new()],
        }
    }
}

impl Env {
    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: String, value: Value) {
        self.scopes
            .last_mut()
            .expect("env always has a scope")
            .insert(name, value);
    }

    fn assign(&mut self, name: &str, value: Value) -> Result<(), RuntimeError> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                *slot = value;
                return Ok(());
            }
        }
        Err(RuntimeError::new(
            "L0403",
            format!("unknown variable `{name}`"),
        ))
    }

    fn get(&self, name: &str) -> Result<Value, RuntimeError> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
            .cloned()
            .ok_or_else(|| RuntimeError::new("L0403", format!("unknown variable `{name}`")))
    }
}

impl Value {
    pub fn as_i64(&self) -> Result<i64, RuntimeError> {
        match self {
            Self::I64(value) => Ok(*value),
            _ => Err(RuntimeError::new("L0407", "expected i64 value")),
        }
    }

    pub fn as_f64(&self) -> Result<f64, RuntimeError> {
        match self {
            Self::F64(value) => Ok(*value),
            _ => Err(RuntimeError::new("L0421", "expected f64 value")),
        }
    }

    pub fn as_bool(&self) -> Result<bool, RuntimeError> {
        match self {
            Self::Bool(value) => Ok(*value),
            _ => Err(RuntimeError::new("L0408", "expected bool value")),
        }
    }

    pub fn as_ptr(&self) -> Result<usize, RuntimeError> {
        match self {
            Self::Ptr(value) => Ok(*value),
            _ => Err(RuntimeError::new("L0409", "expected pointer value")),
        }
    }

    pub fn as_string(&self) -> Result<String, RuntimeError> {
        match self {
            Self::String(value) => Ok(value.clone()),
            _ => Err(RuntimeError::new("L0417", "expected string value")),
        }
    }

    pub fn as_string_array(&self) -> Result<Vec<String>, RuntimeError> {
        match self {
            Self::Array(values) => values
                .iter()
                .map(Value::as_string)
                .collect::<Result<Vec<_>, _>>(),
            _ => Err(RuntimeError::new("L0418", "expected array<string> value")),
        }
    }
}

#[cfg(test)]
mod tests {
    use lullaby_lexer::lex;
    use lullaby_parser::parse;
    use lullaby_semantics::validate;

    use super::*;

    fn run_source(source: &str) -> Result<Value, RuntimeError> {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        validate(&program).expect("semantic");
        run_main(&program)
    }

    #[test]
    fn runs_function_calls_and_arithmetic() {
        let source = "fn add x i64 y i64 -> i64\n    x + y\n\nfn main -> i64\n    let value i64 = add(40, 2)\n    value\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn rejects_asm_on_the_ast_interpreter() {
        // Inline assembly is native-only; the AST interpreter rejects it with
        // `L0425` rather than executing raw machine code.
        let source = "fn main -> i64\n    unsafe\n        asm 72, 199, 192, 42, 0, 0, 0\n";
        let error = run_source(source).expect_err("asm must not run on an interpreter");
        assert_eq!(error.code, "L0425");
    }

    #[test]
    fn dispatches_trait_method_by_receiver_type() {
        // `p.show()` dispatches to Point's `Show` impl; the bounded generic
        // `describe(p)` calls the same trait method on the concrete type.
        let source = concat!(
            "trait Show\n",
            "    fn show self -> string\n\n",
            "struct Point\n",
            "    x i64\n",
            "    y i64\n\n",
            "enum Light\n",
            "    Red\n",
            "    Green\n\n",
            "impl Show for Point\n",
            "    fn show self -> string\n",
            "        to_string(self.x)\n\n",
            "impl Show for Light\n",
            "    fn show self -> string\n",
            "        match self\n",
            "            Red -> \"r\"\n",
            "            Green -> \"green\"\n\n",
            "fn describe<T: Show> v T -> string\n",
            "    v.show()\n\n",
            "fn main -> i64\n",
            "    let p Point = Point(3, 4)\n",
            "    let g Light = Green\n",
            // len("3")=1 + len("green")=5 + len(describe(p))=1 + len(describe(g))=5
            "    len(p.show()) + len(g.show()) + len(describe(p)) + len(describe(g))\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(12));
    }

    #[test]
    fn runs_higher_order_function_call() {
        let source = concat!(
            "fn inc x i64 -> i64\n",
            "    x + 1\n\n",
            "fn dbl x i64 -> i64\n",
            "    x * 2\n\n",
            "fn apply f fn(i64) -> i64 v i64 -> i64\n",
            "    f(v)\n\n",
            "fn picker -> fn(i64) -> i64\n",
            "    dbl\n\n",
            "fn main -> i64\n",
            "    let g fn(i64) -> i64 = inc\n",
            "    let h fn(i64) -> i64 = picker()\n",
            "    apply(inc, 10) + g(5) + h(20) + apply(dbl, 3)\n",
        );
        // apply(inc,10)=11 + g(5)=6 + h=dbl,h(20)=40 + apply(dbl,3)=6 = 63.
        assert_eq!(run_source(source).expect("run"), Value::I64(63));
    }

    #[test]
    fn runs_char_and_byte_builtins() {
        let source = concat!(
            "fn main -> i64\n",
            "    let a char = 'A'\n",
            "    let b char = char_from(char_code(a) + 1)\n",
            "    let ordered i64 = 0\n",
            "    if a < b\n",
            "        ordered = 1\n",
            "    let big byte = byte(250)\n",
            "    let text string = to_string(a) + to_string(byte(10))\n",
            "    char_code(b) + byte_val(big) + ordered + len(text)\n",
        );
        // code('B')=66 + byte_val(250)=250 + ordered=1 + len("A10")=3 = 320.
        assert_eq!(run_source(source).expect("run"), Value::I64(320));
    }

    #[test]
    fn char_from_rejects_invalid_scalar() {
        let source = "fn main -> char\n    char_from(0 - 1)\n";
        let error = run_source(source).expect_err("invalid scalar");
        assert_eq!(error.code, "L0417");
    }

    #[test]
    fn byte_rejects_out_of_range() {
        let source = "fn main -> byte\n    byte(300)\n";
        let error = run_source(source).expect_err("out of range");
        assert_eq!(error.code, "L0417");
    }

    #[test]
    fn runs_memory_builtins() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn runs_store_builtin() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn runs_list_builtins() {
        let source = concat!(
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, 10)\n",
            "    l = push(l, 20)\n",
            "    l = push(l, 30)\n",
            "    l = set(l, 1, 25)\n",
            "    let a i64 = get(l, 0)\n",
            "    let b i64 = get(l, 2)\n",
            "    let n i64 = len(l)\n",
            "    l = pop(l)\n",
            "    a + b + n + len(l) + get(l, 1)\n",
        );
        // [10,20,30] -> set(1,25) -> [10,25,30]; a=10, b=30, n=3;
        // pop -> [10,25]; len=2, get(1)=25; 10+30+3+2+25 = 70.
        assert_eq!(run_source(source).expect("run"), Value::I64(70));
    }

    #[test]
    fn list_get_out_of_bounds_errors() {
        let source = concat!(
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, 1)\n",
            "    get(l, 5)\n",
        );
        let error = run_source(source).expect_err("run");
        assert_eq!(error.code, "L0413");
    }

    #[test]
    fn map_set_get_round_trips_via_option() {
        let source = concat!(
            "fn main -> i64\n",
            "    let m map<string, i64> = map_new()\n",
            "    m = map_set(m, \"x\", 41)\n",
            "    m = map_set(m, \"x\", 42)\n",
            "    match map_get(m, \"x\")\n",
            "        some(v) -> v\n",
            "        none -> 0\n",
        );
        // Insert then replace `x`; `map_get` returns `some(42)`, unwrapped to 42.
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn map_get_missing_key_returns_none() {
        let source = concat!(
            "fn main -> i64\n",
            "    let m map<string, i64> = map_new()\n",
            "    m = map_set(m, \"x\", 1)\n",
            "    m = map_del(m, \"x\")\n",
            "    match map_get(m, \"x\")\n",
            "        some(v) -> v\n",
            "        none -> 7\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(7));
    }

    #[test]
    fn runs_string_builtins() {
        let source = concat!(
            "fn main -> i64\n",
            "    let s string = \"Hello, World\"\n",
            "    let parts array<string> = split(\"a,b,c\", \",\")\n",
            "    find(s, \"World\") + len(parts)\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(10));
    }

    #[test]
    fn runs_string_transforms() {
        let source = concat!(
            "fn main -> string\n",
            "    let joined string = join(split(\"a,b,c\", \",\"), \"-\")\n",
            "    upper(replace(substring(joined, 0, 3), \"-\", \"_\"))\n",
        );
        assert_eq!(
            run_source(source).expect("run"),
            Value::String("A_B".to_string())
        );
    }

    #[test]
    fn substring_out_of_range_is_runtime_error() {
        let source = "fn main -> string\n    substring(\"hi\", 0, 5)\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0413");
    }

    #[test]
    fn split_empty_separator_is_runtime_error() {
        let source = "fn main -> i64\n    len(split(\"hi\", \"\"))\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0417");
    }

    #[test]
    fn runs_integer_math_builtins() {
        let source = "fn main -> i64\n    let a i64 = abs(0 - 5)\n    let b i64 = min(3, 7)\n    let c i64 = max(3, 7)\n    let d i64 = pow(2, 10)\n    a + b + c + d\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(1039));
    }

    #[test]
    fn runs_float_math_builtins() {
        let source = "fn check f f64 want f64 -> i64\n    if f == want\n        1\n    else\n        0\n\nfn main -> i64\n    check(sqrt(16.0), 4.0) + check(floor(2.7), 2.0) + check(ceil(2.1), 3.0) + check(round(2.5), 3.0)\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(4));
    }

    #[test]
    fn rejects_negative_integer_pow_at_runtime() {
        let source = "fn main -> i64\n    pow(2, 0 - 1)\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0417");
    }

    #[test]
    fn runs_if_expression_result() {
        let source = "fn main -> i64\n    if true\n        42\n    else\n        0\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn runs_while_loop_with_assignment() {
        let source = "fn main -> i64\n    let x i64 = 0\n    while x < 3\n        x += 1\n    x\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(3));
    }

    #[test]
    fn runs_loop_break_and_continue() {
        let source = "fn main -> i64\n    let x i64 = 0\n    loop\n        x += 1\n        if x < 3\n            continue\n        break\n    x\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(3));
    }

    #[test]
    fn runs_logical_expressions() {
        let source = "fn main -> bool\n    not false and true or false\n";
        assert_eq!(run_source(source).expect("run"), Value::Bool(true));
    }

    #[test]
    fn short_circuits_logical_expressions() {
        let source = "fn main -> bool\n    false and (1 / 0 == 0) or true\n";
        assert_eq!(run_source(source).expect("run"), Value::Bool(true));
    }

    #[test]
    fn runs_for_loop() {
        let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    total\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(6));
    }

    #[test]
    fn mutates_array_elements_and_reports_len() {
        let source = "fn main -> i64\n    let xs array<i64> = [1, 2, 3]\n    xs[0] = 10\n    xs[len(xs) - 1] += 4\n    xs[0] + xs[2]\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(17));
    }

    #[test]
    fn array_element_assignment_bounds_checked() {
        let source = "fn main -> i64\n    let xs array<i64> = [1]\n    xs[3] = 9\n    xs[0]\n";
        let error = run_source(source).expect_err("out of bounds");
        assert_eq!(error.code, "L0413");
    }

    #[test]
    fn runs_for_loop_with_step() {
        let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 5 by 2\n        total += i\n    total\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(9));
    }

    #[test]
    fn runs_descending_for_loop() {
        let source = "fn main -> i64\n    let total i64 = 0\n    for i from 3 to 1 by -1\n        total += i\n    total\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(6));
    }

    #[test]
    fn runs_array_literal_and_index() {
        let source = "fn main -> i64\n    let values array<i64> = [2, 4, 6]\n    values[2]\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(6));
    }

    #[test]
    fn rejects_array_index_out_of_bounds() {
        let source = "fn main -> i64\n    let values array<i64> = [1, 2]\n    values[3]\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0413");
    }

    #[test]
    fn rejects_zero_for_step() {
        let source = "fn main -> i64\n    for i from 1 to 3 by 0\n        i\n    0\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0411");
    }

    #[test]
    fn keeps_let_bindings_block_scoped() {
        let source = "fn main -> i64\n    let x i64 = 1\n    if true\n        let x i64 = 2\n        x\n    x\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(1));
    }

    #[test]
    fn rejects_double_dealloc() {
        // The free is inside a branch, so the conservative compile-time
        // lifetime analysis does not track it out; the runtime L0406 guard
        // still catches the double free.
        let source = "fn main -> void\n    let ptr ptr_i64 = alloc(1)\n    if true\n        dealloc(ptr)\n    dealloc(ptr)\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0406");
    }

    #[test]
    fn rejects_store_after_dealloc() {
        let source = "fn main -> void\n    let ptr ptr_i64 = alloc(1)\n    if true\n        dealloc(ptr)\n    store(ptr, 2)\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0406");
    }

    #[test]
    fn runs_file_io_builtins() {
        let path = std::env::temp_dir()
            .join(format!("lullaby-runtime-{}.txt", std::process::id()))
            .to_string_lossy()
            .replace('\\', "/");
        let source = format!(
            "fn main -> string\n    write_file(\"{path}\", \"alpha\")\n    append_file(\"{path}\", \" beta\")\n    read_file(\"{path}\")\n"
        );
        assert_eq!(
            run_source(&source).expect("run"),
            Value::String("alpha beta".to_string())
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reports_missing_file_as_resource_error() {
        let path = std::env::temp_dir()
            .join(format!("lullaby-missing-{}.txt", std::process::id()))
            .to_string_lossy()
            .replace('\\', "/");
        let source = format!("fn main -> string\n    read_file(\"{path}\")\n");
        let error = run_source(&source).expect_err("runtime error");
        assert_eq!(error.code, "L0414");
        assert_eq!(error.category, ErrorCategory::Resource);
    }

    #[test]
    fn runs_safe_system_status_builtin() {
        let source = "fn main -> i64\n    sys_status(\"rustc\", [\"--version\"])\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(0));
    }

    #[test]
    fn runs_reference_counted_values() {
        let source = "fn main -> i64\n    let handle rc<i64> = rc_new(41)\n    let shared rc<i64> = rc_clone(handle)\n    let view ref<i64> = rc_borrow(handle)\n    let a i64 = rc_get(handle)\n    let b i64 = ref_get(view)\n    rc_release(shared)\n    rc_release(handle)\n    a + b - 40\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn rejects_use_after_rc_release() {
        // Release inside a branch escapes the conservative compile-time
        // analysis; the runtime guard still reports the dangling handle.
        let source = "fn main -> i64\n    let handle rc<i64> = rc_new(1)\n    if true\n        rc_release(handle)\n    rc_get(handle)\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0406");
    }

    #[test]
    fn parallel_map_returns_mapped_list_in_order() {
        // Each element is squared on its own OS thread; results come back in the
        // same order as the input, so the mapped list is deterministic.
        let source = "fn sq x i64 -> i64\n    x * x\n\nfn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 1)\n    base = push(base, 2)\n    base = push(base, 3)\n    base = push(base, 4)\n    parallel_map(sq, base)\n";
        assert_eq!(
            run_source(source).expect("run"),
            Value::Array(vec![
                Value::I64(1),
                Value::I64(4),
                Value::I64(9),
                Value::I64(16),
            ])
        );
    }

    #[test]
    fn spawn_channel_round_trip_sums_deterministically() {
        // Four detached workers each `send(ch, v * v)`; `main` joins them and
        // sums the four received values. The total is order-independent, so it is
        // a deterministic 30 (1 + 4 + 9 + 16) regardless of thread scheduling.
        let source = "fn worker ch Chan v i64 -> void\n    send(ch, v * v)\n\nfn main -> i64\n    let ch Chan = chan_new()\n    let t1 Task = spawn(worker, ch, 1)\n    let t2 Task = spawn(worker, ch, 2)\n    let t3 Task = spawn(worker, ch, 3)\n    let t4 Task = spawn(worker, ch, 4)\n    task_join(t1)\n    task_join(t2)\n    task_join(t3)\n    task_join(t4)\n    let total i64 = 0\n    for i from 0 to 3\n        total += recv(ch)\n    total\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(30));
    }

    #[test]
    fn mutex_accumulates_and_reads_back() {
        // Exercise the mutex builtins: create, set, atomically add, and read back.
        let source = "fn main -> i64\n    let m Mutex = mutex_new(10)\n    mutex_set(m, 5)\n    let a i64 = mutex_add(m, 3)\n    mutex_add(m, 4)\n    a + mutex_get(m)\n";
        // set -> 5, add 3 -> 8 (returned as `a`), add 4 -> 12 (read back).
        assert_eq!(run_source(source).expect("run"), Value::I64(20));
    }

    #[test]
    fn mutex_shared_across_threads_via_clone() {
        // The `Value::Mutex` handle shares its cell on clone, so accumulating from
        // several OS threads over the same `Arc<Mutex<i64>>` is safe and yields a
        // deterministic total. This proves cross-thread mutex sharing directly
        // (the language `spawn`'s fixed `(Chan, i64)` shape cannot pass a mutex to
        // a worker yet, so this is verified at the runtime level).
        let mutex = SharedMutex {
            cell: Arc::new(Mutex::new(0)),
        };
        let value = Value::Mutex(mutex);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let handle = value.clone();
                scope.spawn(move || {
                    for _ in 0..100 {
                        Runtime::builtin_mutex_add(vec![handle.clone(), Value::I64(1)])
                            .expect("mutex_add");
                    }
                });
            }
        });
        assert_eq!(
            Runtime::builtin_mutex_get(vec![value]).expect("mutex_get"),
            Value::I64(800)
        );
    }

    #[test]
    fn runs_unsafe_raw_pointer_read() {
        let source = "fn main -> i64\n    let p ptr_i64 = alloc(42)\n    let v i64 = 0\n    unsafe\n        v = ptr_read(p)\n    dealloc(p)\n    v\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn try_catch_yields_a_value_from_either_arm() {
        let caught = "fn main -> string\n    try\n        throw \"boom\"\n    catch message\n        \"caught: \" + message\n";
        assert_eq!(
            run_source(caught).expect("run"),
            Value::String("caught: boom".to_string())
        );
        let ok = "fn main -> i64\n    try\n        42\n    catch message\n        0\n";
        assert_eq!(run_source(ok).expect("run"), Value::I64(42));
    }

    #[test]
    fn catches_thrown_error_and_recovers() {
        let source = "fn main -> i64\n    let result i64 = 0\n    try\n        throw \"boom\"\n    catch message\n        result = 7\n    result\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(7));
    }

    #[test]
    fn propagates_uncaught_throw() {
        let source = "fn main -> i64\n    throw \"unhandled\"\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0420");
        assert_eq!(error.message, "unhandled");
    }

    #[test]
    fn assert_true_returns_void() {
        let source = "fn main -> void\n    assert(true)\n";
        assert_eq!(run_source(source).expect("run"), Value::Void);
    }

    #[test]
    fn assert_false_yields_catchable_runtime_error() {
        // `assert(false)` raises the same catchable user-error a `throw` does.
        let source = "fn main -> void\n    assert(false)\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0420");
        assert_eq!(error.message, "assertion failed");
    }

    #[test]
    fn assert_false_is_recoverable_by_try_catch() {
        let source = "fn main -> i64\n    let result i64 = 0\n    try\n        assert(false)\n    catch message\n        result = 7\n    result\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(7));
    }

    #[test]
    fn run_named_function_runs_a_test_without_main() {
        // A library-style program with no `main`: run a named zero-arg function
        // directly. A passing test returns Ok; a failing one propagates L0420.
        let source =
            "fn test_ok -> void\n    assert(true)\n\nfn test_bad -> void\n    assert(false)\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        validate(&program).expect("semantic");
        assert_eq!(
            run_named_function(&program, "test_ok").expect("run test_ok"),
            Value::Void
        );
        let error = run_named_function(&program, "test_bad").expect_err("test_bad fails");
        assert_eq!(error.code, "L0420");
        assert_eq!(error.message, "assertion failed");
    }

    #[test]
    fn catch_binds_thrown_message_across_call_boundary() {
        let source = "fn risky -> i64\n    throw \"from risky\"\n\nfn main -> string\n    let captured string = \"\"\n    try\n        let value i64 = risky()\n    catch message\n        captured = message\n    captured\n";
        assert_eq!(
            run_source(source).expect("run"),
            Value::String("from risky".to_string())
        );
    }

    #[test]
    fn mutates_struct_fields() {
        let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(1, 2)\n    p.x = 10\n    p.y += 5\n    p.x + p.y\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(17));
    }

    #[test]
    fn constructs_and_reads_struct_fields() {
        let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(3, 4)\n    p.x * p.x + p.y * p.y\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(25));
    }

    #[test]
    fn passes_structs_through_functions() {
        let source = "struct Player\n    name string\n    score i64\n\nfn label hero Player -> string\n    hero.name + \":\" + to_string(hero.score)\n\nfn main -> string\n    label(Player(\"Ada\", 100))\n";
        assert_eq!(
            run_source(source).expect("run"),
            Value::String("Ada:100".to_string())
        );
    }

    #[test]
    fn evaluates_f64_arithmetic() {
        let source = "fn main -> f64\n    let x f64 = 3.5\n    x + 1.5\n";
        assert_eq!(run_source(source).expect("run"), Value::F64(5.0));
    }

    #[test]
    fn compares_and_stringifies_f64() {
        let source = "fn main -> string\n    let x f64 = 2.5\n    to_string(x < 3.0) + \" \" + to_string(x * 2.0)\n";
        assert_eq!(
            run_source(source).expect("run"),
            Value::String("true 5".to_string())
        );
    }

    #[test]
    fn concatenates_strings_and_converts_values() {
        let source = "fn main -> string\n    let n i64 = 40 + 2\n    \"answer: \" + to_string(n) + \" ok=\" + to_string(n == 42)\n";
        assert_eq!(
            run_source(source).expect("run"),
            Value::String("answer: 42 ok=true".to_string())
        );
    }

    #[test]
    fn runs_standard_stream_builtins() {
        let source = "fn main -> void\n    println(\"hello\")\n    print(\"a\")\n    warn(\"w\")\n    flush()\n";
        assert_eq!(run_source(source).expect("run"), Value::Void);
    }

    #[test]
    fn runs_safe_system_output_builtin() {
        let source = "fn main -> bool\n    let output string = sys_output(\"rustc\", [\"--version\"])\n    output == \"\" == false\n";
        assert_eq!(run_source(source).expect("run"), Value::Bool(true));
    }

    #[test]
    fn constructs_and_passes_enum_values() {
        // Constructs unit and payload variants, stores them in locals and arrays,
        // passes them through functions, and returns an i64 computed from plain
        // locals (there is no `match` yet).
        let source = "enum Color\n    Red\n    Green\n    Blue\n\nenum Shape\n    Circle f64\n    Rect f64 f64\n    Empty\n\nfn tag c Color -> i64\n    7\n\nfn main -> i64\n    let c Color = Green\n    let palette array<Color> = [Red, Green, Blue]\n    let circle Shape = Circle(2.0)\n    let hole Shape = Empty\n    let shapes array<Shape> = [circle, hole]\n    tag(c) + len(palette) + len(shapes)\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(12));
    }

    #[test]
    fn matches_enum_and_extracts_payload() {
        let source = concat!(
            "enum Shape\n    Circle i64\n    Rect i64 i64\n    Empty\n\n",
            "fn area s Shape -> i64\n",
            "    match s\n",
            "        Circle(r) -> r * r\n",
            "        Rect(w, h) -> w * h\n",
            "        Empty -> 0\n\n",
            "fn main -> i64\n",
            "    area(Circle(3)) + area(Rect(4, 5)) + area(Empty)\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(29));
    }

    #[test]
    fn match_wildcard_arm_covers_remaining_variants() {
        let source = concat!(
            "enum Color\n    Red\n    Green\n    Blue\n\n",
            "fn rank c Color -> i64\n",
            "    match c\n",
            "        Green -> 10\n",
            "        _ -> 1\n\n",
            "fn main -> i64\n    rank(Green) + rank(Red) + rank(Blue)\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(12));
    }

    #[test]
    fn runs_option_and_result_via_match() {
        let source = concat!(
            "fn unwrap_or o option<i64> fallback i64 -> i64\n",
            "    match o\n",
            "        some(v) -> v\n",
            "        none -> fallback\n\n",
            "fn describe r result<i64, string> -> string\n",
            "    match r\n",
            "        ok(v) -> \"ok \" + to_string(v)\n",
            "        err(m) -> \"err \" + m\n\n",
            "fn main -> string\n",
            "    let a option<i64> = some(3)\n",
            "    let b option<i64> = none\n",
            "    let sum i64 = unwrap_or(a, 0) + unwrap_or(b, 100)\n",
            "    let good result<i64, string> = ok(sum)\n",
            "    let bad result<i64, string> = err(\"boom\")\n",
            "    describe(good) + \" / \" + describe(bad)\n",
        );
        assert_eq!(
            run_source(source).expect("run"),
            Value::String("ok 103 / err boom".to_string())
        );
    }

    #[test]
    fn enum_value_display_formats_unit_and_payload_variants() {
        let unit = Value::Enum {
            enum_name: "Shape".to_string(),
            variant: "Empty".to_string(),
            payload: Vec::new(),
        };
        assert_eq!(unit.to_string(), "Empty");

        let payload = Value::Enum {
            enum_name: "Shape".to_string(),
            variant: "Circle".to_string(),
            payload: vec![Value::F64(2.0)],
        };
        assert_eq!(payload.to_string(), "Circle(2)");
    }

    #[test]
    fn runs_generic_identity_at_two_types() {
        // A single erased generic function called at `i64` and `string`; the
        // string result is measured with `len` so `main` stays `i64`.
        let source = concat!(
            "fn identity<T> x T -> T\n",
            "    x\n\n",
            "fn main -> i64\n",
            "    let n i64 = identity(41)\n",
            "    let s string = identity(\"abc\")\n",
            "    n + len(s)\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(44));
    }

    #[test]
    fn runs_generic_choose_selecting_by_flag() {
        let source = concat!(
            "fn choose<T> pick bool a T b T -> T\n",
            "    if pick\n",
            "        return a\n",
            "    b\n\n",
            "fn main -> i64\n",
            "    choose(true, 10, 20) + choose(false, 3, 7)\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(17));
    }

    #[test]
    fn tcp_connect_refused_yields_err_result() {
        // Connecting to port 1 on loopback is a deterministic refusal, so the
        // `result` takes the `err` arm and the program returns 1. No server.
        let source = concat!(
            "fn main -> i64\n",
            "    let outcome result<Socket, string> = tcp_connect(\"127.0.0.1\", 1)\n",
            "    match outcome\n",
            "        ok(conn) -> 0\n",
            "        err(message) -> 1\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(1));
    }

    #[test]
    fn http_get_refused_yields_err_result() {
        // Connecting to port 1 on loopback is a deterministic refusal, so the
        // `result` takes the `err` arm and the program returns 1. No server.
        let source = concat!(
            "fn main -> i64\n",
            "    let outcome result<string, string> = http_get(\"http://127.0.0.1:1/\")\n",
            "    match outcome\n",
            "        ok(body) -> 0\n",
            "        err(message) -> 1\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(1));
    }

    #[test]
    fn http_get_https_url_yields_err_result() {
        // `https://` is out of scope; it returns an `err` deterministically.
        let source = concat!(
            "fn main -> i64\n",
            "    let outcome result<string, string> = http_get(\"https://example.com/\")\n",
            "    match outcome\n",
            "        ok(body) -> 0\n",
            "        err(message) -> 1\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(1));
    }

    #[test]
    fn to_bytes_from_bytes_round_trip_and_byte_len() {
        // `to_bytes("Hi")` = [72, 105]; `from_bytes` decodes back to "Hi";
        // `byte_len("café")` = 5 while `len` counts 4 characters.
        let source = concat!(
            "fn main -> i64\n",
            "    let bytes list<byte> = to_bytes(\"Hi\")\n",
            "    let first i64 = byte_val(get(bytes, 0))\n",
            "    let second i64 = byte_val(get(bytes, 1))\n",
            "    let decoded i64 = 0\n",
            "    match from_bytes(bytes)\n",
            // 72 + 105 + len("Hi")=2 + (byte_len=5 - len=4)=1 => 180
            "        ok(s) -> first + second + len(s) + (byte_len(\"café\") - len(\"café\"))\n",
            "        err(m) -> 0 - len(m)\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(180));
    }

    #[test]
    fn from_bytes_rejects_invalid_utf8_with_err() {
        // A lone `0xFF` byte is not valid UTF-8: `from_bytes` returns `err`
        // (never a panic, never a lossy replacement).
        let source = concat!(
            "fn main -> i64\n",
            "    let bad list<byte> = push(list_new(), byte(255))\n",
            "    match from_bytes(bad)\n",
            "        ok(s) -> len(s)\n",
            "        err(m) -> 1\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(1));
    }

    #[test]
    fn os_random_bytes_len_and_bounds_behavior() {
        // A positive length yields exactly that many bytes.
        assert_eq!(os_random_bytes(16).expect("ok").len(), 16);
        // Zero yields an empty buffer (no syscall, no error).
        assert_eq!(os_random_bytes(0).expect("ok"), Vec::<u8>::new());
        // A negative length is an error, not a panic.
        assert_eq!(
            os_random_bytes(-1),
            Err("os_random length must be non-negative".to_string())
        );
    }

    #[test]
    fn os_random_returns_requested_length_and_empty_and_err() {
        // `os_random(16)` yields `ok` with 16 bytes; `os_random(0)` yields `ok`
        // with an empty list; `os_random(-1)` yields `err` (never a panic). The
        // fixed total is 16 + 0 + (0 - 1) = 15.
        let source = concat!(
            "fn amount n i64 -> i64\n",
            "    match os_random(n)\n",
            "        ok(bytes) -> len(bytes)\n",
            "        err(_) -> 0 - 1\n\n",
            "fn main -> i64\n",
            "    amount(16) + amount(0) + amount(0 - 1)\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(15));
    }

    #[test]
    fn os_random_is_non_deterministic_across_calls() {
        // A real OS CSPRNG (not a seeded PRNG) produces different 32-byte draws
        // with overwhelming probability, so two draws must differ.
        let first = os_random_bytes(32).expect("ok");
        let second = os_random_bytes(32).expect("ok");
        assert_eq!(first.len(), 32);
        assert_eq!(second.len(), 32);
        assert_ne!(first, second, "two OS-CSPRNG draws must not be identical");
    }
}
