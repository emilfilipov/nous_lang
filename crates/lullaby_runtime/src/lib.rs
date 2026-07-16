//! The Lullaby AST-interpreter runtime crate: the `Value` model shared by every
//! backend plus the interpreter and helper machinery around it.
//!
//! The value model (`Value`, `StructValue`, `EnumValue`, `Closure`, the layout
//! and scalar accessors), the `apply_compound`/place helpers, and the small
//! scalar constructors live here in the crate root. The surrounding machinery is
//! split into cohesive sibling modules and re-exported so external paths
//! (`lullaby_runtime::X`) are unchanged:
//!
//! - `runtime_int`: the fixed-width integer lattice and integer/overflow ops.
//! - `runtime_map`: the ordered-map value.
//! - `runtime_error`: `RuntimeError`, `ErrorCategory`, and scalar extractors.
//! - `runtime_concurrency`: channel/task/future/mutex/atomic values + builtins.
//! - `runtime_os`: clocks, sleep, OS randomness, and stdin readers.
//! - `runtime_net`: sockets, processes, and the HTTP exchange.
//! - `runtime_list`: homogeneous-list aggregate/sort operations.
//! - `interpreter`: the `Runtime` evaluator, the env shelf, and the entry points;
//!   its builtin and eval halves stay in the crate root's `builtins`/`eval`
//!   submodules.
//! - `runtime_env`: the lexical `Env` (scope stack, monotonic scope ids, the
//!   process-unique env id the raw-pointer model resolves against), mirroring the
//!   IR/bytecode tiers' `ir_env.rs`.
//! - `raw_pointer`: the place-backed raw-pointer address space behind `addr_of` /
//!   `ptr_offset` / `ptr_cast`.

use std::fmt;

mod interpreter;
mod raw_pointer;
mod runtime_concurrency;
mod runtime_env;
mod runtime_error;
mod runtime_int;
mod runtime_list;
mod runtime_map;
mod runtime_net;
mod runtime_os;

pub use interpreter::{run_main, run_main_with_args, run_named_function};
pub use raw_pointer::{
    RAW_POINTER_BASE, RawPointerMemory, RawResolve, RootSlot, arena_buffer_key, arena_cursor_key, dangling_place, next_env_id,
    unmapped_raw, unreachable_frame,
};
pub use runtime_concurrency::*;
pub use runtime_error::*;
pub use runtime_int::*;
pub use runtime_list::*;
pub use runtime_map::*;
pub use runtime_net::*;
pub use runtime_os::*;

// Crate-internal re-exports so the `builtins`/`eval` submodules (which reach the
// crate root via `use super::*`) and the AST-interpreter test module see the
// interpreter types, the private process-pipe selector, the parser AST types,
// and the standard-library/diagnostics types they name. `AssignOp` is also used
// by `apply_compound` below.
// The actor data model lives in the `actor` module, next to the scheduler that
// owns it; `Runtime` only holds the tables.
pub(crate) use actor::{ActorInstance, ActorMessage, ReplySlot, SupervisionAction};
pub(crate) use interpreter::{
    CallFrame, Control, ParallelCallable, Runtime, index_into, statement_span,
};
pub(crate) use lullaby_diagnostics::{Span, TraceFrame};
pub(crate) use lullaby_parser::{
    AssignOp, BinaryOp, Expr, ExprKind, Function, MatchArm, MatchPattern, Place, Stmt, UnaryOp,
};
pub(crate) use runtime_env::Env;
pub(crate) use runtime_net::PipeKind;
pub(crate) use std::fs;
pub(crate) use std::net::{TcpListener, TcpStream, UdpSocket};
pub(crate) use std::process::{Command, Stdio};
pub(crate) use std::sync::atomic::{AtomicI64, Ordering};
pub(crate) use std::sync::{Arc, Mutex};

/// The runtime type name of a value, used for trait-method dispatch. Structs and
/// enums carry their declared type name; scalars map to their primitive name.
pub fn value_type_name(value: &Value) -> String {
    match value {
        Value::I64(_) => "i64".to_string(),
        Value::Int { ty, .. } => ty.type_name().to_string(),
        Value::F64(_) => "f64".to_string(),
        Value::F32(_) => "f32".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::String(_) => "string".to_string(),
        Value::Char(_) => "char".to_string(),
        Value::Byte(_) => "byte".to_string(),
        Value::Array(_) => "array".to_string(),
        Value::Struct(s) => s.name.clone(),
        Value::Enum(e) => e.enum_name.clone(),
        Value::Map(_) => "map".to_string(),
        Value::Func(_) => "fn".to_string(),
        // The runtime closure value carries no parameter/return types (they live
        // in the type checker), so the runtime type name is the bare `fn` family.
        Value::Closure(_) => "fn".to_string(),
        Value::Ptr(_) => "ptr".to_string(),
        Value::Socket(_) => "Socket".to_string(),
        Value::Process(_) => "process".to_string(),
        Value::Chan(_) => "Chan".to_string(),
        Value::Task(_) => "Task".to_string(),
        Value::Future(_) => "Future".to_string(),
        Value::ActorFuture(_) => "Future".to_string(),
        Value::Mutex(_) => "Mutex".to_string(),
        Value::Atomic(_) => "atomic_i64".to_string(),
        Value::ActorRef(_) => "Actor".to_string(),
        Value::Void => "void".to_string(),
    }
}

/// An environment-capturing closure value. It is deliberately **backend-neutral**:
/// it stores no body node, only a parse-order `id` that keys each backend's own
/// `id -> (param_names, body)` closure-body table, plus a by-value snapshot of the
/// enclosing locals captured when the closure literal was evaluated. Cloning the
/// closure clones `captured`, so value-semantic collections snapshot and
/// reference-semantic handles (`Mutex`/`Chan`/`rc`) share by their own clone.
#[derive(Debug, Clone, PartialEq)]
pub struct Closure {
    pub id: usize,
    pub captured: Vec<(String, Value)>,
}

// `Eq` is intentionally omitted: `Value::F64` holds an `f64`, which is not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    I64(i64),
    /// A fixed-width integer (`i32`/`u32`) carrying its width/signedness tag. The
    /// `value` cell is always normalized to `ty`'s range via [`Value::int`], so
    /// the derived `PartialEq` and plain `i64` ordering are exact. `i64` itself
    /// stays [`Value::I64`] (its own full-width cell), never an `Int`.
    Int {
        value: i64,
        ty: IntKind,
    },
    F64(f64),
    /// A 32-bit IEEE-754 float. Stored as a native `f32`, so every operation is
    /// inherently rounded to `f32` precision (the required normalization); a
    /// distinct `f32` never mixes with an `f64` without an explicit conversion.
    F32(f32),
    Bool(bool),
    /// A string value. Held as a `Box<str>` (pointer + length, 16 bytes) rather
    /// than a `String` (pointer + length + capacity, 24 bytes): interpreter
    /// strings are immutable values (concatenation and slicing build fresh
    /// strings), so the growth capacity is never used, and the narrower cell keeps
    /// the whole `Value` enum small — every scalar clone/move on the hot path
    /// copies fewer bytes. Access is the same single indirection as `&str`.
    String(Box<str>),
    /// A Unicode scalar value.
    Char(char),
    /// An 8-bit unsigned integer (0-255).
    Byte(u8),
    /// A fixed array. Held as a `Box<[Value]>` (pointer + length, 16 bytes) rather
    /// than a `Vec<Value>` (24 bytes): a `Value::Array` is value-semantic and
    /// never grown in place (element assignment mutates in place; map/filter/slice
    /// build fresh arrays), so the growth capacity is unused and the narrower cell
    /// keeps `Value` small. Element access/mutation is unchanged.
    Array(Box<[Value]>),
    Ptr(usize),
    /// A struct value. Boxed so the common scalar `Value` variants stay small
    /// (the interpreter moves/clones `Value`s constantly, so enum size is on the
    /// hot path); the box is one indirection paid only by struct values.
    Struct(Box<StructValue>),
    /// An enum value (including the built-in `option`/`result`). Boxed for the
    /// same size reason as [`Value::Struct`].
    Enum(Box<EnumValue>),
    /// A `map<K, V>`: an insertion-ordered association list ([`OrderedMap`])
    /// backed by a hash index, so `map_get`/`map_has`/`map_set`-of-an-existing-
    /// key are O(1) while iteration order and `==` remain those of the entries.
    /// Boxed so the map's Vec+hash-index (the largest payload) does not inflate
    /// every `Value`.
    Map(Box<OrderedMap>),
    /// A first-class function value: a handle to a top-level function by name.
    /// `Box<str>` for the same small-cell reason as [`Value::String`].
    Func(Box<str>),
    /// An environment-capturing closure value: a parse-order `id` keying the
    /// runtime's closure-body table plus a by-value snapshot of captured locals.
    /// Boxed so its `id + captured Vec` payload (32 bytes) does not inflate every
    /// `Value`; invoked through the same `Call` dispatch as [`Value::Func`].
    Closure(Box<Closure>),
    /// A network socket handle: an index into the interpreter's per-runtime
    /// `sockets` table. The underlying OS resource (a `TcpListener`,
    /// `TcpStream`, or `UdpSocket`) is not `Clone`, so sockets are represented
    /// as opaque integer handles, mirroring how `Ptr` indexes the heap.
    Socket(usize),
    /// A live external-process handle: an index into the interpreter's per-runtime
    /// `processes` table. The underlying `std::process::Child` is not `Clone`, so a
    /// spawned process is surfaced to Lullaby as an opaque integer handle, exactly
    /// like `Socket`.
    Process(usize),
    /// An unbounded `i64` message-passing channel; shared on clone.
    Chan(Chan),
    /// A one-shot handle to a spawned detached thread; `join`ed once.
    Task(Task),
    /// A handle to an `async fn` call running on a spawned OS thread; `await`ed
    /// once to retrieve the produced value.
    Future(Future),
    /// A one-shot actor request-reply future `Future<R>`: an index into the
    /// interpreter's `actor_reply_slots` table. `ask` produces it (allocating the
    /// slot and enqueuing the request); `await` resolves it by driving the
    /// deterministic mailbox until the slot is filled by the target handler's
    /// reply. Distinct from [`Value::Future`] (an OS-thread join) because an actor
    /// reply is fulfilled cooperatively by the single-threaded scheduler, not by a
    /// thread. The value is just the slot index, so the `Value` cell stays small.
    ActorFuture(usize),
    /// A shared mutex over one `i64`; shared on clone.
    Mutex(SharedMutex),
    /// A shared atomic `i64` cell (`atomic_i64`); shared on clone. Backed by
    /// `Arc<AtomicI64>` so cross-thread updates are lock-free and visible to
    /// every holder.
    Atomic(SharedAtomic),
    /// A typed actor handle `Actor<T>`: an index into the interpreter's actor
    /// table (the actor's mailbox + private state). It is the only way to reach
    /// an actor and carries no reference into the actor's private heap; it is
    /// itself sendable, so actors can address one another. The value is just the
    /// table index, so the `Value` cell stays small.
    ActorRef(usize),
    Void,
}

/// The payload of a [`Value::Struct`]: the struct type name plus its fields in
/// declaration order. Boxed inside `Value` to keep the enum small.
#[derive(Debug, Clone, PartialEq)]
pub struct StructValue {
    pub name: String,
    pub fields: Vec<(String, Value)>,
}

/// The payload of a [`Value::Enum`]: the owning enum name, the variant name,
/// and the (positional) payload values. Boxed inside `Value` to keep the enum
/// small.
#[derive(Debug, Clone, PartialEq)]
pub struct EnumValue {
    pub enum_name: String,
    pub variant: String,
    pub payload: Vec<Value>,
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::I64(value) => write!(formatter, "{value}"),
            // The cell is normalized, so an unsigned kind prints the unsigned
            // reinterpretation and a signed kind prints its sign-extended value.
            Self::Int { value, ty } => {
                if ty.is_unsigned() {
                    write!(formatter, "{}", *value as u64)
                } else {
                    write!(formatter, "{value}")
                }
            }
            Self::F64(value) => write!(formatter, "{value}"),
            Self::F32(value) => write!(formatter, "{value}"),
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
            Self::Struct(s) => {
                let rendered = s
                    .fields
                    .iter()
                    .map(|(field, value)| format!("{field}: {value}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(formatter, "{}({rendered})", s.name)
            }
            Self::Enum(e) => {
                let variant = &e.variant;
                if e.payload.is_empty() {
                    write!(formatter, "{variant}")
                } else {
                    let rendered = e
                        .payload
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
            Self::Closure(_) => write!(formatter, "closure"),
            Self::Socket(handle) => write!(formatter, "socket({handle})"),
            Self::Process(handle) => write!(formatter, "process({handle})"),
            Self::Chan(_) => write!(formatter, "chan"),
            Self::Task(_) => write!(formatter, "task"),
            Self::Future(_) => write!(formatter, "future"),
            Self::ActorFuture(_) => write!(formatter, "future"),
            Self::Mutex(_) => write!(formatter, "mutex"),
            Self::Atomic(_) => write!(formatter, "atomic"),
            Self::ActorRef(id) => write!(formatter, "actor({id})"),
            Self::Void => write!(formatter, "void"),
        }
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

/// Build an `option<V>` runtime value using the shared `Value::Enum` option
/// representation (`some(v)` or `none`).
pub fn option_value(payload: Option<Value>) -> Value {
    match payload {
        Some(value) => Value::Enum(Box::new(EnumValue {
            enum_name: "option".to_string(),
            variant: "some".to_string(),
            payload: vec![value],
        })),
        None => Value::Enum(Box::new(EnumValue {
            enum_name: "option".to_string(),
            variant: "none".to_string(),
            payload: Vec::new(),
        })),
    }
}

/// Build a `result<T, E>` runtime value using the shared `Value::Enum` result
/// representation (`ok(v)` or `err(e)`).
pub fn result_value(payload: Result<Value, Value>) -> Value {
    match payload {
        Ok(value) => Value::Enum(Box::new(EnumValue {
            enum_name: "result".to_string(),
            variant: "ok".to_string(),
            payload: vec![value],
        })),
        Err(error) => Value::Enum(Box::new(EnumValue {
            enum_name: "result".to_string(),
            variant: "err".to_string(),
            payload: vec![error],
        })),
    }
}

/// First character index of `needle` in `text`, or `-1` when absent.
pub fn char_find(text: &str, needle: &str) -> i64 {
    match text.find(needle) {
        Some(byte_index) => text[..byte_index].chars().count() as i64,
        None => -1,
    }
}

// Captured program output for wasm builds (the browser has no real stdout).
// `print`/`println` append here; the wasm entry point drains it after a run.
#[cfg(target_arch = "wasm32")]
thread_local! {
    static WASM_STDOUT: core::cell::RefCell<String> = const { core::cell::RefCell::new(String::new()) };
}

/// Clear the captured wasm output buffer (call before a run).
#[cfg(target_arch = "wasm32")]
pub fn clear_wasm_output() {
    WASM_STDOUT.with(|b| b.borrow_mut().clear());
}

/// Take and clear the captured wasm output buffer (call after a run).
#[cfg(target_arch = "wasm32")]
pub fn take_wasm_output() -> String {
    WASM_STDOUT.with(|b| core::mem::take(&mut *b.borrow_mut()))
}

#[path = "runtime_builtins.rs"]
mod builtins;

#[path = "runtime_eval.rs"]
mod eval;

#[path = "runtime_actor.rs"]
mod actor;

/// Apply a compound assignment operator (`+=` etc.) to `current` and `rhs`,
/// supporting i64 and f64.
pub fn apply_compound(current: Value, op: &AssignOp, rhs: Value) -> Result<Value, RuntimeError> {
    // `s += piece` is string concatenation (semantics allows `+=` only when both
    // sides are strings). Reuse the left buffer so a `s += x` loop stays O(n).
    if let (Value::String(_), AssignOp::Add) = (&current, op) {
        // `s += piece` where `piece` is a `string` or a `char` (coerced to a
        // one-character string).
        return Ok(Value::String(
            (current.into_string()? + &rhs.as_concat_string()?).into(),
        ));
    }
    if let (Value::F64(a), Value::F64(b)) = (&current, &rhs) {
        let (a, b) = (*a, *b);
        return Ok(Value::F64(match op {
            AssignOp::Add => a + b,
            AssignOp::Subtract => a - b,
            AssignOp::Multiply => a * b,
            AssignOp::Divide => a / b,
            AssignOp::Remainder => {
                unreachable!("`%=` requires integer operands (rejected by semantics)")
            }
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
            // Wrap `i64::MIN /= -1` to `i64::MIN` instead of panicking, matching
            // the binary-division and native paths.
            a.wrapping_div(b)
        }
        AssignOp::Remainder => {
            if b == 0 {
                return Err(RuntimeError::new("L0404", "remainder by zero"));
            }
            a.wrapping_rem(b)
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
            let Value::Struct(s) = current else {
                return Err(RuntimeError::new(
                    "L0371",
                    format!("cannot access field `{field}` on non-struct value"),
                ));
            };
            s.fields
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
            let Value::Struct(s) = current else {
                return Err(RuntimeError::new(
                    "L0371",
                    format!("cannot access field `{field}` on non-struct value"),
                ));
            };
            s.fields
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
/// An **empty path addresses the root itself** — `set_place(&mut x, &[], v)` sets
/// `x` to `v`. Ordinary assignment never takes this path (a bare `x = v` goes
/// through `Env::assign`), but a place-backed `ptr_write(addr_of(x), v)` does, and
/// silently doing nothing here would be exactly the kind of lost write the
/// place-backed raw-pointer model exists to prevent. Keeping the empty case total
/// makes `get_place`/`set_place` agree: `get_place(v, &[])` already returns `v`.
pub fn set_place(root: &mut Value, path: &[ResolvedPlace], new: Value) -> Result<(), RuntimeError> {
    let Some((last, parents)) = path.split_last() else {
        *root = new;
        return Ok(());
    };
    let mut current = root;
    for place in parents {
        current = place_get_mut(current, place)?;
    }
    *place_get_mut(current, last)? = new;
    Ok(())
}

/// Round `offset` up to the next multiple of `align` for C-natural struct and
/// array layout. `align` is always a positive power of two in `1..=8`.
fn layout_round_up(offset: i64, align: i64) -> i64 {
    debug_assert!(align > 0);
    (offset + align - 1) / align * align
}

impl Value {
    /// Build a fixed-width integer value, normalizing the cell to `ty`'s range so
    /// the stored representation is always canonical (see [`IntKind`]).
    pub fn int(value: i64, ty: IntKind) -> Value {
        Value::Int {
            value: ty.normalize(value),
            ty,
        }
    }

    /// The C-natural raw-memory byte size of this value's type, or `None` when
    /// the value has no defined raw-memory layout (a `string`, `list`, `map`,
    /// growable/heap container, enum, closure, or OS handle). Scalars use their
    /// natural width (`i8`/`u8`/`bool`/`byte` = 1, `i16`/`u16` = 2,
    /// `i32`/`u32`/`f32`/`char` = 4, `i64`/`u64`/`f64` = 8); every pointer or
    /// reference handle (`ptr<T>`/`rc<T>`/`ref<T>`) is 8. A struct lays its
    /// fields out in declaration order, aligning each up to its natural
    /// alignment and rounding the total to the struct's own alignment (the max
    /// field alignment). A fixed `array<T>` is `n * stride(T)` where the element
    /// stride is `size_of(T)` rounded up to `align_of(T)`. See
    /// `documents/lullaby_memory_management.md`.
    pub fn layout_size(&self) -> Option<i64> {
        Some(match self {
            Value::I64(_) | Value::F64(_) | Value::Ptr(_) => 8,
            Value::Int { ty, .. } => i64::from(ty.width_bits() / 8),
            Value::F32(_) => 4,
            Value::Char(_) => 4,
            Value::Bool(_) | Value::Byte(_) => 1,
            Value::Struct(s) => {
                let mut offset = 0i64;
                let mut max_align = 1i64;
                for (_, field) in &s.fields {
                    let size = field.layout_size()?;
                    let align = field.layout_align()?;
                    max_align = max_align.max(align);
                    offset = layout_round_up(offset, align) + size;
                }
                layout_round_up(offset, max_align)
            }
            Value::Array(values) => match values.first() {
                Some(element) => {
                    let stride = layout_round_up(element.layout_size()?, element.layout_align()?);
                    stride * values.len() as i64
                }
                // Zero elements occupy zero bytes regardless of the (here
                // unrecoverable) element type.
                None => 0,
            },
            _ => return None,
        })
    }

    /// The C-natural alignment of this value's type (see [`Value::layout_size`]),
    /// or `None` when the value has no defined raw-memory layout.
    pub fn layout_align(&self) -> Option<i64> {
        Some(match self {
            Value::I64(_) | Value::F64(_) | Value::Ptr(_) => 8,
            Value::Int { ty, .. } => i64::from(ty.width_bits() / 8),
            Value::F32(_) => 4,
            Value::Char(_) => 4,
            Value::Bool(_) | Value::Byte(_) => 1,
            Value::Struct(s) => {
                let mut max_align = 1i64;
                for (_, field) in &s.fields {
                    max_align = max_align.max(field.layout_align()?);
                }
                max_align
            }
            Value::Array(values) => match values.first() {
                Some(element) => element.layout_align()?,
                None => 1,
            },
            _ => return None,
        })
    }

    /// The C-natural byte offset of `field` within this struct value, or `None`
    /// when this is not a struct, has no such field, or a preceding field has no
    /// defined layout. Fields are laid out in declaration order per
    /// [`Value::layout_size`].
    pub fn layout_field_offset(&self, field: &str) -> Option<i64> {
        let Value::Struct(s) = self else {
            return None;
        };
        let mut offset = 0i64;
        for (name, value) in &s.fields {
            offset = layout_round_up(offset, value.layout_align()?);
            if name == field {
                return Some(offset);
            }
            offset += value.layout_size()?;
        }
        None
    }

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
            Self::String(value) => Ok((value.clone()).into()),
            _ => Err(RuntimeError::new("L0417", "expected string value")),
        }
    }

    /// A concatenation operand for `+`/`+=`: a `String` as-is, or a `char`
    /// rendered as a one-character string. Semantics restricts `string`
    /// concatenation operands to `string`/`char`, so nothing else reaches here.
    pub fn as_concat_string(&self) -> Result<String, RuntimeError> {
        match self {
            Self::String(value) => Ok((value.clone()).into()),
            Self::Char(c) => Ok(c.to_string()),
            _ => Err(RuntimeError::new("L0417", "expected string or char value")),
        }
    }

    /// Move the owned `String` out of a [`Value::String`] without cloning its
    /// heap buffer. Used for the left operand of string `+` so the concatenation
    /// reuses (and grows in place) that buffer instead of allocating a fresh one;
    /// the result is byte-identical to `as_string`, only cheaper.
    pub fn into_string(self) -> Result<String, RuntimeError> {
        match self {
            Self::String(value) => Ok((value).into()),
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
#[path = "runtime_tests.rs"]
mod tests;
#[cfg(test)]
mod value_size_check {
    /// The interpreters move and clone `Value` on every operation, so its size is
    /// on the hot path. Boxing the four largest variants (`String`/`Array` as
    /// `Box<str>`/`Box<[Value]>`, `Func` as `Box<str>`, `Closure` as
    /// `Box<Closure>`) keeps the cell at 24 bytes instead of 32. This guards
    /// against a future variant silently inflating it again.
    #[test]
    fn value_cell_stays_small() {
        assert!(
            std::mem::size_of::<super::Value>() <= 24,
            "Value grew past 24 bytes ({}); box the offending variant",
            std::mem::size_of::<super::Value>()
        );
    }
}
