use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
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
        Value::Mutex(_) => "Mutex".to_string(),
        Value::Atomic(_) => "atomic_i64".to_string(),
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

/// A shared atomic `i64` cell. `Arc<AtomicI64>`, so the value's `Clone` shares
/// the same lock-free cell across threads (reference semantics, exactly like
/// [`SharedMutex`], but backed by `std::sync::atomic` for wait-free access).
/// `Send + Sync`, so the handle crosses thread boundaries safely. Every
/// operation uses `Ordering::SeqCst` in this increment; weaker orderings are a
/// documented future optimization.
#[derive(Debug, Clone)]
pub struct SharedAtomic {
    pub cell: Arc<AtomicI64>,
}

impl PartialEq for SharedAtomic {
    /// Atomic handles compare by identity of the shared cell.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cell, &other.cell)
    }
}

/// Width/signedness tag for the fixed-width integer lattice carried by
/// [`Value::Int`]. The stored `i64` cell is always kept normalized to the kind's
/// range (truncate to width, then sign- or zero-extend). Signed kinds sit in
/// their signed range so plain `i64` ordering is correct; unsigned kinds are
/// zero-extended, so the ≤32-bit ones also order as `i64`. The 64-bit unsigned
/// kinds (`u64`/`usize`) can hold values above `i64::MAX`, stored bit-for-bit as
/// a negative `i64`, so division and ordering consult [`IntKind::is_unsigned`]
/// and operate on the `u64` reinterpretation. Every dynamic backend (AST
/// runtime, IR interpreter, bytecode VM) normalizes at the same points so
/// results agree bit-for-bit; `usize`/`isize` are 64-bit on the current targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntKind {
    /// Signed 8-bit.
    I8,
    /// Signed 16-bit.
    I16,
    /// Signed 32-bit.
    I32,
    /// Unsigned 8-bit. Distinct from `byte` (the raw-I/O octet): `u8` arithmetic
    /// wraps, whereas `byte()` construction errors outside 0-255.
    U8,
    /// Unsigned 16-bit.
    U16,
    /// Unsigned 32-bit.
    U32,
    /// Unsigned 64-bit.
    U64,
    /// Pointer-sized signed (64-bit on the current targets).
    Isize,
    /// Pointer-sized unsigned (64-bit on the current targets).
    Usize,
}

impl IntKind {
    /// The canonical Lullaby type name for this integer kind.
    pub fn type_name(self) -> &'static str {
        match self {
            IntKind::I8 => "i8",
            IntKind::I16 => "i16",
            IntKind::I32 => "i32",
            IntKind::U8 => "u8",
            IntKind::U16 => "u16",
            IntKind::U32 => "u32",
            IntKind::U64 => "u64",
            IntKind::Isize => "isize",
            IntKind::Usize => "usize",
        }
    }

    /// The bit width of this kind (8/16/32/64). `usize`/`isize` are 64-bit on the
    /// current targets. Used for shift-amount masking.
    pub fn width_bits(self) -> u32 {
        match self {
            IntKind::I8 | IntKind::U8 => 8,
            IntKind::I16 | IntKind::U16 => 16,
            IntKind::I32 | IntKind::U32 => 32,
            IntKind::U64 | IntKind::Isize | IntKind::Usize => 64,
        }
    }

    /// Whether this kind is unsigned. Division and ordering of unsigned kinds use
    /// the `u64` reinterpretation of the normalized cell.
    pub fn is_unsigned(self) -> bool {
        matches!(
            self,
            IntKind::U8 | IntKind::U16 | IntKind::U32 | IntKind::U64 | IntKind::Usize
        )
    }

    /// Normalize a mathematical `i64` result into this kind's range: truncate to
    /// the kind's width, then sign-extend (signed kinds) or zero-extend
    /// (unsigned kinds) back into the `i64` cell. Total and deterministic — this
    /// is the wrapping default shared by every backend. The 64-bit kinds occupy
    /// the whole cell, so normalization is the identity on the bits.
    pub fn normalize(self, value: i64) -> i64 {
        match self {
            IntKind::I8 => i64::from(value as i8),
            IntKind::I16 => i64::from(value as i16),
            IntKind::I32 => i64::from(value as i32),
            IntKind::U8 => i64::from(value as u8),
            IntKind::U16 => i64::from(value as u16),
            IntKind::U32 => i64::from(value as u32),
            IntKind::U64 | IntKind::Usize | IntKind::Isize => value,
        }
    }

    /// The inclusive `[min, max]` range of this kind as `i128`, wide enough to
    /// hold every kind (up to `u64::MAX`). Used by checked/saturating arithmetic
    /// to detect and clamp overflow exactly.
    pub fn range_i128(self) -> (i128, i128) {
        match self {
            IntKind::I8 => (i128::from(i8::MIN), i128::from(i8::MAX)),
            IntKind::I16 => (i128::from(i16::MIN), i128::from(i16::MAX)),
            IntKind::I32 => (i128::from(i32::MIN), i128::from(i32::MAX)),
            IntKind::Isize => (i128::from(i64::MIN), i128::from(i64::MAX)),
            IntKind::U8 => (0, i128::from(u8::MAX)),
            IntKind::U16 => (0, i128::from(u16::MAX)),
            IntKind::U32 => (0, i128::from(u32::MAX)),
            IntKind::U64 | IntKind::Usize => (0, i128::from(u64::MAX)),
        }
    }

    /// The mathematical value of a normalized `i64` cell of this kind as `i128`
    /// (unsigned kinds read the cell as `u64`, so a negative cell becomes its
    /// large positive magnitude).
    pub fn value_to_i128(self, cell: i64) -> i128 {
        if self.is_unsigned() {
            i128::from(cell as u64)
        } else {
            i128::from(cell)
        }
    }

    /// Pack an in-range `i128` value back into this kind's `i64` cell (the
    /// inverse of [`IntKind::value_to_i128`] for values within `range_i128`).
    pub fn i128_to_cell(self, value: i128) -> i64 {
        if self.is_unsigned() {
            value as u64 as i64
        } else {
            value as i64
        }
    }
}

/// The three arithmetic operations that the overflow-aware builtins provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
}

/// Signedness-aware quotient of two normalized `i64` cells tagged `ty`; the
/// caller guarantees a non-zero divisor. Unsigned kinds divide on the `u64`
/// reinterpretation so 64-bit unsigned values above `i64::MAX` divide correctly.
pub fn int_div(left: i64, right: i64, ty: IntKind) -> i64 {
    if ty.is_unsigned() {
        (left as u64).wrapping_div(right as u64) as i64
    } else {
        left.wrapping_div(right)
    }
}

/// Signedness-aware remainder of two normalized `i64` cells tagged `ty`; the
/// caller guarantees a non-zero divisor. Signed kinds use the truncated
/// remainder (sign of the dividend, like C/Rust `%`); unsigned kinds take the
/// remainder on the `u64` reinterpretation. The result magnitude is smaller than
/// the divisor, so it is already normalized to the kind's width.
pub fn int_rem(left: i64, right: i64, ty: IntKind) -> i64 {
    if ty.is_unsigned() {
        (left as u64).wrapping_rem(right as u64) as i64
    } else {
        left.wrapping_rem(right)
    }
}

/// Left shift of a normalized fixed-width cell, with the shift amount masked to
/// the kind's width (`amount & (width-1)` — total and deterministic, like the
/// `i64` shift) and the result re-normalized to the width.
pub fn int_shl(value: i64, amount: i64, ty: IntKind) -> i64 {
    let masked = (amount as u64 & u64::from(ty.width_bits() - 1)) as u32;
    ty.normalize(value.wrapping_shl(masked))
}

/// Right shift of a normalized fixed-width cell: logical (zero-filling) for
/// unsigned kinds, arithmetic (sign-preserving) for signed kinds, with the same
/// masked amount as [`int_shl`].
pub fn int_shr(value: i64, amount: i64, ty: IntKind) -> i64 {
    let masked = (amount as u64 & u64::from(ty.width_bits() - 1)) as u32;
    let shifted = if ty.is_unsigned() {
        (value as u64).wrapping_shr(masked) as i64
    } else {
        value.wrapping_shr(masked)
    };
    ty.normalize(shifted)
}

/// Signedness-aware ordering of two normalized `i64` cells tagged `ty`. Unsigned
/// kinds compare on the `u64` reinterpretation (correct for the 64-bit unsigned
/// kinds whose cells may be negative `i64`s); signed kinds compare as `i64`.
pub fn int_cmp(left: i64, right: i64, ty: IntKind) -> std::cmp::Ordering {
    if ty.is_unsigned() {
        (left as u64).cmp(&(right as u64))
    } else {
        left.cmp(&right)
    }
}

/// Overflow behaviour selector for the `checked_*`/`saturating_*`/`wrapping_*`
/// arithmetic builtins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowMode {
    /// `option<T>`: `none` when the true result is outside `T`.
    Checked,
    /// `T`: clamp to `T`'s bounds.
    Saturating,
    /// `T`: wrap modulo the type width (the default `+`/`-`/`*` behaviour).
    Wrapping,
}

/// Unwrap a `Value::Int`, returning its normalized cell and kind, or an `L0407`
/// runtime error otherwise.
pub fn expect_fixed_int(name: &str, value: &Value) -> Result<(i64, IntKind), RuntimeError> {
    match value {
        Value::Int { value, ty } => Ok((*value, *ty)),
        other => Err(RuntimeError::new(
            "L0407",
            format!("{name} expects a fixed-width integer but got `{other}`"),
        )),
    }
}

/// Shared implementation of the overflow-aware arithmetic builtins. Both operands
/// must be the same fixed-width kind (enforced by the type checker); the true
/// result is computed in `i128` — wide enough that no fixed-width add/sub/mul
/// overflows it — then resolved per `mode`. Identical on every backend.
pub fn overflow_arith(
    name: &str,
    args: Vec<Value>,
    op: ArithOp,
    mode: OverflowMode,
) -> Result<Value, RuntimeError> {
    let [a, b]: [Value; 2] = args.try_into().map_err(|args: Vec<Value>| {
        RuntimeError::new(
            "L0407",
            format!("{name} expects 2 arguments but got {}", args.len()),
        )
    })?;
    let (la, ta) = expect_fixed_int(name, &a)?;
    let (lb, tb) = expect_fixed_int(name, &b)?;
    if ta != tb {
        return Err(RuntimeError::new(
            "L0407",
            format!("{name} operands must have the same integer type"),
        ));
    }
    // The exact result in `i128`. Add/Sub of any two fixed-width operands always
    // fit `i128`; only unsigned 64-bit `Mul` can exceed it (`u64::MAX^2` is just
    // over `i128::MAX`). Because unsigned operands are non-negative, such a
    // product is unambiguously above `max`, so `checked_mul` returning `None`
    // exactly IS the overflow signal — no `i128` multiply ever panics.
    let (la128, lb128) = (ta.value_to_i128(la), ta.value_to_i128(lb));
    let exact = match op {
        ArithOp::Add => la128.checked_add(lb128),
        ArithOp::Sub => la128.checked_sub(lb128),
        ArithOp::Mul => la128.checked_mul(lb128),
    };
    let (min, max) = ta.range_i128();
    Ok(match mode {
        // Two's-complement wrap on the raw normalized cells (bit-level identical
        // for signed/unsigned add/sub/mul); `Value::int` then re-normalizes to the
        // kind's width. This is total even when the exact `i128` would overflow.
        OverflowMode::Wrapping => {
            let low = match op {
                ArithOp::Add => la.wrapping_add(lb),
                ArithOp::Sub => la.wrapping_sub(lb),
                ArithOp::Mul => la.wrapping_mul(lb),
            };
            Value::int(low, ta)
        }
        OverflowMode::Saturating => match exact {
            Some(wide) => Value::int(ta.i128_to_cell(wide.clamp(min, max)), ta),
            // Only unsigned 64-bit mul reaches `None`; the true product is
            // positive and above `max`, so saturation clamps to `max`.
            None => Value::int(ta.i128_to_cell(max), ta),
        },
        OverflowMode::Checked => match exact {
            Some(wide) if wide >= min && wide <= max => {
                option_value(Some(Value::int(ta.i128_to_cell(wide), ta)))
            }
            _ => option_value(None),
        },
    })
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

/// A hashable projection of a `map<K, V>` key. The type system restricts map
/// keys to `i64` or `string` (both hashable) — see `map_key_ok` in
/// `lullaby_semantics` — so this enum captures exactly those two kinds and lets
/// [`OrderedMap`] index entries by key for O(1) lookup. Any other value kind
/// (never produced for a well-typed program) has no projection; the map falls
/// back to a linear scan so `Value` equality semantics are preserved exactly.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum MapKey {
    I64(i64),
    Str(String),
}

impl MapKey {
    fn from_value(value: &Value) -> Option<MapKey> {
        match value {
            Value::I64(n) => Some(MapKey::I64(*n)),
            Value::String(s) => Some(MapKey::Str(s.clone())),
            _ => None,
        }
    }
}

/// An insertion-ordered `map<K, V>` value.
///
/// `entries` is the single source of truth for iteration order and value
/// equality — it is byte-for-byte the old `Vec<(Value, Value)>` representation,
/// so `map_keys`/`map_values` iterate in insertion order and `==` compares the
/// entries element-wise in order, unchanged. `index` maps each hashable key to
/// its position in `entries`, turning `map_get`/`map_has`/`map_set`-of-an-
/// existing-key from an O(n) linear scan into an O(1) hash probe.
///
/// Only `entries` is observable, so `PartialEq` compares just that (`index` is a
/// derived acceleration structure), and `Clone`/`Debug` are manual for the same
/// reason — `Debug` renders as the entries vector so `Value`'s derived `Debug`
/// output is identical to the previous `Map([..])` form.
pub struct OrderedMap {
    entries: Vec<(Value, Value)>,
    index: HashMap<MapKey, usize>,
}

impl OrderedMap {
    /// A fresh empty map.
    pub fn new() -> Self {
        OrderedMap {
            entries: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The value mapped to `key`, if present. O(1) for the guaranteed
    /// `i64`/`string` keys, with a linear-scan fallback for any other kind.
    pub fn get(&self, key: &Value) -> Option<&Value> {
        match MapKey::from_value(key) {
            Some(mk) => self.index.get(&mk).map(|&i| &self.entries[i].1),
            None => self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        }
    }

    /// Whether `key` is present. O(1) for `i64`/`string` keys.
    pub fn contains_key(&self, key: &Value) -> bool {
        match MapKey::from_value(key) {
            Some(mk) => self.index.contains_key(&mk),
            None => self.entries.iter().any(|(k, _)| k == key),
        }
    }

    /// Insert or overwrite `key -> value`, preserving the position of an
    /// existing key. O(1) for `i64`/`string` keys.
    pub fn insert(&mut self, key: Value, value: Value) {
        match MapKey::from_value(&key) {
            Some(mk) => match self.index.get(&mk) {
                Some(&i) => self.entries[i].1 = value,
                None => {
                    let position = self.entries.len();
                    self.entries.push((key, value));
                    self.index.insert(mk, position);
                }
            },
            None => match self.entries.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 = value,
                None => self.entries.push((key, value)),
            },
        }
    }

    /// Remove `key` if present, preserving the order of the remaining entries.
    /// O(n) (rare operation): the vector shift plus an index rebuild.
    pub fn remove(&mut self, key: &Value) {
        let position = match MapKey::from_value(key) {
            Some(mk) => self.index.get(&mk).copied(),
            None => self.entries.iter().position(|(k, _)| k == key),
        };
        if let Some(i) = position {
            self.entries.remove(i);
            self.reindex();
        }
    }

    /// Rebuild `index` from `entries` after positions shift.
    fn reindex(&mut self) {
        self.index.clear();
        self.index.reserve(self.entries.len());
        for (i, (key, _)) in self.entries.iter().enumerate() {
            if let Some(mk) = MapKey::from_value(key) {
                self.index.insert(mk, i);
            }
        }
    }

    /// Iterate the entries in insertion order.
    pub fn iter(&self) -> std::slice::Iter<'_, (Value, Value)> {
        self.entries.iter()
    }

    /// Consume the map, yielding the entries in insertion order.
    pub fn into_entries(self) -> Vec<(Value, Value)> {
        self.entries
    }
}

impl Default for OrderedMap {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for OrderedMap {
    fn clone(&self) -> Self {
        OrderedMap {
            entries: self.entries.clone(),
            index: self.index.clone(),
        }
    }
}

// Only the entries are observable; the index is a derived acceleration
// structure, so equality compares entries alone. This is byte-for-byte the old
// `Vec<(Value, Value)>` element-wise, in-order comparison.
impl PartialEq for OrderedMap {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

// Render as the entries vector so `Value`'s derived `Debug` output stays
// identical to the previous `Map([(k, v), ..])` representation.
impl fmt::Debug for OrderedMap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.entries.fmt(formatter)
    }
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
    String(String),
    /// A Unicode scalar value.
    Char(char),
    /// An 8-bit unsigned integer (0-255).
    Byte(u8),
    Array(Vec<Value>),
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
    /// No environment is captured in this increment.
    Func(String),
    /// An environment-capturing closure value: a parse-order `id` keying the
    /// runtime's closure-body table plus a by-value snapshot of captured locals.
    /// Invoked through the same `Call` dispatch as [`Value::Func`].
    Closure(Closure),
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
    /// A shared mutex over one `i64`; shared on clone.
    Mutex(SharedMutex),
    /// A shared atomic `i64` cell (`atomic_i64`); shared on clone. Backed by
    /// `Arc<AtomicI64>` so cross-thread updates are lock-free and visible to
    /// every holder.
    Atomic(SharedAtomic),
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
            Self::Mutex(_) => write!(formatter, "mutex"),
            Self::Atomic(_) => write!(formatter, "atomic"),
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

/// Unwrap a runtime `Value` expected to be an `atomic_i64` handle, reporting
/// `L0417` otherwise.
pub fn expect_atomic(name: &str, value: Value) -> Result<SharedAtomic, RuntimeError> {
    match value {
        Value::Atomic(atomic) => Ok(atomic),
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects an atomic_i64 but got `{other}`"),
        )),
    }
}

/// The five `MemoryOrder` enum variant names, in strengthening order. Registered
/// as a compiler-provided nominal enum so `relaxed`/`acquire`/`release`/
/// `acq_rel`/`seq_cst` construct `MemoryOrder` unit-variant values that the
/// ordering-taking atomic builtins and `fence` decode into real
/// `std::sync::atomic::Ordering` values.
pub const MEMORY_ORDER_VARIANTS: [&str; 5] =
    ["relaxed", "acquire", "release", "acq_rel", "seq_cst"];

/// Decode a `MemoryOrder` unit-variant runtime value into the corresponding
/// `std::sync::atomic::Ordering`. Semantics guarantees the argument type, so a
/// non-`MemoryOrder` value here indicates an interpreter bug and reports
/// `L0432`.
pub fn expect_memory_order(name: &str, value: Value) -> Result<Ordering, RuntimeError> {
    match value {
        Value::Enum(e) if e.enum_name == "MemoryOrder" => match e.variant.as_str() {
            "relaxed" => Ok(Ordering::Relaxed),
            "acquire" => Ok(Ordering::Acquire),
            "release" => Ok(Ordering::Release),
            "acq_rel" => Ok(Ordering::AcqRel),
            "seq_cst" => Ok(Ordering::SeqCst),
            other => Err(RuntimeError::new(
                "L0432",
                format!("{name} received an unknown MemoryOrder variant `{other}`"),
            )),
        },
        other => Err(RuntimeError::new(
            "L0432",
            format!("{name} expects a MemoryOrder but got `{other}`"),
        )),
    }
}

/// Guard that `order` is a legal ordering for an atomic *load* (or a CAS failure
/// ordering): `relaxed`, `acquire`, or `seq_cst`. `release`/`acq_rel` would
/// panic inside `std`, so they are rejected with `L0432` first.
fn load_ordering(name: &str, order: Ordering) -> Result<Ordering, RuntimeError> {
    match order {
        Ordering::Relaxed | Ordering::Acquire | Ordering::SeqCst => Ok(order),
        _ => Err(RuntimeError::new(
            "L0432",
            format!("{name} cannot use a `release`/`acq_rel` ordering for a load"),
        )),
    }
}

/// Guard that `order` is a legal ordering for an atomic *store*: `relaxed`,
/// `release`, or `seq_cst`. `acquire`/`acq_rel` would panic inside `std`.
fn store_ordering(name: &str, order: Ordering) -> Result<Ordering, RuntimeError> {
    match order {
        Ordering::Relaxed | Ordering::Release | Ordering::SeqCst => Ok(order),
        _ => Err(RuntimeError::new(
            "L0432",
            format!("{name} cannot use an `acquire`/`acq_rel` ordering for a store"),
        )),
    }
}

/// Guard that `order` is a legal ordering for a `fence`: `acquire`, `release`,
/// `acq_rel`, or `seq_cst`. `relaxed` would panic inside `std`.
fn fence_ordering(name: &str, order: Ordering) -> Result<Ordering, RuntimeError> {
    match order {
        Ordering::Acquire | Ordering::Release | Ordering::AcqRel | Ordering::SeqCst => Ok(order),
        _ => Err(RuntimeError::new(
            "L0432",
            format!("{name} cannot use a `relaxed` ordering"),
        )),
    }
}

/// Build the `L0405` arity error for a free-standing ordering builtin.
fn ordering_arity(name: &str, expected: usize, actual: usize) -> RuntimeError {
    RuntimeError::new(
        "L0405",
        format!("function `{name}` expects {expected} arguments but got {actual}"),
    )
}

/// `atomic_load_ordered(a atomic_i64, order MemoryOrder) -> i64`: read the cell
/// under `order` (`relaxed`/`acquire`/`seq_cst`), mapping to the real
/// `std::sync::atomic::Ordering`.
pub fn builtin_atomic_load_ordered(args: Vec<Value>) -> Result<Value, RuntimeError> {
    let name = "atomic_load_ordered";
    let [atomic, order]: [Value; 2] = args
        .try_into()
        .map_err(|args: Vec<Value>| ordering_arity(name, 2, args.len()))?;
    let atomic = expect_atomic(name, atomic)?;
    let order = load_ordering(name, expect_memory_order(name, order)?)?;
    Ok(Value::I64(atomic.cell.load(order)))
}

/// `atomic_store_ordered(a atomic_i64, v i64, order MemoryOrder) -> void`: write
/// the cell under `order` (`relaxed`/`release`/`seq_cst`).
pub fn builtin_atomic_store_ordered(args: Vec<Value>) -> Result<Value, RuntimeError> {
    let name = "atomic_store_ordered";
    let [atomic, value, order]: [Value; 3] = args
        .try_into()
        .map_err(|args: Vec<Value>| ordering_arity(name, 3, args.len()))?;
    let atomic = expect_atomic(name, atomic)?;
    let value = expect_i64(name, value)?;
    let order = store_ordering(name, expect_memory_order(name, order)?)?;
    atomic.cell.store(value, order);
    Ok(Value::Void)
}

/// Shared decode for the ordered read-modify-write family: an atomic handle, an
/// `i64` operand, and any of the five orderings (all are valid for an RMW).
fn atomic_rmw_ordered_args(
    name: &str,
    args: Vec<Value>,
) -> Result<(SharedAtomic, i64, Ordering), RuntimeError> {
    let [atomic, value, order]: [Value; 3] = args
        .try_into()
        .map_err(|args: Vec<Value>| ordering_arity(name, 3, args.len()))?;
    let atomic = expect_atomic(name, atomic)?;
    let value = expect_i64(name, value)?;
    let order = expect_memory_order(name, order)?;
    Ok((atomic, value, order))
}

/// `atomic_swap_ordered(a atomic_i64, v i64, order MemoryOrder) -> i64`: store
/// `v`, return the previous value, under any of the five orderings.
pub fn builtin_atomic_swap_ordered(args: Vec<Value>) -> Result<Value, RuntimeError> {
    let (atomic, value, order) = atomic_rmw_ordered_args("atomic_swap_ordered", args)?;
    Ok(Value::I64(atomic.cell.swap(value, order)))
}

/// `atomic_add_ordered(a atomic_i64, v i64, order MemoryOrder) -> i64`:
/// fetch-and-add returning the previous value, under any of the five orderings.
pub fn builtin_atomic_add_ordered(args: Vec<Value>) -> Result<Value, RuntimeError> {
    let (atomic, value, order) = atomic_rmw_ordered_args("atomic_add_ordered", args)?;
    Ok(Value::I64(atomic.cell.fetch_add(value, order)))
}

/// `atomic_sub_ordered(a atomic_i64, v i64, order MemoryOrder) -> i64`:
/// fetch-and-sub returning the previous value, under any of the five orderings.
pub fn builtin_atomic_sub_ordered(args: Vec<Value>) -> Result<Value, RuntimeError> {
    let (atomic, value, order) = atomic_rmw_ordered_args("atomic_sub_ordered", args)?;
    Ok(Value::I64(atomic.cell.fetch_sub(value, order)))
}

/// `atomic_and_ordered(a atomic_i64, v i64, order MemoryOrder) -> i64`:
/// fetch-and-and returning the previous value, under any of the five orderings.
pub fn builtin_atomic_and_ordered(args: Vec<Value>) -> Result<Value, RuntimeError> {
    let (atomic, value, order) = atomic_rmw_ordered_args("atomic_and_ordered", args)?;
    Ok(Value::I64(atomic.cell.fetch_and(value, order)))
}

/// `atomic_or_ordered(a atomic_i64, v i64, order MemoryOrder) -> i64`:
/// fetch-and-or returning the previous value, under any of the five orderings.
pub fn builtin_atomic_or_ordered(args: Vec<Value>) -> Result<Value, RuntimeError> {
    let (atomic, value, order) = atomic_rmw_ordered_args("atomic_or_ordered", args)?;
    Ok(Value::I64(atomic.cell.fetch_or(value, order)))
}

/// `atomic_xor_ordered(a atomic_i64, v i64, order MemoryOrder) -> i64`:
/// fetch-and-xor returning the previous value, under any of the five orderings.
pub fn builtin_atomic_xor_ordered(args: Vec<Value>) -> Result<Value, RuntimeError> {
    let (atomic, value, order) = atomic_rmw_ordered_args("atomic_xor_ordered", args)?;
    Ok(Value::I64(atomic.cell.fetch_xor(value, order)))
}

/// `atomic_cas_ordered(a atomic_i64, expected i64, new i64, success MemoryOrder,
/// failure MemoryOrder) -> i64`: strong compare-and-swap returning the observed
/// value. `success` may be any of the five orderings; `failure` must be a valid
/// load ordering (`relaxed`/`acquire`/`seq_cst`), exactly as `std` requires.
pub fn builtin_atomic_cas_ordered(args: Vec<Value>) -> Result<Value, RuntimeError> {
    let name = "atomic_cas_ordered";
    let [atomic, expected, new, success, failure]: [Value; 5] = args
        .try_into()
        .map_err(|args: Vec<Value>| ordering_arity(name, 5, args.len()))?;
    let atomic = expect_atomic(name, atomic)?;
    let expected = expect_i64(name, expected)?;
    let new = expect_i64(name, new)?;
    let success = expect_memory_order(name, success)?;
    let failure = load_ordering(name, expect_memory_order(name, failure)?)?;
    let observed = match atomic
        .cell
        .compare_exchange(expected, new, success, failure)
    {
        Ok(prev) => prev,
        Err(current) => current,
    };
    Ok(Value::I64(observed))
}

/// `fence(order MemoryOrder) -> void`: a standalone memory fence mapping to
/// `std::sync::atomic::fence(order)`. `order` must be `acquire`/`release`/
/// `acq_rel`/`seq_cst` (a `relaxed` fence is meaningless and `std` panics on it).
pub fn builtin_fence(args: Vec<Value>) -> Result<Value, RuntimeError> {
    let name = "fence";
    let [order]: [Value; 1] = args
        .try_into()
        .map_err(|args: Vec<Value>| ordering_arity(name, 1, args.len()))?;
    let order = fence_ordering(name, expect_memory_order(name, order)?)?;
    std::sync::atomic::fence(order);
    Ok(Value::Void)
}

/// Unwrap a runtime `Value` expected to be a map, reporting `L0417` otherwise.
pub fn expect_map(name: &str, value: Value) -> Result<OrderedMap, RuntimeError> {
    match value {
        Value::Map(entries) => Ok(*entries),
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

/// Greatest common divisor of two `i64` values, over their absolute values.
///
/// Total on every input: `gcd(0, 0) == 0`, `gcd(0, n) == |n|`, and the result
/// is always non-negative. `i64::MIN.abs()` overflows, so absolute values are
/// taken in the wider `i128` domain and the Euclidean loop runs there before the
/// (always in-range) result is narrowed back to `i64`.
pub fn gcd_i64(a: i64, b: i64) -> i64 {
    let mut x = (a as i128).unsigned_abs();
    let mut y = (b as i128).unsigned_abs();
    while y != 0 {
        let r = x % y;
        x = y;
        y = r;
    }
    // `x` is bounded by `max(|a|, |b|) <= 2^63`, so `2^63` (the `i64::MIN` case)
    // is the only value that would not fit; but it can only appear when the
    // other operand is `0`, and `gcd(0, i64::MIN) == 2^63` overflows `i64`. Wrap
    // that single case to `i64::MIN` (its own magnitude) to stay total.
    x as i64
}

/// Sum the elements of a numeric list. `list<i64>` sums with wrapping
/// arithmetic (matching the interpreter's `+`), `list<f64>` sums as f64. An
/// empty list yields `0`/`0.0` (defaulting to `i64` `0`, which the semantic
/// type check pins to the element type). A non-numeric element is a runtime
/// type error (`L0417`).
pub fn list_sum_values(name: &str, values: Vec<Value>) -> Result<Value, RuntimeError> {
    let mut iter = values.into_iter();
    let Some(first) = iter.next() else {
        return Ok(Value::I64(0));
    };
    match first {
        Value::I64(mut acc) => {
            for value in iter {
                match value {
                    Value::I64(n) => acc = acc.wrapping_add(n),
                    other => return Err(mixed_numeric_list_error(name, &other)),
                }
            }
            Ok(Value::I64(acc))
        }
        Value::F64(mut acc) => {
            for value in iter {
                match value {
                    Value::F64(n) => acc += n,
                    other => return Err(mixed_numeric_list_error(name, &other)),
                }
            }
            Ok(Value::F64(acc))
        }
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a list<i64> or list<f64> but found `{other}`"),
        )),
    }
}

/// Find the extreme (min when `want_max` is false, max otherwise) element of a
/// numeric list, returning `None` for an empty list. f64 comparisons use total
/// ordering so `NaN` participates deterministically. A non-numeric or mixed
/// element is a runtime type error (`L0417`).
pub fn list_extreme(
    name: &str,
    values: Vec<Value>,
    want_max: bool,
) -> Result<Option<Value>, RuntimeError> {
    let mut iter = values.into_iter();
    let Some(first) = iter.next() else {
        return Ok(None);
    };
    match first {
        Value::I64(mut best) => {
            for value in iter {
                match value {
                    Value::I64(n) => {
                        if (want_max && n > best) || (!want_max && n < best) {
                            best = n;
                        }
                    }
                    other => return Err(mixed_numeric_list_error(name, &other)),
                }
            }
            Ok(Some(Value::I64(best)))
        }
        Value::F64(mut best) => {
            for value in iter {
                match value {
                    Value::F64(n) => {
                        let ordering = n.total_cmp(&best);
                        let replace = if want_max {
                            ordering == std::cmp::Ordering::Greater
                        } else {
                            ordering == std::cmp::Ordering::Less
                        };
                        if replace {
                            best = n;
                        }
                    }
                    other => return Err(mixed_numeric_list_error(name, &other)),
                }
            }
            Ok(Some(Value::F64(best)))
        }
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a list<i64> or list<f64> but found `{other}`"),
        )),
    }
}

/// Sort a scalar list ascending, dispatching on the element type. Supports
/// `i64`, `f64` (total order via `total_cmp`, so `NaN` sorts deterministically),
/// and `string` (lexicographic by Rust `str` ordering). The list must be
/// homogeneous; a mixed or unsupported element type yields `L0417`.
pub fn sort_scalar_list(name: &str, values: Vec<Value>) -> Result<Value, RuntimeError> {
    let Some(first) = values.first() else {
        return Ok(Value::Array(Vec::new()));
    };
    match first {
        Value::I64(_) => {
            let mut nums: Vec<i64> = Vec::with_capacity(values.len());
            for value in values {
                match value {
                    Value::I64(n) => nums.push(n),
                    other => return Err(mixed_numeric_list_error(name, &other)),
                }
            }
            nums.sort();
            Ok(Value::Array(nums.into_iter().map(Value::I64).collect()))
        }
        Value::F64(_) => {
            let mut nums: Vec<f64> = Vec::with_capacity(values.len());
            for value in values {
                match value {
                    Value::F64(n) => nums.push(n),
                    other => return Err(mixed_numeric_list_error(name, &other)),
                }
            }
            nums.sort_by(|a, b| a.total_cmp(b));
            Ok(Value::Array(nums.into_iter().map(Value::F64).collect()))
        }
        Value::String(_) => {
            let mut strs: Vec<String> = Vec::with_capacity(values.len());
            for value in values {
                match value {
                    Value::String(s) => strs.push(s),
                    other => return Err(mixed_numeric_list_error(name, &other)),
                }
            }
            strs.sort();
            Ok(Value::Array(strs.into_iter().map(Value::String).collect()))
        }
        other => Err(RuntimeError::new(
            "L0417",
            format!("{name} expects a list<i64>, list<f64>, or list<string> but found `{other}`"),
        )),
    }
}

fn mixed_numeric_list_error(name: &str, value: &Value) -> RuntimeError {
    RuntimeError::new(
        "L0417",
        format!("{name} expects a homogeneous numeric list but found `{value}`"),
    )
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

/// A live external process held behind a `Value::Process` handle. Not `Clone`
/// (a `std::process::Child` owns OS resources), which is why processes are
/// surfaced to Lullaby as opaque integer handles, exactly like `SocketResource`.
/// Shared by the AST interpreter and the IR interpreter / bytecode VM so every
/// backend keeps identical process semantics. `stdout`/`stderr` are taken out of
/// the `Child` on the first `proc_stdout`/`proc_stderr` read (a `ChildStdout`
/// cannot be read twice), leaving `None` behind so a second read returns EOF.
pub struct ProcessResource {
    pub child: Child,
}

/// Which captured pipe a `proc_stdout`/`proc_stderr` read should drain.
#[derive(Clone, Copy)]
enum PipeKind {
    Stdout,
    Stderr,
}

/// Convert a finished child's exit status into the `i64` a `proc_wait`/`proc_kill`
/// success returns. On every platform a normal exit yields its exit code. On Unix
/// a process killed by a signal has no exit code; by convention that is reported
/// as `128 + signal` (the shell convention), so callers still get a total,
/// deterministic `i64`. Shared by both interpreters so the value is identical
/// across backends.
pub fn process_exit_code(status: &std::process::ExitStatus) -> i64 {
    if let Some(code) = status.code() {
        return i64::from(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + i64::from(signal);
        }
    }
    // No exit code and (on non-Unix) no signal information available.
    -1
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

/// Register every `ExprKind::Closure` reachable from a block of statements into
/// `table`, keyed by the closure's parse-order `id`. The body borrows the program
/// with the lifetime `'a`; the parameter names are cloned. Nested closures (a
/// closure whose body is itself a closure) are collected recursively.
fn collect_closures_in_block<'a>(
    body: &'a [Stmt],
    table: &mut HashMap<usize, (Vec<String>, &'a Expr)>,
) {
    for stmt in body {
        collect_closures_in_stmt(stmt, table);
    }
}

fn collect_closures_in_stmt<'a>(
    stmt: &'a Stmt,
    table: &mut HashMap<usize, (Vec<String>, &'a Expr)>,
) {
    match stmt {
        Stmt::Let { value, .. } | Stmt::Assign { value, .. } | Stmt::Throw { value, .. } => {
            collect_closures_in_expr(value, table);
        }
        Stmt::Return(Some(expr)) | Stmt::Expr(expr) => collect_closures_in_expr(expr, table),
        Stmt::Return(None)
        | Stmt::Break(_)
        | Stmt::Continue(_)
        | Stmt::Asm { .. }
        | Stmt::Region(_) => {}
        Stmt::If {
            branches,
            else_body,
            ..
        } => {
            for branch in branches {
                collect_closures_in_expr(&branch.condition, table);
                collect_closures_in_block(&branch.body, table);
            }
            collect_closures_in_block(else_body, table);
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_closures_in_expr(condition, table);
            collect_closures_in_block(body, table);
        }
        Stmt::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            collect_closures_in_expr(start, table);
            collect_closures_in_expr(end, table);
            if let Some(step) = step {
                collect_closures_in_expr(step, table);
            }
            collect_closures_in_block(body, table);
        }
        Stmt::Loop { body, .. } | Stmt::Unsafe { body, .. } => {
            collect_closures_in_block(body, table);
        }
        Stmt::Try {
            body, catch_body, ..
        } => {
            collect_closures_in_block(body, table);
            collect_closures_in_block(catch_body, table);
        }
    }
}

fn collect_closures_in_expr<'a>(
    expr: &'a Expr,
    table: &mut HashMap<usize, (Vec<String>, &'a Expr)>,
) {
    match &expr.kind {
        ExprKind::Closure { id, params, body } => {
            let names = params.iter().map(|param| param.name.clone()).collect();
            table.insert(*id, (names, body.as_ref()));
            // A closure body may itself contain further closures.
            collect_closures_in_expr(body, table);
        }
        ExprKind::Integer(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::String(_)
        | ExprKind::Char(_)
        | ExprKind::Variable(_) => {}
        ExprKind::Array(items) => {
            for item in items {
                collect_closures_in_expr(item, table);
            }
        }
        ExprKind::Index { target, index } => {
            collect_closures_in_expr(target, table);
            collect_closures_in_expr(index, table);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Await { expr } => {
            collect_closures_in_expr(expr, table);
        }
        ExprKind::Binary { left, right, .. } => {
            collect_closures_in_expr(left, table);
            collect_closures_in_expr(right, table);
        }
        ExprKind::Call { args, .. } => {
            for arg in args {
                collect_closures_in_expr(arg, table);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                collect_closures_in_expr(value, table);
            }
        }
        ExprKind::Field { target, .. } => collect_closures_in_expr(target, table),
        ExprKind::Match { scrutinee, arms } => {
            collect_closures_in_expr(scrutinee, table);
            for arm in arms {
                collect_closures_in_block(&arm.body, table);
            }
        }
        ExprKind::Try(inner) => collect_closures_in_expr(inner, table),
    }
}

/// The function value `parallel_map` runs on each worker thread: either a named
/// top-level function or a self-contained capturing closure. Both are `Send`
/// (`String` / `Closure`), so they cross the scoped-thread boundary safely.
#[derive(Debug, Clone)]
enum ParallelCallable {
    Func(String),
    Closure(Closure),
}

/// One entry in the interpreter's active call stack. The function name is
/// *borrowed* from the program (`&'a str`), so pushing a frame on every call is
/// allocation-free; the owned [`TraceFrame`]s a `RuntimeError` carries are
/// materialized only when a traceback is actually attached on the error path.
struct CallFrame<'a> {
    function: &'a str,
    span: Option<Span>,
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
    /// Per-runtime table of live external processes. A `Value::Process(i)` indexes
    /// this vector; a killed/reaped process keeps its slot but `child.stdout`/
    /// `stderr` are drained on read. Mirrors `sockets`.
    processes: Vec<Option<ProcessResource>>,
    call_stack: Vec<CallFrame<'a>>,
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
    /// The failure value carried by an in-flight postfix `?` early return. When
    /// `EXPR?` hits `none`/`err` it stashes the whole enum value here and raises
    /// the `L0430` sentinel; `invoke_function` (the call boundary) takes this
    /// value and returns it as the enclosing function's result. The unwind is
    /// synchronous — nothing else runs between the raise and the catch — so a
    /// single slot is sufficient and never observed as stale.
    pending_try_return: Option<Value>,
    /// The closure-body table: `closure id -> (parameter names, body expression)`.
    /// Built once at construction by walking every function/impl-method body for
    /// `ExprKind::Closure` nodes. A `Value::Closure` carries only its `id`, so an
    /// invocation looks its body up here — the runtime value stays backend-neutral
    /// and stores no AST node. Bodies borrow the program with lifetime `'a`.
    closures: HashMap<usize, (Vec<String>, &'a Expr)>,
    /// A free-list of reusable per-call environments. Function invocation is on the
    /// hot path and each call needs a fresh `Env`; rather than allocate one (its
    /// scope `Vec` plus a first-scope `Vec` that grows as parameters bind) on every
    /// call, callees borrow a reset `Env` from here and return it on a normal exit,
    /// so a deep or repeated call reuses buffers instead of reallocating. Envs are
    /// only returned on the success path; error/`?`-unwind paths simply drop theirs
    /// (correctness is unaffected — a smaller pool just means a few more allocs).
    env_pool: Vec<Env>,
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
        // Compiler-provided `MemoryOrder` enum: its five unit variants construct
        // the ordering values consumed by the ordering-taking atomic builtins and
        // `fence`. Registered like `option`/`result` so bare `acquire`/`seq_cst`/…
        // build `Value::Enum` through the shared unit-variant path.
        for variant in MEMORY_ORDER_VARIANTS {
            variants.insert(variant, "MemoryOrder");
        }
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

        // Build the closure-body table by walking every function and impl-method
        // body for `ExprKind::Closure` nodes. Each node's parse-order `id` keys a
        // `(param names, body)` entry; the runtime `Value::Closure` carries only
        // the id, so this is where the id is resolved back to a body to evaluate.
        let mut closures = HashMap::new();
        for function in &program.functions {
            collect_closures_in_block(&function.body, &mut closures);
        }
        for decl in &program.impls {
            for method in &decl.methods {
                collect_closures_in_block(&method.body, &mut closures);
            }
        }

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
            processes: Vec::new(),
            call_stack: Vec::new(),
            impl_methods,
            trait_method_names,
            async_functions,
            extern_functions,
            pending_try_return: None,
            closures,
            env_pool: Vec::new(),
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

    /// True when a call to `name` is a plain builtin (or infallible enum/struct
    /// constructor) rather than anything that could raise a *catchable* `L0420`
    /// user error. This is the safety gate for the move-on-functional-update fast
    /// path: builtins in the accumulation idiom (`push`, `concat`, `map_set`,
    /// `sort`, `replace`, `set`, …) only ever fail with non-catchable errors that
    /// halt the program, so moving the consumed argument out can never leave a
    /// moved-out placeholder observable by a surrounding `catch`. Excluded, because
    /// they can run user code (which may `throw`) or dispatch by value:
    /// closure/func-valued variables, `extern`/`async` functions, trait methods,
    /// user-defined functions, and `assert` (the one builtin that raises `L0420`).
    fn is_move_safe_builtin(&self, name: &str, env: &Env) -> bool {
        if matches!(
            env.get_ref(name),
            Some(Value::Closure(_)) | Some(Value::Func(_))
        ) {
            return false;
        }
        name != "assert"
            && !self.extern_functions.contains(name)
            && !self.async_functions.contains(name)
            && !self.trait_method_names.contains(name)
            && !self.functions.contains_key(name)
    }

    /// The move-on-functional-update fast path for the pervasive `x = f(x, …)`
    /// (CALL) and `x = x <binop> e` / `x = e <binop> x` (BINARY) accumulation
    /// idioms. When the assignment target `name` appears **exactly once** — as a
    /// bare call argument, or as exactly one bare operand of a binary op — and
    /// nowhere else in the RHS, and `name` is a local, this evaluates the RHS
    /// with that one occurrence **moved** out of the environment instead of
    /// cloned, and returns `Some(result)`. Returning `None` means the pattern did
    /// not apply and the caller must fall back to the ordinary clone path.
    ///
    /// The binary form is what makes `s = s + piece` in a loop O(n): the moved
    /// left operand's heap buffer is reused by `eval_binary`'s string concat
    /// (see [`Value::into_string`]) instead of being cloned on read.
    ///
    /// Safety: moving is observably identical to cloning here because `name` is
    /// (a) consumed exactly once, (b) not read anywhere else in the statement, and
    /// (c) immediately overwritten with the result. The *other* operand/arguments
    /// are evaluated *before* the move, so a failure while evaluating them leaves
    /// `name` intact. The consuming op cannot raise a *catchable* error: builtins
    /// on the call path are gated to those that never raise `L0420`, and
    /// `eval_binary` only ever raises non-catchable errors (e.g. `L0404`
    /// div-by-zero, `L0417` type) — only user-thrown `L0420` is recoverable by a
    /// `catch` — so a mid-op failure halts the program with the moved-out
    /// placeholder never observed. Short-circuit `and`/`or` are excluded: they do
    /// not route through `eval_binary` and evaluating the non-target operand early
    /// would change their conditional-evaluation semantics.
    fn try_move_functional_update(
        &mut self,
        name: &str,
        rhs: &Expr,
        env: &mut Env,
        require_innermost: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        match &rhs.kind {
            ExprKind::Call { name: callee, args } => {
                self.try_move_call_update(name, callee, args, env, require_innermost)
            }
            ExprKind::Binary { op, left, right } => {
                self.try_move_binary_update(name, *op, left, right, env, require_innermost)
            }
            _ => Ok(None),
        }
    }

    /// `x = f(x, …)` arm of [`Self::try_move_functional_update`].
    fn try_move_call_update(
        &mut self,
        name: &str,
        callee: &str,
        args: &[Expr],
        env: &mut Env,
        require_innermost: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        // Never optimize when the call target is the variable itself (`x = x(x)`):
        // moving `x` out would change how the call name resolves.
        if callee == name {
            return Ok(None);
        }
        if !self.is_move_safe_builtin(callee, env) {
            return Ok(None);
        }
        // The consumed binding must be a local. For a `let` re-binding it must be
        // the innermost binding (because `let` shadows into the innermost scope);
        // for a plain reassignment any-scope is fine (moved from, and written back
        // to, the nearest binding). Checked before locating the argument so the
        // common non-matching cases stay cheap.
        let bound = if require_innermost {
            env.innermost_has(name)
        } else {
            env.is_bound(name)
        };
        if !bound {
            return Ok(None);
        }
        // Locate the single bare `Variable(name)` argument and prove `name` does
        // not appear anywhere else in the argument list.
        let mut target_idx: Option<usize> = None;
        for (i, arg) in args.iter().enumerate() {
            let is_bare = matches!(&arg.kind, ExprKind::Variable(v) if v == name);
            if is_bare && target_idx.is_none() {
                target_idx = Some(i);
            } else if expr_mentions_var(arg, name) {
                return Ok(None);
            }
        }
        let Some(target_idx) = target_idx else {
            return Ok(None);
        };
        // Evaluate every *other* argument first, in source order. If one fails
        // (e.g. a nested `throw`), `name` is still intact and the env consistent.
        let mut evaluated: Vec<Option<Value>> = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            if i == target_idx {
                evaluated.push(None);
            } else {
                evaluated.push(Some(self.eval_expr(arg, env)?));
            }
        }
        // All other arguments succeeded: move the target's value out (no clone),
        // leaving a placeholder in its slot for the caller's write-back.
        let moved = env
            .move_out_nearest(name)
            .expect("target verified bound as a local");
        let mut moved = Some(moved);
        let values: Vec<Value> = evaluated
            .into_iter()
            .enumerate()
            .map(|(i, slot)| {
                if i == target_idx {
                    moved.take().expect("single target slot")
                } else {
                    slot.expect("non-target slots are evaluated")
                }
            })
            .collect();
        // `callee` is a plain builtin/constructor here (closures/func values/
        // extern/async/user functions were excluded), so dispatch it directly.
        Ok(Some(self.call_function(callee, values)?))
    }

    /// `x = x <binop> e` / `x = e <binop> x` arm of
    /// [`Self::try_move_functional_update`]. Fires when exactly one operand is the
    /// bare variable `name` and `name` appears nowhere else in either operand.
    fn try_move_binary_update(
        &mut self,
        name: &str,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        env: &mut Env,
        require_innermost: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        // Short-circuit operators are evaluated in `eval_expr`, not `eval_binary`,
        // and their right operand is conditional; reordering evaluation would
        // change semantics, so never optimize them.
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            return Ok(None);
        }
        let bound = if require_innermost {
            env.innermost_has(name)
        } else {
            env.is_bound(name)
        };
        if !bound {
            return Ok(None);
        }
        // Exactly one operand must be the bare `Variable(name)`, and `name` must
        // not appear anywhere in the *other* operand. `s = s + s`, `s = pre + s +
        // suf`, `n = a - n + n`, etc. therefore fall back to the clone path.
        let left_bare = matches!(&left.kind, ExprKind::Variable(v) if v == name);
        let right_bare = matches!(&right.kind, ExprKind::Variable(v) if v == name);
        let target_is_left = if left_bare && !expr_mentions_var(right, name) {
            true
        } else if right_bare && !expr_mentions_var(left, name) {
            false
        } else {
            return Ok(None);
        };
        // Evaluate the non-target operand *before* moving the target, so a failure
        // there leaves `name` intact and the env consistent.
        let other = if target_is_left {
            self.eval_expr(right, env)?
        } else {
            self.eval_expr(left, env)?
        };
        let moved = env
            .move_out_nearest(name)
            .expect("target verified bound as a local");
        let (l, r) = if target_is_left {
            (moved, other)
        } else {
            (other, moved)
        };
        Ok(Some(self.eval_binary(l, op, r)?))
    }

    /// Dispatch a call to an already-resolved top-level function name: reject an
    /// `extern fn` (C-ABI, native-only) with `L0423`, spawn an `async fn` on its
    /// own OS thread yielding a `Future`, or invoke the function / builtin /
    /// constructor synchronously through [`Self::call_function`].
    fn dispatch_named_call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        if self.extern_functions.contains(name) {
            return Err(extern_call_error(name));
        }
        if self.async_functions.contains(name) {
            Ok(self.spawn_async(name, args))
        } else {
            self.call_function(name, args)
        }
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
            return Ok(Value::Enum(Box::new(EnumValue {
                enum_name: enum_name.to_string(),
                variant: name.to_string(),
                payload: args,
            })));
        }
        if let Some(field_names) = self.structs.get(name) {
            return Ok(Value::Struct(Box::new(StructValue {
                name: name.to_string(),
                fields: field_names.iter().cloned().zip(args).collect(),
            })));
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
            "is_digit" => Self::builtin_is_digit(args),
            "is_alpha" => Self::builtin_is_alpha(args),
            "is_alnum" => Self::builtin_is_alnum(args),
            "is_whitespace" => Self::builtin_is_whitespace(args),
            "is_upper" => Self::builtin_is_upper(args),
            "is_lower" => Self::builtin_is_lower(args),
            "byte" => Self::builtin_byte(args),
            "byte_val" => Self::builtin_byte_val(args),
            "to_i8" => Self::builtin_to_int("to_i8", args, IntKind::I8),
            "to_u8" => Self::builtin_to_int("to_u8", args, IntKind::U8),
            "to_i16" => Self::builtin_to_int("to_i16", args, IntKind::I16),
            "to_i32" => Self::builtin_to_int("to_i32", args, IntKind::I32),
            "to_u16" => Self::builtin_to_int("to_u16", args, IntKind::U16),
            "to_u32" => Self::builtin_to_int("to_u32", args, IntKind::U32),
            "to_u64" => Self::builtin_to_int("to_u64", args, IntKind::U64),
            "to_isize" => Self::builtin_to_int("to_isize", args, IntKind::Isize),
            "to_usize" => Self::builtin_to_int("to_usize", args, IntKind::Usize),
            "to_i64" => Self::builtin_to_i64(args),
            "to_f32" => Self::builtin_to_f32(args),
            "to_f64" => Self::builtin_to_f64(args),
            "checked_add" => overflow_arith(name, args, ArithOp::Add, OverflowMode::Checked),
            "checked_sub" => overflow_arith(name, args, ArithOp::Sub, OverflowMode::Checked),
            "checked_mul" => overflow_arith(name, args, ArithOp::Mul, OverflowMode::Checked),
            "saturating_add" => overflow_arith(name, args, ArithOp::Add, OverflowMode::Saturating),
            "saturating_sub" => overflow_arith(name, args, ArithOp::Sub, OverflowMode::Saturating),
            "saturating_mul" => overflow_arith(name, args, ArithOp::Mul, OverflowMode::Saturating),
            "wrapping_add" => overflow_arith(name, args, ArithOp::Add, OverflowMode::Wrapping),
            "wrapping_sub" => overflow_arith(name, args, ArithOp::Sub, OverflowMode::Wrapping),
            "wrapping_mul" => overflow_arith(name, args, ArithOp::Mul, OverflowMode::Wrapping),
            "len" => Self::builtin_len(args),
            "list_new" => Self::builtin_list_new(args),
            "push" => Self::builtin_push(args),
            "get" => Self::builtin_get(args),
            "set" => Self::builtin_set(args),
            "pop" => Self::builtin_pop(args),
            "list_index_of" => Self::builtin_list_index_of(args),
            "list_contains" => Self::builtin_list_contains(args),
            "reverse" => Self::builtin_reverse(args),
            "sort" => Self::builtin_sort(args),
            "sort_by" => self.builtin_sort_by(args),
            "concat" => Self::builtin_concat(args),
            "slice" => Self::builtin_slice(args),
            "list_map" => self.builtin_list_map(args),
            "list_filter" => self.builtin_list_filter(args),
            "list_reduce" => self.builtin_list_reduce(args),
            "map_new" => Self::builtin_map_new(args),
            "map_set" => Self::builtin_map_set(args),
            "map_get" => Self::builtin_map_get(args),
            "map_has" => Self::builtin_map_has(args),
            "map_len" => Self::builtin_map_len(args),
            "map_keys" => Self::builtin_map_keys(args),
            "map_values" => Self::builtin_map_values(args),
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
            "chars" => Self::builtin_chars(args),
            "string_from_chars" => Self::builtin_string_from_chars(args),
            "lower" => Self::builtin_lower(args),
            "to_bytes" => Self::builtin_to_bytes(args),
            "from_bytes" => Self::builtin_from_bytes(args),
            "byte_len" => Self::builtin_byte_len(args),
            "parse_i64" => Self::builtin_parse_i64(args),
            "parse_f64" => Self::builtin_parse_f64(args),
            "abs" => Self::builtin_abs(args),
            "min" => Self::builtin_min(args),
            "max" => Self::builtin_max(args),
            "clamp" => Self::builtin_clamp(args),
            "sign" => Self::builtin_sign(args),
            "gcd" => Self::builtin_gcd(args),
            "list_sum" => Self::builtin_list_sum(args),
            "list_min" => Self::builtin_list_min(args),
            "list_max" => Self::builtin_list_max(args),
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
            "size_of" => Self::builtin_size_of(args),
            "align_of" => Self::builtin_align_of(args),
            "offset_of" => Self::builtin_offset_of(args),
            "ptr_to_int" => Self::builtin_ptr_to_int(args),
            "int_to_ptr" => Self::builtin_int_to_ptr(args),
            // Volatile raw-memory access behaves exactly like `load`/`store` on
            // the interpreters' single-threaded abstract heap; the no-elision /
            // no-reordering guarantee is a native-codegen concern.
            "volatile_load" => self.builtin_load(args),
            "volatile_store" => self.builtin_store(args),
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
            "atomic_new" => Self::builtin_atomic_new(args),
            "atomic_load" => Self::builtin_atomic_load(args),
            "atomic_store" => Self::builtin_atomic_store(args),
            "atomic_swap" => Self::builtin_atomic_swap(args),
            "atomic_cas" => Self::builtin_atomic_cas(args),
            "atomic_add" => Self::builtin_atomic_add(args),
            "atomic_sub" => Self::builtin_atomic_sub(args),
            "atomic_and" => Self::builtin_atomic_and(args),
            "atomic_or" => Self::builtin_atomic_or(args),
            "atomic_xor" => Self::builtin_atomic_xor(args),
            "atomic_load_ordered" => builtin_atomic_load_ordered(args),
            "atomic_store_ordered" => builtin_atomic_store_ordered(args),
            "atomic_swap_ordered" => builtin_atomic_swap_ordered(args),
            "atomic_cas_ordered" => builtin_atomic_cas_ordered(args),
            "atomic_add_ordered" => builtin_atomic_add_ordered(args),
            "atomic_sub_ordered" => builtin_atomic_sub_ordered(args),
            "atomic_and_ordered" => builtin_atomic_and_ordered(args),
            "atomic_or_ordered" => builtin_atomic_or_ordered(args),
            "atomic_xor_ordered" => builtin_atomic_xor_ordered(args),
            "fence" => builtin_fence(args),
            "tcp_connect" => self.builtin_tcp_connect(args),
            "tcp_listen" => self.builtin_tcp_listen(args),
            "tcp_accept" => self.builtin_tcp_accept(args),
            "tcp_accept_nb" => self.builtin_tcp_accept_nb(args),
            "tcp_read" => self.builtin_tcp_read(args),
            "tcp_read_nb" => self.builtin_tcp_read_nb(args),
            "tcp_write" => self.builtin_tcp_write(args),
            "tcp_shutdown" => self.builtin_tcp_shutdown(args),
            "tcp_close" => self.builtin_socket_close(args),
            "set_nonblocking" => self.builtin_set_nonblocking(args),
            "udp_bind" => self.builtin_udp_bind(args),
            "udp_send_to" => self.builtin_udp_send_to(args),
            "udp_recv" => self.builtin_udp_recv(args),
            "udp_recv_nb" => self.builtin_udp_recv_nb(args),
            "http_get" => Self::builtin_http_get(args),
            "http_post" => Self::builtin_http_post(args),
            "proc_spawn" => self.builtin_proc_spawn(args),
            "proc_wait" => self.builtin_proc_wait(args),
            "proc_stdout" => self.builtin_proc_stdout(args),
            "proc_stderr" => self.builtin_proc_stderr(args),
            "proc_kill" => self.builtin_proc_kill(args),
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
        // `parallel_map` accepts either a named function value or a capturing
        // closure. A closure is self-contained (it carries its captured snapshot,
        // all `Send`) and the worker's fresh interpreter rebuilds the same
        // id-keyed body table from the shared program, so invoking it there is
        // sound and stays order-deterministic.
        let callable = match callee {
            Value::Func(name) => ParallelCallable::Func(name),
            Value::Closure(closure) => ParallelCallable::Closure(closure),
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
        let callable = &callable;
        let results: Vec<Value> = std::thread::scope(|scope| {
            let handles: Vec<_> = arg_values
                .iter()
                .map(|value| {
                    let callable = callable.clone();
                    let value = value.clone();
                    let arc = Arc::clone(program_arc);
                    scope.spawn(move || {
                        let mut runtime = Runtime::new(program, arc)?;
                        match callable {
                            ParallelCallable::Func(name) => {
                                runtime.call_function(&name, vec![value])
                            }
                            ParallelCallable::Closure(closure) => {
                                runtime.invoke_closure(&closure, vec![value])
                            }
                        }
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

    /// `atomic_new(v i64) -> atomic_i64`: allocate a fresh shared atomic cell
    /// initialized to `v`. Cloning the returned handle shares the same
    /// `Arc<AtomicI64>`, so several threads observe each other's updates.
    fn builtin_atomic_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_new", 1, args.len()))?;
        let value = expect_i64("atomic_new", value)?;
        Ok(Value::Atomic(SharedAtomic {
            cell: Arc::new(AtomicI64::new(value)),
        }))
    }

    /// `atomic_load(a atomic_i64) -> i64`: read the cell (SeqCst).
    fn builtin_atomic_load(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_load", 1, args.len()))?;
        let atomic = expect_atomic("atomic_load", atomic)?;
        Ok(Value::I64(atomic.cell.load(Ordering::SeqCst)))
    }

    /// `atomic_store(a atomic_i64, v i64) -> void`: write the cell (SeqCst).
    fn builtin_atomic_store(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_store", 2, args.len()))?;
        let atomic = expect_atomic("atomic_store", atomic)?;
        let value = expect_i64("atomic_store", value)?;
        atomic.cell.store(value, Ordering::SeqCst);
        Ok(Value::Void)
    }

    /// `atomic_swap(a atomic_i64, v i64) -> i64`: store `v`, return the previous
    /// value (SeqCst).
    fn builtin_atomic_swap(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_swap", 2, args.len()))?;
        let atomic = expect_atomic("atomic_swap", atomic)?;
        let value = expect_i64("atomic_swap", value)?;
        Ok(Value::I64(atomic.cell.swap(value, Ordering::SeqCst)))
    }

    /// `atomic_cas(a atomic_i64, expected i64, new i64) -> i64`: strong
    /// compare-and-swap. Returns the value that was in the cell (equal to
    /// `expected` on success), matching C11's value-returning shape. SeqCst on
    /// both success and failure.
    fn builtin_atomic_cas(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic, expected, new]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_cas", 3, args.len()))?;
        let atomic = expect_atomic("atomic_cas", atomic)?;
        let expected = expect_i64("atomic_cas", expected)?;
        let new = expect_i64("atomic_cas", new)?;
        // `compare_exchange` returns `Ok(prev)` on success and `Err(current)` on
        // failure; both payloads carry the value that was observed in the cell.
        let observed =
            match atomic
                .cell
                .compare_exchange(expected, new, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(prev) => prev,
                Err(current) => current,
            };
        Ok(Value::I64(observed))
    }

    /// `atomic_add(a atomic_i64, v i64) -> i64`: fetch-and-add, returning the
    /// PREVIOUS value (SeqCst). Wrapping arithmetic, as `fetch_add` defines.
    fn builtin_atomic_add(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_add", args)?;
        Ok(Value::I64(atomic.cell.fetch_add(value, Ordering::SeqCst)))
    }

    /// `atomic_sub(a atomic_i64, v i64) -> i64`: fetch-and-sub, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_sub(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_sub", args)?;
        Ok(Value::I64(atomic.cell.fetch_sub(value, Ordering::SeqCst)))
    }

    /// `atomic_and(a atomic_i64, v i64) -> i64`: fetch-and-and, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_and(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_and", args)?;
        Ok(Value::I64(atomic.cell.fetch_and(value, Ordering::SeqCst)))
    }

    /// `atomic_or(a atomic_i64, v i64) -> i64`: fetch-and-or, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_or(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_or", args)?;
        Ok(Value::I64(atomic.cell.fetch_or(value, Ordering::SeqCst)))
    }

    /// `atomic_xor(a atomic_i64, v i64) -> i64`: fetch-and-xor, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_xor(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_xor", args)?;
        Ok(Value::I64(atomic.cell.fetch_xor(value, Ordering::SeqCst)))
    }

    /// Shared argument-decoding for the `atomic_<op>(a atomic_i64, v i64)`
    /// fetch-and-op family: exactly two arguments, an atomic handle then an
    /// `i64` operand.
    fn atomic_binary_args(
        name: &str,
        args: Vec<Value>,
    ) -> Result<(SharedAtomic, i64), RuntimeError> {
        let [atomic, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 2, args.len()))?;
        let atomic = expect_atomic(name, atomic)?;
        let value = expect_i64(name, value)?;
        Ok((atomic, value))
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

    /// Push a freshly spawned child into the handle table, returning its index
    /// wrapped as a `Value::Process`. Mirrors `register_socket`.
    fn register_process(&mut self, resource: ProcessResource) -> Value {
        self.processes.push(Some(resource));
        Value::Process(self.processes.len() - 1)
    }

    /// Resolve a process handle argument to its live slot index, reporting a
    /// wrong-argument-type error for a non-process value and a stale-handle error
    /// for a reaped/invalid slot. Mirrors `socket_slot`.
    fn process_slot(&self, name: &str, value: &Value) -> Result<usize, RuntimeError> {
        let Value::Process(handle) = value else {
            return Err(RuntimeError::new(
                "L0417",
                format!("{name} expects a process but got `{value}`"),
            ));
        };
        match self.processes.get(*handle) {
            Some(Some(_)) => Ok(*handle),
            _ => Err(RuntimeError::new(
                "L0406",
                format!("{name} received a reaped or invalid process `{handle}`"),
            )),
        }
    }

    /// `proc_spawn(cmd string, args array<string>) -> result<process, string>`:
    /// spawn `cmd` with `args`, piping stdout/stderr so they can be read later.
    /// `ok(handle)` on success, `err(message)` if the process cannot be started
    /// (e.g. the command is not found). Never panics.
    fn builtin_proc_spawn(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [cmd, cmd_args]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_spawn", 2, args.len()))?;
        let cmd = expect_string("proc_spawn", cmd)?;
        let cmd_args = cmd_args.as_string_array()?;
        match Command::new(&cmd)
            .args(cmd_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => {
                let handle = self.register_process(ProcessResource { child });
                Ok(result_value(Ok(handle)))
            }
            Err(error) => Ok(result_value(Err(Value::String(error.to_string())))),
        }
    }

    /// `proc_wait(p process) -> result<i64, string>`: block until the child exits
    /// and return its exit code (`128 + signal` on Unix signal termination).
    /// `err` if the handle is already reaped/invalid or the wait fails.
    fn builtin_proc_wait(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [proc]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_wait", 1, args.len()))?;
        let slot = self.process_slot("proc_wait", &proc)?;
        let Some(resource) = self.processes[slot].as_mut() else {
            return Ok(result_value(Err(Value::String(
                "proc_wait requires a live process".to_string(),
            ))));
        };
        match resource.child.wait() {
            Ok(status) => Ok(result_value(Ok(Value::I64(process_exit_code(&status))))),
            Err(error) => Ok(result_value(Err(Value::String(error.to_string())))),
        }
    }

    /// `proc_stdout(p process) -> result<string, string>`: read the child's
    /// captured stdout to end as a lossy UTF-8 string. The pipe is taken out of
    /// the `Child` on first read, so a second call returns an empty string.
    fn builtin_proc_stdout(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        self.proc_read_pipe("proc_stdout", args, PipeKind::Stdout)
    }

    /// `proc_stderr(p process) -> result<string, string>`: like `proc_stdout` but
    /// for the child's captured stderr.
    fn builtin_proc_stderr(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        self.proc_read_pipe("proc_stderr", args, PipeKind::Stderr)
    }

    /// Shared body of `proc_stdout`/`proc_stderr`: take the requested pipe out of
    /// the child and read it to end.
    fn proc_read_pipe(
        &mut self,
        name: &'static str,
        args: Vec<Value>,
        kind: PipeKind,
    ) -> Result<Value, RuntimeError> {
        use std::io::Read;
        let [proc]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        let slot = self.process_slot(name, &proc)?;
        let Some(resource) = self.processes[slot].as_mut() else {
            return Ok(result_value(Err(Value::String(format!(
                "{name} requires a live process"
            )))));
        };
        let mut buffer = String::new();
        let read = match kind {
            PipeKind::Stdout => resource
                .child
                .stdout
                .take()
                .map(|mut pipe| pipe.read_to_string(&mut buffer)),
            PipeKind::Stderr => resource
                .child
                .stderr
                .take()
                .map(|mut pipe| pipe.read_to_string(&mut buffer)),
        };
        match read {
            // Pipe already drained (or was never captured): report EOF.
            None => Ok(result_value(Ok(Value::String(String::new())))),
            Some(Ok(_)) => Ok(result_value(Ok(Value::String(buffer)))),
            Some(Err(error)) => Ok(result_value(Err(Value::String(error.to_string())))),
        }
    }

    /// `proc_kill(p process) -> result<i64, string>`: kill the child, returning
    /// `ok(0)` on success. Killing an already-exited child still succeeds.
    fn builtin_proc_kill(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [proc]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_kill", 1, args.len()))?;
        let slot = self.process_slot("proc_kill", &proc)?;
        let Some(resource) = self.processes[slot].as_mut() else {
            return Ok(result_value(Err(Value::String(
                "proc_kill requires a live process".to_string(),
            ))));
        };
        match resource.child.kill() {
            Ok(()) => Ok(result_value(Ok(Value::I64(0)))),
            Err(error) => Ok(result_value(Err(Value::String(error.to_string())))),
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

    /// `tcp_accept_nb(listener Socket) -> result<option<Socket>, string>`:
    /// non-blocking accept. Returns `ok(some(client))` when a connection is
    /// pending, `ok(none)` when the listener would block (no pending connection),
    /// and `err(message)` on a real error. The listener must first be put into
    /// non-blocking mode with `set_nonblocking`.
    fn builtin_tcp_accept_nb(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [listener]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_accept_nb", 1, args.len()))?;
        let slot = self.socket_slot("tcp_accept_nb", &listener)?;
        let accepted = match &self.sockets[slot] {
            Some(SocketResource::Listener(listener)) => listener.accept(),
            _ => {
                return Ok(result_value(Err(Value::String(
                    "tcp_accept_nb requires a listening socket".to_string(),
                ))));
            }
        };
        match accepted {
            Ok((stream, _addr)) => {
                let socket = self.register_socket(SocketResource::Stream(stream));
                Ok(result_value(Ok(option_value(Some(socket)))))
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(result_value(Ok(option_value(None))))
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

    /// `tcp_read_nb(conn Socket, max i64) -> result<option<string>, string>`:
    /// non-blocking read of up to `max` bytes, returned as a lossy UTF-8 string.
    /// Returns `ok(some(data))` when bytes are available, `ok(some(""))` on a
    /// clean EOF (the peer closed the connection — matching blocking `tcp_read`),
    /// `ok(none)` when the stream would block (no data ready yet), and
    /// `err(message)` on a real error. `max` must be positive. The stream must
    /// first be put into non-blocking mode with `set_nonblocking`.
    fn builtin_tcp_read_nb(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Read;
        let [conn, max]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_read_nb", 2, args.len()))?;
        let slot = self.socket_slot("tcp_read_nb", &conn)?;
        let max = expect_i64("tcp_read_nb", max)?;
        if max <= 0 {
            return Ok(result_value(Err(Value::String(
                "tcp_read_nb requires a positive `max` byte count".to_string(),
            ))));
        }
        let mut buffer = vec![0u8; max as usize];
        let read = match &mut self.sockets[slot] {
            Some(SocketResource::Stream(stream)) => stream.read(&mut buffer),
            _ => {
                return Ok(result_value(Err(Value::String(
                    "tcp_read_nb requires a connected stream socket".to_string(),
                ))));
            }
        };
        match read {
            Ok(count) => Ok(result_value(Ok(option_value(Some(Value::String(
                String::from_utf8_lossy(&buffer[..count]).into_owned(),
            )))))),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(result_value(Ok(option_value(None))))
            }
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

    /// `set_nonblocking(sock Socket, enabled bool) -> result<i64, string>`: put a
    /// socket (a listener, connected stream, or UDP socket) into or out of
    /// non-blocking mode via std's `set_nonblocking`. In non-blocking mode,
    /// accept/read/recv operations that would block instead surface as
    /// `ErrorKind::WouldBlock`, which the `*_nb` builtins report as `ok(none)`.
    /// Returns `ok(0)` on success or `err(message)` on failure.
    fn builtin_set_nonblocking(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [sock, enabled]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("set_nonblocking", 2, args.len()))?;
        let slot = self.socket_slot("set_nonblocking", &sock)?;
        let enabled = expect_bool("set_nonblocking", enabled)?;
        let outcome = match &self.sockets[slot] {
            Some(SocketResource::Listener(listener)) => listener.set_nonblocking(enabled),
            Some(SocketResource::Stream(stream)) => stream.set_nonblocking(enabled),
            Some(SocketResource::Udp(socket)) => socket.set_nonblocking(enabled),
            None => {
                return Ok(result_value(Err(Value::String(
                    "set_nonblocking requires an open socket".to_string(),
                ))));
            }
        };
        match outcome {
            Ok(()) => Ok(result_value(Ok(Value::I64(0)))),
            Err(error) => Ok(net_err(&error)),
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

    /// `udp_recv_nb(sock Socket) -> result<option<string>, string>`: non-blocking
    /// receive of one datagram (sender address dropped), returned as a lossy
    /// UTF-8 string. Returns `ok(some(data))` when a datagram is ready,
    /// `ok(none)` when the socket would block (no datagram pending), and
    /// `err(message)` on a real error. The socket must first be put into
    /// non-blocking mode with `set_nonblocking`.
    fn builtin_udp_recv_nb(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [sock]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("udp_recv_nb", 1, args.len()))?;
        let slot = self.socket_slot("udp_recv_nb", &sock)?;
        let mut buffer = [0u8; 4096];
        let received = match &self.sockets[slot] {
            Some(SocketResource::Udp(socket)) => socket.recv_from(&mut buffer),
            _ => {
                return Ok(result_value(Err(Value::String(
                    "udp_recv_nb requires a UDP socket".to_string(),
                ))));
            }
        };
        match received {
            Ok((count, _addr)) => Ok(result_value(Ok(option_value(Some(Value::String(
                String::from_utf8_lossy(&buffer[..count]).into_owned(),
            )))))),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(result_value(Ok(option_value(None))))
            }
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

        // Borrow a reset environment from the pool (or make a fresh one) instead
        // of allocating per call; it is returned to the pool on the normal exit
        // path below.
        let mut env = match self.env_pool.pop() {
            Some(mut env) => {
                env.reset();
                env
            }
            None => Env::default(),
        };
        for (param, value) in function.params.iter().zip(args) {
            env.define(param.name.clone(), value);
        }

        self.call_stack.push(CallFrame {
            function: function.name.as_str(),
            span: Some(function.span),
        });
        let result = self.eval_block(&function.body, &mut env);

        // Attach the traceback lazily. `with_traceback` records only the first
        // (innermost) stack — every later frame's attach is a no-op — so eagerly
        // cloning `call_stack` on every successful call, and on every frame an
        // error merely passes through, is pure waste. Clone it only when this
        // frame is the one first recording a traceback, while the frame is still
        // on the stack so it is included.
        //
        // A postfix `?` on a `none`/`err` unwinds to here as the `L0430`
        // sentinel, carrying the failure value in `pending_try_return`. Catch it
        // at this call boundary and turn it into a normal function return of that
        // value (this is the function-level early return `?` denotes). The slot
        // is always taken, so it never leaks into a later call.
        let control = match result {
            Err(error) if error.code == "L0430" => {
                self.call_stack.pop();
                let value = self.pending_try_return.take().ok_or_else(|| {
                    RuntimeError::new(
                        "L0430",
                        "missing `?` propagation value at function boundary",
                    )
                })?;
                return Ok(value);
            }
            Err(error) => {
                let error = if error.traceback.is_empty() {
                    error.with_traceback(self.build_traceback())
                } else {
                    error
                };
                self.call_stack.pop();
                return Err(error);
            }
            Ok(control) => {
                self.call_stack.pop();
                control
            }
        };
        // Normal exit: return the environment to the pool for the next call. The
        // early-return paths above simply drop theirs.
        self.env_pool.push(env);

        match control {
            Control::Return(value) | Control::Value(value) => Ok(value),
            Control::Break | Control::Continue => Err(RuntimeError::new(
                "L0410",
                "loop control escaped function body",
            )),
        }
    }

    /// Invoke a closure value: look its body up in the id-keyed closure table,
    /// push a fresh scope, bind the captured snapshot first and then the
    /// parameters (so parameters shadow captured names of the same identifier),
    /// evaluate the single-expression body, and return the produced value. The
    /// closure is self-contained (its captured values travel with it), so it runs
    /// against a fresh environment rather than the caller's.
    fn invoke_closure(
        &mut self,
        closure: &Closure,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        // Copy the body pointer and parameter names out of the table so the
        // borrow of `self.closures` ends before the `&mut self` evaluation.
        let (param_names, body) = match self.closures.get(&closure.id) {
            Some((names, body)) => (names.clone(), *body),
            None => {
                return Err(RuntimeError::new(
                    "L0402",
                    format!("closure #{} has no registered body", closure.id),
                ));
            }
        };
        if param_names.len() != args.len() {
            return Err(RuntimeError::new(
                "L0402",
                format!(
                    "closure expects {} arguments but got {}",
                    param_names.len(),
                    args.len()
                ),
            ));
        }

        let mut env = Env::default();
        // Captured bindings first, then parameters (parameters shadow captures).
        for (name, value) in &closure.captured {
            env.define(name.clone(), value.clone());
        }
        for (name, value) in param_names.iter().zip(args) {
            env.define(name.clone(), value);
        }
        self.eval_expr(body, &env)
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
                // Move-on-functional-update fast path: `let x = f(x, …)` re-binding
                // an existing innermost local consumes it by move, not clone.
                let value = match self.try_move_functional_update(name, value, env, true)? {
                    Some(result) => result,
                    None => self.eval_expr(value, env)?,
                };
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
                if path.is_empty() && matches!(op, AssignOp::Replace) {
                    // Whole-variable reassignment `x = RHS`: try the
                    // move-on-functional-update fast path (`x = f(x, …)`) before
                    // falling back to the ordinary clone path.
                    let new = match self.try_move_functional_update(name, value, env, false)? {
                        Some(result) => result,
                        None => self.eval_expr(value, env)?,
                    };
                    env.assign(name, new)?;
                } else {
                    let rhs = self.eval_expr(value, env)?;
                    if path.is_empty() {
                        let new = apply_compound(env.get(name)?, op, rhs)?;
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
                    env.define(catch_name.clone(), Value::String(error.message));
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
        let Value::Enum(e) = value else {
            return Err(RuntimeError::new(
                "L0383",
                "match scrutinee did not evaluate to an enum value",
            ));
        };
        let EnumValue {
            variant, payload, ..
        } = *e;
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
                    Value::Struct(s) => s
                        .fields
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
                        Ok(Value::Enum(Box::new(EnumValue {
                            enum_name: enum_name.to_string(),
                            variant: name.clone(),
                            payload: Vec::new(),
                        })))
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
                    // Bitwise NOT (one's complement). On a fixed-width integer the
                    // complement is re-normalized to the width.
                    UnaryOp::BitNot => match value {
                        Value::Int { value, ty } => Ok(Value::int(!value, ty)),
                        other => Ok(Value::I64(!other.as_i64()?)),
                    },
                    // Arithmetic negation, preserving the operand's numeric type.
                    // Integer negation wraps; float negation flips the sign bit.
                    UnaryOp::Negate => match value {
                        Value::Int { value, ty } => Ok(Value::int(value.wrapping_neg(), ty)),
                        Value::F64(f) => Ok(Value::F64(-f)),
                        Value::F32(f) => Ok(Value::F32(-f)),
                        other => Ok(Value::I64(other.as_i64()?.wrapping_neg())),
                    },
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
                // Resolve the call target with a single borrowing lookup. A call
                // name bound to a closure value invokes that closure (binding its
                // captured snapshot then the arguments); a name bound to a
                // function value dispatches through it; otherwise `name` is a plain
                // top-level function / builtin / constructor and dispatches by
                // name. Using `get_ref` keeps the common case — an ordinary
                // top-level call, where `name` is not a local at all — free of the
                // clone and the discarded "unknown variable" error a bare
                // `env.get` allocates on every such call.
                let target: &str = match env.get_ref(name) {
                    Some(Value::Closure(closure)) => {
                        let closure = closure.clone();
                        return self.invoke_closure(&closure, values);
                    }
                    Some(Value::Func(func)) => {
                        let func = func.clone();
                        return self.dispatch_named_call(&func, values);
                    }
                    _ => name,
                };
                self.dispatch_named_call(target, values)
            }
            ExprKind::Await { expr } => {
                let value = self.eval_expr(expr, env)?;
                let future = expect_future("await", value)?;
                await_future(&future)
            }
            // Postfix `EXPR?` error propagation. Evaluate the operand to an
            // `option`/`result` enum value; on the success variant (`some`/`ok`)
            // the expression is the payload, and on the failure variant
            // (`none`/`err`) we raise a function-level early-return signal that
            // carries the whole enum value. `invoke_function` (the call boundary)
            // catches the `L0430` sentinel and returns the stashed value as the
            // enclosing function's result — mirroring how `throw`'s `L0420`
            // unwinds to the nearest `try`/`catch`, but unwinding to the function
            // boundary instead. Semantics has already verified the operand is an
            // `option`/`result` and the return type is compatible.
            ExprKind::Try(inner) => {
                let value = self.eval_expr(inner, env)?;
                let Value::Enum(e) = &value else {
                    return Err(RuntimeError::new(
                        "L0428",
                        "`?` operand did not evaluate to an option/result value",
                    )
                    .with_span(expr.span));
                };
                let (variant, payload) = (&e.variant, &e.payload);
                match variant.as_str() {
                    "ok" | "some" => payload.first().cloned().ok_or_else(|| {
                        RuntimeError::new("L0428", format!("`{variant}` payload missing for `?`"))
                    }),
                    "err" | "none" => {
                        // Stash the whole failure value and unwind to the nearest
                        // function boundary via the `L0430` early-return sentinel.
                        self.pending_try_return = Some(value.clone());
                        Err(RuntimeError::new(
                            "L0430",
                            "`?` propagated a failure value to the enclosing function",
                        ))
                    }
                    other => Err(RuntimeError::new(
                        "L0428",
                        format!("`?` operand has unexpected variant `{other}`"),
                    )),
                }
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
            // Evaluating a closure literal snapshots the current environment's
            // in-scope locals by value and yields a backend-neutral
            // `Value::Closure` carrying the literal's parse-order `id` plus that
            // snapshot. The body is not stored here — it lives in `self.closures`,
            // keyed by `id`, and is looked up at invocation time.
            ExprKind::Closure { id, .. } => Ok(Value::Closure(Closure {
                id: *id,
                captured: env.snapshot_locals(),
            })),
        };
        result.map_err(|error| self.annotate_error(error, expr.span))
    }

    fn annotate_error(&self, error: RuntimeError, span: Span) -> RuntimeError {
        let error = error.with_span(span);
        match self.call_stack.last() {
            Some(frame) => error
                .with_function(frame.function.to_string())
                .with_traceback(self.build_traceback()),
            None => error,
        }
    }

    /// Materialize the active call stack as owned [`TraceFrame`]s for a
    /// `RuntimeError`. Called only on the error path, so the per-frame name
    /// clone stays off the hot call path (the live `call_stack` borrows each
    /// name from the program).
    fn build_traceback(&self) -> Vec<TraceFrame> {
        self.call_stack
            .iter()
            .map(|frame| TraceFrame {
                function: frame.function.to_string(),
                span: frame.span,
            })
            .collect()
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
                BinaryOp::Remainder => {
                    unreachable!("`%` requires integer operands (rejected by semantics)")
                }
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
        // 32-bit float arithmetic/comparison, IEEE 754 like f64. Storing a native
        // f32 rounds each result to f32 precision automatically.
        if let (Value::F32(l), Value::F32(r)) = (&left, &right) {
            let (l, r) = (*l, *r);
            return Ok(match op {
                BinaryOp::Add => Value::F32(l + r),
                BinaryOp::Subtract => Value::F32(l - r),
                BinaryOp::Multiply => Value::F32(l * r),
                BinaryOp::Divide => Value::F32(l / r),
                BinaryOp::Remainder => {
                    unreachable!("`%` requires integer operands (rejected by semantics)")
                }
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
        // Fixed-width integer arithmetic/comparison. Both operands carry the same
        // width/signedness tag (the type checker forbids mixing widths); the
        // arithmetic result is wrap-normalized back into that width, and ordering
        // reduces to plain `i64` comparison of the normalized cells (signed-
        // correct for `i32`, unsigned-correct for `u32`).
        if let (Value::Int { value: l, ty }, Value::Int { value: r, ty: rk }) = (&left, &right) {
            debug_assert_eq!(ty, rk, "mixed-width integer operands reached eval_binary");
            let (l, r, ty) = (*l, *r, *ty);
            return match op {
                BinaryOp::Add => Ok(Value::int(l.wrapping_add(r), ty)),
                BinaryOp::Subtract => Ok(Value::int(l.wrapping_sub(r), ty)),
                BinaryOp::Multiply => Ok(Value::int(l.wrapping_mul(r), ty)),
                BinaryOp::Divide => {
                    if r == 0 {
                        Err(RuntimeError::new("L0404", "division by zero"))
                    } else {
                        Ok(Value::int(int_div(l, r, ty), ty))
                    }
                }
                BinaryOp::Remainder => {
                    if r == 0 {
                        Err(RuntimeError::new("L0404", "remainder by zero"))
                    } else {
                        Ok(Value::int(int_rem(l, r, ty), ty))
                    }
                }
                BinaryOp::Equal => Ok(Value::Bool(l == r)),
                BinaryOp::NotEqual => Ok(Value::Bool(l != r)),
                BinaryOp::Less => Ok(Value::Bool(int_cmp(l, r, ty).is_lt())),
                BinaryOp::LessEqual => Ok(Value::Bool(int_cmp(l, r, ty).is_le())),
                BinaryOp::Greater => Ok(Value::Bool(int_cmp(l, r, ty).is_gt())),
                BinaryOp::GreaterEqual => Ok(Value::Bool(int_cmp(l, r, ty).is_ge())),
                // Bitwise ops operate on the normalized cell and re-normalize;
                // shifts mask the amount to the width and honor signedness.
                BinaryOp::BitAnd => Ok(Value::int(l & r, ty)),
                BinaryOp::BitOr => Ok(Value::int(l | r, ty)),
                BinaryOp::BitXor => Ok(Value::int(l ^ r, ty)),
                BinaryOp::Shl => Ok(Value::int(int_shl(l, r, ty), ty)),
                BinaryOp::Shr => Ok(Value::int(int_shr(l, r, ty), ty)),
                BinaryOp::And | BinaryOp::Or => {
                    unreachable!("logical ops short-circuit in eval_expr")
                }
            };
        }
        match op {
            // `+` concatenates when both operands are strings; otherwise it adds i64s.
            BinaryOp::Add if matches!((&left, &right), (Value::String(_), Value::String(_))) => {
                // Reuse the left operand's heap buffer: `String + &str` is a
                // `push_str`, amortized O(1) when capacity allows, so building a
                // string with `s = s + piece` in a loop stays O(n) overall
                // instead of reallocating a fresh buffer on every concat.
                Ok(Value::String(left.into_string()? + &right.as_string()?))
            }
            BinaryOp::Add => Ok(Value::I64(left.as_i64()? + right.as_i64()?)),
            BinaryOp::Subtract => Ok(Value::I64(left.as_i64()? - right.as_i64()?)),
            BinaryOp::Multiply => Ok(Value::I64(left.as_i64()? * right.as_i64()?)),
            BinaryOp::Divide => {
                let divisor = right.as_i64()?;
                if divisor == 0 {
                    Err(RuntimeError::new("L0404", "division by zero"))
                } else {
                    // Wrap the one signed-overflow case (`i64::MIN / -1`) to
                    // `i64::MIN` rather than panicking, matching `int_div` and the
                    // native backend's guarded `idiv`.
                    Ok(Value::I64(left.as_i64()?.wrapping_div(divisor)))
                }
            }
            BinaryOp::Remainder => {
                let divisor = right.as_i64()?;
                if divisor == 0 {
                    Err(RuntimeError::new("L0404", "remainder by zero"))
                } else {
                    // `i64::MIN % -1` is 0; `wrapping_rem` yields it without the
                    // overflow panic, matching the native backend's guarded `idiv`.
                    Ok(Value::I64(left.as_i64()?.wrapping_rem(divisor)))
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

    /// `size_of(x) -> i64`: the C-natural byte size of `x`'s type (a
    /// compile-time constant that depends only on the type).
    fn builtin_size_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("size_of", 1, args.len()))?;
        value.layout_size().map(Value::I64).ok_or_else(|| {
            RuntimeError::new(
                "L0431",
                "size_of requires a type with a defined memory layout",
            )
        })
    }

    /// `align_of(x) -> i64`: the C-natural alignment of `x`'s type.
    fn builtin_align_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("align_of", 1, args.len()))?;
        value.layout_align().map(Value::I64).ok_or_else(|| {
            RuntimeError::new(
                "L0431",
                "align_of requires a type with a defined memory layout",
            )
        })
    }

    /// `offset_of(x, "field") -> i64`: the C-natural byte offset of `field`
    /// within struct value `x`.
    fn builtin_offset_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value, field]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("offset_of", 2, args.len()))?;
        let field = field.as_string()?;
        value
            .layout_field_offset(&field)
            .map(Value::I64)
            .ok_or_else(|| {
                RuntimeError::new(
                    "L0431",
                    format!("offset_of could not resolve field `{field}` in a struct value"),
                )
            })
    }

    /// `ptr_to_int(p) -> i64`: the integer address/handle of a raw pointer. On
    /// the interpreters a pointer is a heap-slot handle, so this returns that
    /// handle; it round-trips through `int_to_ptr`.
    fn builtin_ptr_to_int(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("ptr_to_int", 1, args.len()))?;
        Ok(Value::I64(ptr.as_ptr()? as i64))
    }

    /// `int_to_ptr(n) -> ptr<T>`: reconstruct a raw pointer from an integer
    /// handle (the inverse of `ptr_to_int`).
    fn builtin_int_to_ptr(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("int_to_ptr", 1, args.len()))?;
        Ok(Value::Ptr(handle.as_i64()? as usize))
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
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        let text = text.as_string()?;
        #[cfg(target_arch = "wasm32")]
        {
            // wasm has no real stdout — capture program output into a thread-local
            // buffer the wasm entry point drains (the browser playground reads it).
            let _ = name;
            WASM_STDOUT.with(|buf| {
                let mut b = buf.borrow_mut();
                b.push_str(&text);
                if newline {
                    b.push('\n');
                }
            });
            Ok(Value::Void)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::io::Write;
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
            | Value::Int { .. }
            | Value::F64(_)
            | Value::F32(_)
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

    /// `is_digit(c char) -> bool`: whether `c` is an ASCII digit (`0`-`9`).
    fn builtin_is_digit(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_digit", args, |c| c.is_ascii_digit())
    }

    /// `is_alpha(c char) -> bool`: whether `c` is an alphabetic character.
    fn builtin_is_alpha(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_alpha", args, |c| c.is_alphabetic())
    }

    /// `is_alnum(c char) -> bool`: whether `c` is alphabetic or numeric.
    fn builtin_is_alnum(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_alnum", args, |c| c.is_alphanumeric())
    }

    /// `is_whitespace(c char) -> bool`: whether `c` is a whitespace character.
    fn builtin_is_whitespace(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_whitespace", args, |c| c.is_whitespace())
    }

    /// `is_upper(c char) -> bool`: whether `c` is an uppercase character.
    fn builtin_is_upper(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_upper", args, |c| c.is_uppercase())
    }

    /// `is_lower(c char) -> bool`: whether `c` is a lowercase character.
    fn builtin_is_lower(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_lower", args, |c| c.is_lowercase())
    }

    /// Shared helper for the deterministic `char -> bool` classification
    /// predicates: unwrap a single `char` operand and apply `test`, reporting a
    /// runtime error (never a panic) on a non-char operand.
    fn char_predicate(
        name: &'static str,
        args: Vec<Value>,
        test: impl Fn(char) -> bool,
    ) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        match value {
            Value::Char(c) => Ok(Value::Bool(test(c))),
            other => Err(RuntimeError::new(
                "L0417",
                format!("{name} expects a char but got `{other}`"),
            )),
        }
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

    /// `to_<T>(x i64) -> T`: reinterpret an `i64` into fixed-width integer `T`,
    /// truncating to `T`'s width (the wrapping conversion). Shared by every
    /// `to_i8`/`to_i16`/`to_i32`/`to_u16`/`to_u32`/`to_u64`/`to_isize`/`to_usize`.
    fn builtin_to_int(
        name: &'static str,
        args: Vec<Value>,
        ty: IntKind,
    ) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        Ok(Value::int(expect_i64(name, value)?, ty))
    }

    /// `to_i64(x) -> i64`: widen a fixed-width integer into `i64`. The cell is
    /// already normalized (signed kinds sign-extended, unsigned zero-extended),
    /// so widening is the identity on the stored value.
    fn builtin_to_i64(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_i64", 1, args.len()))?;
        match value {
            Value::Int { value, .. } => Ok(Value::I64(value)),
            other => Err(RuntimeError::new(
                "L0407",
                format!("to_i64 expects a fixed-width integer but got `{other}`"),
            )),
        }
    }

    /// `to_f32(x f64) -> f32`: round an `f64` to the nearest `f32`.
    fn builtin_to_f32(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_f32", 1, args.len()))?;
        Ok(Value::F32(value.as_f64()? as f32))
    }

    /// `to_f64(x f32) -> f64`: widen an `f32` to `f64` (exact).
    fn builtin_to_f64(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_f64", 1, args.len()))?;
        match value {
            Value::F32(value) => Ok(Value::F64(f64::from(value))),
            other => Err(RuntimeError::new(
                "L0421",
                format!("to_f64 expects an f32 but got `{other}`"),
            )),
        }
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

    /// `list_index_of(l, x) -> i64`: index of the first element equal to `x`, or -1.
    fn builtin_list_index_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, target]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_index_of", 2, args.len()))?;
        let values = expect_list("list_index_of", list)?;
        let index = values
            .iter()
            .position(|value| *value == target)
            .map(|i| i as i64)
            .unwrap_or(-1);
        Ok(Value::I64(index))
    }

    /// `list_contains(l, x) -> bool`: whether any element equals `x`.
    fn builtin_list_contains(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, target]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_contains", 2, args.len()))?;
        let values = expect_list("list_contains", list)?;
        Ok(Value::Bool(values.contains(&target)))
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

    /// `sort(l list<i64>) -> list<i64>`: a new list with the elements sorted
    /// ascending.
    fn builtin_sort(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sort", 1, args.len()))?;
        let values = expect_list("sort", list)?;
        sort_scalar_list("sort", values)
    }

    /// `sort_by(l list<T>, cmp fn(T, T) -> i64) -> list<T>`: return a new list
    /// sorted by the comparator (`cmp(a, b)` negative if `a` precedes `b`, zero
    /// if equal, positive if after). Uses a stable sort, so equal elements keep
    /// their input order. The comparator's error, if any, is propagated.
    fn builtin_sort_by(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, callee]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sort_by", 2, args.len()))?;
        let mut values = expect_list("sort_by", list)?;
        // A comparator error must abort the whole sort, so capture the first
        // error out of band; `sort_by` itself cannot propagate `Result`.
        let mut error: Option<RuntimeError> = None;
        values.sort_by(|a, b| {
            if error.is_some() {
                return std::cmp::Ordering::Equal;
            }
            match self.invoke_callable("sort_by", callee.clone(), vec![a.clone(), b.clone()]) {
                Ok(Value::I64(n)) => n.cmp(&0),
                Ok(other) => {
                    error = Some(RuntimeError::new(
                        "L0417",
                        format!("sort_by comparator must return i64 but returned `{other}`"),
                    ));
                    std::cmp::Ordering::Equal
                }
                Err(err) => {
                    error = Some(err);
                    std::cmp::Ordering::Equal
                }
            }
        });
        if let Some(err) = error {
            return Err(err);
        }
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

    /// Invoke a first-class function value (`Value::Func` name or a capturing
    /// `Value::Closure`) with `args`, reusing the same invocation path that
    /// `parallel_map`/direct call dispatch use. Shared by the higher-order list
    /// builtins so closures capture correctly and named functions resolve
    /// through the normal call machinery.
    fn invoke_callable(
        &mut self,
        builtin: &str,
        callee: Value,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        match callee {
            Value::Func(name) => self.call_function(&name, args),
            Value::Closure(closure) => self.invoke_closure(&closure, args),
            other => Err(RuntimeError::new(
                "L0417",
                format!("{builtin} expects a function but got `{other}`"),
            )),
        }
    }

    /// `list_map(l list<T>, f fn(T) -> U) -> list<U>`: apply `f` to each element
    /// in order, collecting the mapped values into a new list.
    fn builtin_list_map(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, callee]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_map", 2, args.len()))?;
        let values = expect_list("list_map", list)?;
        let mut mapped = Vec::with_capacity(values.len());
        for value in values {
            mapped.push(self.invoke_callable("list_map", callee.clone(), vec![value])?);
        }
        Ok(Value::Array(mapped))
    }

    /// `list_filter(l list<T>, pred fn(T) -> bool) -> list<T>`: keep the elements
    /// for which `pred` returns `true`, preserving input order.
    fn builtin_list_filter(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, callee]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_filter", 2, args.len()))?;
        let values = expect_list("list_filter", list)?;
        let mut kept = Vec::new();
        for value in values {
            let keep = self.invoke_callable("list_filter", callee.clone(), vec![value.clone()])?;
            if keep.as_bool()? {
                kept.push(value);
            }
        }
        Ok(Value::Array(kept))
    }

    /// `list_reduce(l list<T>, init U, f fn(U, T) -> U) -> U`: a left fold,
    /// threading the accumulator (starting at `init`) through `f(acc, element)`.
    fn builtin_list_reduce(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, init, callee]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_reduce", 3, args.len()))?;
        let values = expect_list("list_reduce", list)?;
        let mut acc = init;
        for value in values {
            acc = self.invoke_callable("list_reduce", callee.clone(), vec![acc, value])?;
        }
        Ok(acc)
    }

    /// `map_new() -> map<K, V>`: a fresh empty map.
    fn builtin_map_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_new", 0, args.len()))?;
        Ok(Value::Map(Box::default()))
    }

    /// `map_set(m, k, v) -> map<K, V>`: a new map with `k` mapped to `v`.
    /// Overwriting an existing key or appending a new one is O(1).
    fn builtin_map_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key, value]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_set", 3, args.len()))?;
        let mut entries = expect_map("map_set", map)?;
        entries.insert(key, value);
        Ok(Value::Map(Box::new(entries)))
    }

    /// `map_get(m, k) -> option<V>`: `some(v)` if present, else `none`. O(1).
    fn builtin_map_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_get", 2, args.len()))?;
        let entries = expect_map("map_get", map)?;
        let found = entries.get(&key).cloned();
        Ok(option_value(found))
    }

    /// `map_has(m, k) -> bool`. O(1).
    fn builtin_map_has(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_has", 2, args.len()))?;
        let entries = expect_map("map_has", map)?;
        Ok(Value::Bool(entries.contains_key(&key)))
    }

    /// `map_len(m) -> i64`.
    fn builtin_map_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_len", 1, args.len()))?;
        let entries = expect_map("map_len", map)?;
        Ok(Value::I64(entries.len() as i64))
    }

    /// `map_keys(m) -> list<K>`: the keys in insertion order.
    fn builtin_map_keys(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_keys", 1, args.len()))?;
        let entries = expect_map("map_keys", map)?;
        Ok(Value::Array(
            entries.into_entries().into_iter().map(|(k, _)| k).collect(),
        ))
    }

    /// `map_values(m) -> list<V>`: the values in insertion order.
    fn builtin_map_values(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_values", 1, args.len()))?;
        let entries = expect_map("map_values", map)?;
        Ok(Value::Array(
            entries.into_entries().into_iter().map(|(_, v)| v).collect(),
        ))
    }

    /// `map_del(m, k) -> map<K, V>`: a new map without key `k`.
    fn builtin_map_del(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_del", 2, args.len()))?;
        let mut entries = expect_map("map_del", map)?;
        entries.remove(&key);
        Ok(Value::Map(Box::new(entries)))
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

    /// `chars(s) -> list<char>`: the characters of `s` in order.
    fn builtin_chars(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("chars", 1, args.len()))?;
        let text = expect_string("chars", text)?;
        Ok(Value::Array(text.chars().map(Value::Char).collect()))
    }

    /// `string_from_chars(cs) -> string`: concatenate a `list<char>` into a string.
    fn builtin_string_from_chars(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("string_from_chars", 1, args.len()))?;
        let values = expect_list("string_from_chars", list)?;
        let mut out = String::new();
        for value in values {
            match value {
                Value::Char(c) => out.push(c),
                other => {
                    return Err(RuntimeError::new(
                        "L0417",
                        format!("string_from_chars expects a list<char> but found `{other}`"),
                    ));
                }
            }
        }
        Ok(Value::String(out))
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

    /// `parse_i64(s string) -> result<i64, string>`: parse `s` as a base-10
    /// signed 64-bit integer via Rust `str::parse::<i64>()`, returning `ok(n)`
    /// on success and `err(message)` on any failure (empty, non-numeric, or out
    /// of range). Whitespace is not trimmed, so a padded string is an `err`. The
    /// error message is a fixed string so every backend matches byte-for-byte.
    fn builtin_parse_i64(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("parse_i64", 1, args.len()))?;
        let text = expect_string("parse_i64", text)?;
        Ok(result_value(match text.parse::<i64>() {
            Ok(value) => Ok(Value::I64(value)),
            Err(_) => Err(Value::String(format!("cannot parse `{text}` as i64"))),
        }))
    }

    /// `parse_f64(s string) -> result<f64, string>`: parse `s` as an `f64` via
    /// Rust `str::parse::<f64>()`, returning `ok(x)` on success and
    /// `err(message)` on failure. The error message is a fixed string so every
    /// backend matches byte-for-byte.
    fn builtin_parse_f64(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("parse_f64", 1, args.len()))?;
        let text = expect_string("parse_f64", text)?;
        Ok(result_value(match text.parse::<f64>() {
            Ok(value) => Ok(Value::F64(value)),
            Err(_) => Err(Value::String(format!("cannot parse `{text}` as f64"))),
        }))
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

    /// `clamp(x, lo, hi) -> T`: `x` limited to `[lo, hi]`. Total on every input:
    /// for `lo > hi` it yields `lo`, and for an f64 NaN `x` it returns `x`
    /// unchanged.
    fn builtin_clamp(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [x, lo, hi]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("clamp", 3, args.len()))?;
        match (x, lo, hi) {
            (Value::I64(x), Value::I64(lo), Value::I64(hi)) => Ok(Value::I64(if x < lo {
                lo
            } else if x > hi {
                hi
            } else {
                x
            })),
            (Value::F64(x), Value::F64(lo), Value::F64(hi)) => Ok(Value::F64(if x < lo {
                lo
            } else if x > hi {
                hi
            } else {
                x
            })),
            (x, lo, hi) => Err(RuntimeError::new(
                "L0417",
                format!(
                    "clamp expects three matching i64 or f64 values but got `{x}`, `{lo}`, and `{hi}`"
                ),
            )),
        }
    }

    /// `sign(x) -> i64`: `-1`/`0`/`1` for negative/zero/positive. For f64, `NaN`
    /// and `-0.0` both map to `0`. Always returns `i64`.
    fn builtin_sign(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sign", 1, args.len()))?;
        match value {
            Value::I64(n) => Ok(Value::I64(n.signum())),
            Value::F64(n) => Ok(Value::I64(if n > 0.0 {
                1
            } else if n < 0.0 {
                -1
            } else {
                0
            })),
            other => Err(RuntimeError::new(
                "L0417",
                format!("sign expects an i64 or f64 but got `{other}`"),
            )),
        }
    }

    /// `gcd(a, b) -> i64`: non-negative greatest common divisor of the absolute
    /// values (see `gcd_i64`, which is total even at `i64::MIN`).
    fn builtin_gcd(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [a, b]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("gcd", 2, args.len()))?;
        match (a, b) {
            (Value::I64(a), Value::I64(b)) => Ok(Value::I64(gcd_i64(a, b))),
            (a, b) => Err(RuntimeError::new(
                "L0417",
                format!("gcd expects two i64 values but got `{a}` and `{b}`"),
            )),
        }
    }

    /// `list_sum(l) -> T`: the sum of a `list<i64>` (wrapping, matching `+`) or a
    /// `list<f64>`. An empty list yields `0`/`0.0`.
    fn builtin_list_sum(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_sum", 1, args.len()))?;
        let values = expect_list("list_sum", list)?;
        list_sum_values("list_sum", values)
    }

    /// `list_min(l) -> option<T>`: `none` on empty, else `some(minimum)`.
    fn builtin_list_min(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_min", 1, args.len()))?;
        let values = expect_list("list_min", list)?;
        Ok(option_value(list_extreme("list_min", values, false)?))
    }

    /// `list_max(l) -> option<T>`: `none` on empty, else `some(maximum)`.
    fn builtin_list_max(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_max", 1, args.len()))?;
        let values = expect_list("list_max", list)?;
        Ok(option_value(list_extreme("list_max", values, true)?))
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

/// Conservative "does `name` appear anywhere in this expression?" walk used by
/// the move-on-functional-update fast path to prove the target variable is not
/// referenced outside its single consuming argument. It over-approximates on
/// purpose: a mention inside a nested closure body (which may actually bind a
/// fresh `name`) still counts, and a call *name* equal to `name` counts too.
/// Over-approximating only ever forgoes the optimization — it never changes an
/// observable result — so the walk stays simple and total over `ExprKind`.
fn expr_mentions_var(expr: &Expr, name: &str) -> bool {
    match &expr.kind {
        ExprKind::Integer(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::String(_)
        | ExprKind::Char(_) => false,
        ExprKind::Variable(v) => v == name,
        ExprKind::Array(items) => items.iter().any(|item| expr_mentions_var(item, name)),
        ExprKind::Index { target, index } => {
            expr_mentions_var(target, name) || expr_mentions_var(index, name)
        }
        ExprKind::Unary { expr, .. } => expr_mentions_var(expr, name),
        ExprKind::Binary { left, right, .. } => {
            expr_mentions_var(left, name) || expr_mentions_var(right, name)
        }
        ExprKind::Call { name: callee, args } => {
            callee == name || args.iter().any(|arg| expr_mentions_var(arg, name))
        }
        ExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|(_, value)| expr_mentions_var(value, name)),
        ExprKind::Field { target, .. } => expr_mentions_var(target, name),
        ExprKind::Match { scrutinee, arms } => {
            expr_mentions_var(scrutinee, name)
                || arms
                    .iter()
                    .any(|arm| arm.body.iter().any(|stmt| stmt_mentions_var(stmt, name)))
        }
        ExprKind::Await { expr } => expr_mentions_var(expr, name),
        ExprKind::Try(inner) => expr_mentions_var(inner, name),
        ExprKind::Closure { body, .. } => expr_mentions_var(body, name),
    }
}

/// Statement-level companion to [`expr_mentions_var`] for `match` arm bodies.
/// Also conservative: any syntactic mention of `name` counts.
fn stmt_mentions_var(stmt: &Stmt, name: &str) -> bool {
    match stmt {
        Stmt::Let { value, .. } | Stmt::Throw { value, .. } => expr_mentions_var(value, name),
        Stmt::Assign {
            name: target,
            path,
            value,
            ..
        } => {
            target == name
                || path.iter().any(|place| match place {
                    Place::Field(_) => false,
                    Place::Index(index) => expr_mentions_var(index, name),
                })
                || expr_mentions_var(value, name)
        }
        Stmt::Expr(expr) => expr_mentions_var(expr, name),
        Stmt::Return(expr) => expr.as_ref().is_some_and(|e| expr_mentions_var(e, name)),
        Stmt::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().any(|branch| {
                expr_mentions_var(&branch.condition, name)
                    || branch.body.iter().any(|s| stmt_mentions_var(s, name))
            }) || else_body.iter().any(|s| stmt_mentions_var(s, name))
        }
        Stmt::While {
            condition, body, ..
        } => expr_mentions_var(condition, name) || body.iter().any(|s| stmt_mentions_var(s, name)),
        Stmt::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_mentions_var(start, name)
                || expr_mentions_var(end, name)
                || step.as_ref().is_some_and(|e| expr_mentions_var(e, name))
                || body.iter().any(|s| stmt_mentions_var(s, name))
        }
        Stmt::Loop { body, .. } | Stmt::Unsafe { body, .. } => {
            body.iter().any(|s| stmt_mentions_var(s, name))
        }
        Stmt::Try {
            body, catch_body, ..
        } => {
            body.iter().any(|s| stmt_mentions_var(s, name))
                || catch_body.iter().any(|s| stmt_mentions_var(s, name))
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Region(_) | Stmt::Asm { .. } => false,
    }
}

/// A lexical environment: a stack of scopes, each an insertion-ordered
/// association list of `(name, value)`. Function-call and block scopes are
/// small (a handful of bindings), so a linear-scan `Vec` beats a `HashMap`
/// here — it avoids a per-scope bucket allocation and per-access string
/// hashing, and its contiguous layout is cache-friendly. `define` keeps at
/// most one binding per name per scope (replacing in place, exactly like the
/// previous `HashMap::insert`), so resolution never has to disambiguate
/// duplicates within a scope; shadowing across scopes is innermost-first.
#[derive(Debug, Clone)]
struct Env {
    scopes: Vec<Vec<(String, Value)>>,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            scopes: vec![Vec::new()],
        }
    }
}

impl Env {
    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Reset to a single empty scope so a pooled environment can be reused for the
    /// next call. Each scope's backing `Vec` keeps its capacity, so a repeated
    /// call re-binds its locals without reallocating; clearing every entry means
    /// no stale binding can leak into the reused environment.
    fn reset(&mut self) {
        self.scopes.truncate(1);
        match self.scopes.first_mut() {
            Some(first) => first.clear(),
            None => self.scopes.push(Vec::new()),
        }
    }

    fn define(&mut self, name: String, value: Value) {
        let scope = self.scopes.last_mut().expect("env always has a scope");
        // `let` may redefine a name already bound in this scope; replace that
        // binding in place so there is exactly one entry per name per scope
        // (matching the previous `HashMap::insert` semantics).
        for (existing, slot) in scope.iter_mut() {
            if *existing == name {
                *slot = value;
                return;
            }
        }
        scope.push((name, value));
    }

    fn assign(&mut self, name: &str, value: Value) -> Result<(), RuntimeError> {
        for scope in self.scopes.iter_mut().rev() {
            for (existing, slot) in scope.iter_mut() {
                if existing == name {
                    *slot = value;
                    return Ok(());
                }
            }
        }
        Err(RuntimeError::new(
            "L0403",
            format!("unknown variable `{name}`"),
        ))
    }

    fn get(&self, name: &str) -> Result<Value, RuntimeError> {
        self.get_ref(name)
            .cloned()
            .ok_or_else(|| RuntimeError::new("L0403", format!("unknown variable `{name}`")))
    }

    /// Borrow a binding's value without cloning it, resolving innermost-first
    /// exactly like [`Env::get`]. Used to classify a call target (closure/func
    /// value vs. builtin) on the move-on-functional-update fast path without
    /// paying for a clone.
    fn get_ref(&self, name: &str) -> Option<&Value> {
        for scope in self.scopes.iter().rev() {
            for (existing, value) in scope.iter() {
                if existing == name {
                    return Some(value);
                }
            }
        }
        None
    }

    /// True when `name` is bound in the innermost (current) scope. A `let x =
    /// f(x, …)` re-binding only moves when the consumed binding lives here,
    /// because `let` shadows (defines into the innermost scope) rather than
    /// overwriting an outer binding — moving from an outer scope would corrupt it.
    fn innermost_has(&self, name: &str) -> bool {
        self.scopes
            .last()
            .is_some_and(|scope| scope.iter().any(|(n, _)| n == name))
    }

    /// True when `name` is bound in any scope (a normal local). A plain `x =
    /// f(x, …)` reassignment moves from — and writes back to — the *nearest*
    /// binding, and both [`Env::get`] and [`Env::assign`] resolve nearest-first to
    /// that same slot, so the move is safe at any scope depth (e.g. `x` declared
    /// outside a loop, reassigned inside it).
    fn is_bound(&self, name: &str) -> bool {
        self.get_ref(name).is_some()
    }

    /// Move the value out of the nearest scope binding `name`, leaving a cheap
    /// [`Value::Void`] placeholder in the same slot (no clone), and return the old
    /// value. Nearest-first, matching [`Env::get`]/[`Env::assign`] resolution, so
    /// the caller's write-back overwrites this exact slot. The placeholder is
    /// never observable: on the fast path all other work is already done and the
    /// result is written back immediately, and the gating builtin cannot raise a
    /// catchable error mid-call.
    fn move_out_nearest(&mut self, name: &str) -> Option<Value> {
        for scope in self.scopes.iter_mut().rev() {
            for (existing, slot) in scope.iter_mut() {
                if existing == name {
                    return Some(std::mem::replace(slot, Value::Void));
                }
            }
        }
        None
    }

    /// Snapshot every in-scope local by value: one `(name, value.clone())` per
    /// visible binding, with an inner scope's binding shadowing an outer one of
    /// the same name. This is the frame-capture-by-value used when a closure
    /// literal is evaluated. The order is deterministic (outer-to-inner insertion
    /// with later scopes overwriting), which is all closure invocation needs.
    fn snapshot_locals(&self) -> Vec<(String, Value)> {
        let mut flattened: HashMap<&str, &Value> = HashMap::new();
        // Iterate outermost-to-innermost so an inner scope overwrites an outer
        // binding of the same name.
        for scope in &self.scopes {
            for (name, value) in scope {
                flattened.insert(name.as_str(), value);
            }
        }
        let mut captured: Vec<(String, Value)> = flattened
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect();
        // Sort by name for a stable, reproducible capture order.
        captured.sort_by(|(a, _), (b, _)| a.cmp(b));
        captured
    }
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
            Self::String(value) => Ok(value.clone()),
            _ => Err(RuntimeError::new("L0417", "expected string value")),
        }
    }

    /// Move the owned `String` out of a [`Value::String`] without cloning its
    /// heap buffer. Used for the left operand of string `+` so the concatenation
    /// reuses (and grows in place) that buffer instead of allocating a fresh one;
    /// the result is byte-identical to `as_string`, only cheaper.
    pub fn into_string(self) -> Result<String, RuntimeError> {
        match self {
            Self::String(value) => Ok(value),
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
    fn move_on_functional_update_builds_list_correctly() {
        // `l = push(l, i)` in a loop consumes `l` by move on the fast path; the
        // built list must be byte-for-byte what a clone would produce. Sum of
        // 0..=49 is 1225, length 50.
        let source = concat!(
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    for i from 0 to 49\n",
            "        l = push(l, i)\n",
            "    let total i64 = 0\n",
            "    let n i64 = len(l)\n",
            "    for i from 0 to n - 1\n",
            "        total += get(l, i)\n",
            "    total * 100 + len(l)\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(122550));
    }

    #[test]
    fn move_on_functional_update_preserves_aliased_binding() {
        // `let b = a` clones `a`; the subsequent `a = push(a, 9)` moves `a`'s slot
        // but must never corrupt the independent `b`.
        let source = concat!(
            "fn main -> i64\n",
            "    let a list<i64> = list_new()\n",
            "    a = push(a, 1)\n",
            "    a = push(a, 2)\n",
            "    a = push(a, 3)\n",
            "    let b list<i64> = a\n",
            "    a = push(a, 9)\n",
            "    let bsum i64 = get(b, 0) + get(b, 1) + get(b, 2)\n",
            "    len(a) * 10000 + get(a, 3) * 100 + len(b) * 10 + bsum\n",
        );
        // a=[1,2,3,9], b=[1,2,3]: 40936. A corrupted b would change the result.
        assert_eq!(run_source(source).expect("run"), Value::I64(40936));
    }

    #[test]
    fn move_on_functional_update_error_in_other_argument_leaves_target_intact() {
        // In `l = push(l, boom())` the other argument throws before `l` is moved,
        // so `l` stays intact and the `catch` observes the original list, never a
        // moved-out placeholder.
        let source = concat!(
            "fn boom -> i64\n",
            "    throw \"boom\"\n\n",
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, 1)\n",
            "    l = push(l, 2)\n",
            "    let caught i64 = 0\n",
            "    try\n",
            "        l = push(l, boom())\n",
            "    catch m\n",
            "        caught = 1\n",
            "    caught * 1000 + len(l) * 10 + get(l, 0) + get(l, 1)\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(1023));
    }

    #[test]
    fn non_blocking_recv_surfaces_would_block_as_none() {
        // A UDP socket bound to an ephemeral loopback port and put into
        // non-blocking mode reports "no datagram pending" as `ok(none)` — never
        // blocking — so this is deterministic with no live peer. The fixture maps
        // `set_nonblocking` ok to 100 and an `ok(none)` recv to 10, summing 110.
        let source = concat!(
            "fn probe s Socket -> i64\n",
            "    let toggled result<i64, string> = set_nonblocking(s, true)\n",
            "    let received result<option<string>, string> = udp_recv_nb(s)\n",
            "    tcp_close(s)\n",
            "    let a i64 = unwrap_toggle(toggled)\n",
            "    let b i64 = unwrap_recv(received)\n",
            "    a + b\n\n",
            "fn unwrap_toggle r result<i64, string> -> i64\n",
            "    match r\n",
            "        ok(code) -> 100\n",
            "        err(message) -> 0\n\n",
            "fn unwrap_recv r result<option<string>, string> -> i64\n",
            "    match r\n",
            "        ok(maybe) ->\n",
            "            match maybe\n",
            "                some(data) -> 0\n",
            "                none -> 10\n",
            "        err(message) -> 0\n\n",
            "fn main -> i64\n",
            "    let bound result<Socket, string> = udp_bind(\"127.0.0.1\", 0)\n",
            "    match bound\n",
            "        ok(s) -> probe(s)\n",
            "        err(message) -> 0\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(110));
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
    fn rejects_extern_call_on_the_ast_interpreter() {
        // A C-ABI `extern fn` call is native-only; the AST interpreter rejects it
        // with `L0423` regardless of the C scalar width (here an `i32` signature),
        // rather than executing C or silently no-op-ing.
        let source =
            "extern fn toupper c i32 -> i32\n\nfn main -> i64\n    to_i64(toupper(to_i32(97)))\n";
        let error = run_source(source).expect_err("extern call must not run on an interpreter");
        assert_eq!(error.code, "L0423");
        assert!(
            error.message.contains("toupper") && error.message.contains("lullaby native"),
            "L0423 names the extern and points at `lullaby native`: {}",
            error.message
        );
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
    fn atomic_ops_are_deterministic_single_threaded() {
        // Exercise the full atomic surface deterministically, mirroring the
        // `run_atomics.lby` parity fixture: new(10), add(5) -> prev 10 (cell 15),
        // load -> 15, cas(15, 99) -> 15 (cell 99), load -> 99, swap(7) -> 99
        // (cell 7), and the bitwise fetch-ops.
        let source = concat!(
            "fn main -> i64\n",
            "    let a atomic_i64 = atomic_new(10)\n",
            "    let p0 i64 = atomic_add(a, 5)\n", // prev 10, cell 15
            "    let l0 i64 = atomic_load(a)\n",   // 15
            "    let c0 i64 = atomic_cas(a, 15, 99)\n", // 15, cell 99
            "    let l1 i64 = atomic_load(a)\n",   // 99
            "    let s0 i64 = atomic_swap(a, 7)\n", // 99, cell 7
            "    let sub0 i64 = atomic_sub(a, 2)\n", // prev 7, cell 5
            "    let and0 i64 = atomic_and(a, 6)\n", // prev 5 (5&6=4), cell 4
            "    let or0 i64 = atomic_or(a, 1)\n", // prev 4 (4|1=5), cell 5
            "    let xor0 i64 = atomic_xor(a, 7)\n", // prev 5 (5^7=2), cell 2
            "    let final i64 = atomic_load(a)\n", // 2
            "    p0 + l0 + c0 + l1 + s0 + sub0 + and0 + or0 + xor0 + final\n",
        );
        // 10 + 15 + 15 + 99 + 99 + 7 + 5 + 4 + 5 + 2 = 261.
        assert_eq!(run_source(source).expect("run"), Value::I64(261));
    }

    #[test]
    fn ordered_atomics_run_deterministically() {
        // Mirrors the `run_atomic_orderings.lby` parity fixture: a `release`
        // store, `acquire`/`relaxed`/`seq_cst` loads, a `relaxed` fetch-and-add,
        // an `acq_rel`/`acquire` CAS, a `seq_cst` swap, a `relaxed` fetch-and-sub,
        // and a `seq_cst` fence. Single-threaded, so the ordering does not change
        // the produced value: the deterministic total is 300.
        let source = concat!(
            "fn main -> i64\n",
            "    let a atomic_i64 = atomic_new(10)\n",
            "    atomic_store_ordered(a, 20, release)\n", // cell 20
            "    let l0 i64 = atomic_load_ordered(a, acquire)\n", // 20
            "    let p0 i64 = atomic_add_ordered(a, 5, relaxed)\n", // prev 20, cell 25
            "    let l1 i64 = atomic_load_ordered(a, seq_cst)\n", // 25
            "    let c0 i64 = atomic_cas_ordered(a, 25, 99, acq_rel, acquire)\n", // 25, cell 99
            "    let l2 i64 = atomic_load_ordered(a, relaxed)\n", // 99
            "    let s0 i64 = atomic_swap_ordered(a, 7, seq_cst)\n", // prev 99, cell 7
            "    let sub0 i64 = atomic_sub_ordered(a, 2, relaxed)\n", // prev 7, cell 5
            "    fence(seq_cst)\n",
            "    let last i64 = atomic_load_ordered(a, acquire)\n", // 5
            "    l0 + p0 + l1 + c0 + l2 + s0 + sub0 + last\n",
        );
        // 20 + 20 + 25 + 25 + 99 + 99 + 7 + 5 = 300.
        assert_eq!(run_source(source).expect("run"), Value::I64(300));
    }

    #[test]
    fn expect_memory_order_maps_each_variant_to_std_ordering() {
        // The five `MemoryOrder` unit variants decode to the exact std ordering,
        // proving the interpreter selects the real hardware/std ordering rather
        // than seq_cst for everything.
        let order = |name: &str| {
            expect_memory_order(
                "t",
                Value::Enum(Box::new(EnumValue {
                    enum_name: "MemoryOrder".to_string(),
                    variant: name.to_string(),
                    payload: Vec::new(),
                })),
            )
            .expect("decode")
        };
        assert_eq!(order("relaxed"), Ordering::Relaxed);
        assert_eq!(order("acquire"), Ordering::Acquire);
        assert_eq!(order("release"), Ordering::Release);
        assert_eq!(order("acq_rel"), Ordering::AcqRel);
        assert_eq!(order("seq_cst"), Ordering::SeqCst);
    }

    #[test]
    fn ordered_atomic_builtins_guard_invalid_orderings_without_panicking() {
        // A dynamically supplied ordering that is illegal for the op returns a
        // clean `L0432` runtime error instead of panicking inside `std`.
        let atomic = || {
            Value::Atomic(SharedAtomic {
                cell: Arc::new(AtomicI64::new(0)),
            })
        };
        let order = |name: &str| {
            Value::Enum(Box::new(EnumValue {
                enum_name: "MemoryOrder".to_string(),
                variant: name.to_string(),
                payload: Vec::new(),
            }))
        };
        // A `release` load is illegal.
        let load = builtin_atomic_load_ordered(vec![atomic(), order("release")]);
        assert_eq!(load.expect_err("guard").code, "L0432");
        // An `acquire` store is illegal.
        let store = builtin_atomic_store_ordered(vec![atomic(), Value::I64(1), order("acquire")]);
        assert_eq!(store.expect_err("guard").code, "L0432");
        // A `relaxed` fence is illegal.
        let fence = builtin_fence(vec![order("relaxed")]);
        assert_eq!(fence.expect_err("guard").code, "L0432");
        // A `release` CAS failure ordering is illegal.
        let cas = builtin_atomic_cas_ordered(vec![
            atomic(),
            Value::I64(0),
            Value::I64(1),
            order("seq_cst"),
            order("release"),
        ]);
        assert_eq!(cas.expect_err("guard").code, "L0432");
    }

    #[test]
    fn atomic_shared_across_threads_via_clone() {
        // The `Value::Atomic` handle shares its `Arc<AtomicI64>` on clone, so
        // many OS threads racing `atomic_add` against the same cell lose no
        // updates: the final total is the exact sum. This proves real atomicity
        // (an ordinary `mutex`-free counter would drop increments under this
        // contention).
        let atomic = SharedAtomic {
            cell: Arc::new(AtomicI64::new(0)),
        };
        let value = Value::Atomic(atomic);
        const THREADS: i64 = 8;
        const ITERS: i64 = 10_000;
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let handle = value.clone();
                scope.spawn(move || {
                    for _ in 0..ITERS {
                        Runtime::builtin_atomic_add(vec![handle.clone(), Value::I64(1)])
                            .expect("atomic_add");
                    }
                });
            }
        });
        assert_eq!(
            Runtime::builtin_atomic_load(vec![value]).expect("atomic_load"),
            Value::I64(THREADS * ITERS)
        );
    }

    #[test]
    fn atomic_add_returns_previous_and_races_lose_no_updates() {
        // A second multi-threaded proof that also checks the fetch-and-op
        // *return contract*: `atomic_add` returns the PREVIOUS value, so the set
        // of returned values across a single-threaded run is a permutation of
        // the prefix sums. Here we assert the stronger cross-thread invariant:
        // with N threads each adding a distinct large stride, the final load is
        // the exact arithmetic sum with no lost update.
        let atomic = SharedAtomic {
            cell: Arc::new(AtomicI64::new(0)),
        };
        let value = Value::Atomic(atomic);
        const THREADS: i64 = 6;
        const ITERS: i64 = 5_000;
        const STRIDE: i64 = 3;
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let handle = value.clone();
                scope.spawn(move || {
                    for _ in 0..ITERS {
                        Runtime::builtin_atomic_add(vec![handle.clone(), Value::I64(STRIDE)])
                            .expect("atomic_add");
                    }
                });
            }
        });
        assert_eq!(
            Runtime::builtin_atomic_load(vec![value]).expect("atomic_load"),
            Value::I64(THREADS * ITERS * STRIDE)
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
        let unit = Value::Enum(Box::new(EnumValue {
            enum_name: "Shape".to_string(),
            variant: "Empty".to_string(),
            payload: Vec::new(),
        }));
        assert_eq!(unit.to_string(), "Empty");

        let payload = Value::Enum(Box::new(EnumValue {
            enum_name: "Shape".to_string(),
            variant: "Circle".to_string(),
            payload: vec![Value::F64(2.0)],
        }));
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
    fn proc_spawn_missing_command_yields_err_result() {
        // Spawning a command that does not exist on any platform deterministically
        // takes the `err` arm, so the program returns 1. This mirrors the
        // backend-invariant `run_process.lby` parity fixture. (Array literals must
        // be non-empty in the current alpha, so a harmless arg is supplied; a
        // missing command fails to spawn regardless of its arguments.)
        let source = concat!(
            "fn main -> i64\n",
            "    let outcome result<process, string> = proc_spawn(\"lullaby_definitely_not_a_real_program_zzz\", [\"--version\"])\n",
            "    match outcome\n",
            "        ok(p) -> 7\n",
            "        err(message) -> 1\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(1));
    }

    #[test]
    fn proc_spawn_wait_and_stdout_success_path() {
        // Spawn a universally-available shell that echoes `hello`, wait for exit,
        // and assert the exit code is 0 and captured stdout contains `hello`. The
        // command is platform-conditional so the test runs on the host. Every
        // `match` sits in tail position (via helper functions) to stay within the
        // fixture-style surface the parser accepts.
        let (cmd, arg0, arg1) = if cfg!(windows) {
            ("cmd", "/c", "echo hello")
        } else {
            ("sh", "-c", "echo hello")
        };
        let source = format!(
            concat!(
                "fn main -> i64\n",
                "    let spawned result<process, string> = proc_spawn(\"{cmd}\", [\"{arg0}\", \"{arg1}\"])\n",
                "    match spawned\n",
                "        ok(p) -> run_child(p)\n",
                "        err(message) -> 100\n",
                "\n",
                "fn run_child p process -> i64\n",
                "    let waited result<i64, string> = proc_wait(p)\n",
                "    let captured result<string, string> = proc_stdout(p)\n",
                "    check_wait(waited, captured)\n",
                "\n",
                "fn check_wait waited result<i64, string> captured result<string, string> -> i64\n",
                "    match waited\n",
                "        ok(status) -> check_output(status, captured)\n",
                "        err(message) -> 200\n",
                "\n",
                "fn check_output code i64 captured result<string, string> -> i64\n",
                "    match captured\n",
                "        ok(text) -> classify(code, text)\n",
                "        err(message) -> 300\n",
                "\n",
                "fn classify code i64 text string -> i64\n",
                "    if code == 0 and contains(text, \"hello\")\n",
                "        0\n",
                "    else\n",
                "        1\n",
            ),
            cmd = cmd,
            arg0 = arg0,
            arg1 = arg1,
        );
        assert_eq!(run_source(&source).expect("run"), Value::I64(0));
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

    #[test]
    fn try_operator_propagates_ok_and_err_on_ast_backend() {
        // `checked(a)? + checked(b)?` yields the sum when both succeed and
        // short-circuits with the first `err` otherwise. The AST interpreter
        // realizes `?` via a function-level early-return signal.
        let source = concat!(
            "fn checked n i64 -> result<i64, string>\n",
            "    if n < 0\n",
            "        return err(\"neg\")\n",
            "    ok(n)\n\n",
            "fn add_checked a i64 b i64 -> result<i64, string>\n",
            "    let x i64 = checked(a)?\n",
            "    let y i64 = checked(b)?\n",
            "    ok(x + y)\n\n",
            "fn unwrap r result<i64, string> -> i64\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> 0 - len(m)\n\n",
            "fn main -> i64\n",
            // success 3 + 4 = 7; failure err("neg") -> -3.
            "    unwrap(add_checked(3, 4)) + unwrap(add_checked(-1, 4))\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(4));
    }

    #[test]
    fn try_operator_propagates_none_on_ast_backend() {
        // `?` on an `option` returns `none` from the enclosing option-returning
        // function when the operand is `none`.
        let source = concat!(
            "fn lookup present bool -> option<i64>\n",
            "    if present\n",
            "        return some(9)\n",
            "    none\n\n",
            "fn twice present bool -> option<i64>\n",
            "    let x i64 = lookup(present)?\n",
            "    some(x + x)\n\n",
            "fn unwrap o option<i64> -> i64\n",
            "    match o\n",
            "        some(v) -> v\n",
            "        none -> -1\n\n",
            "fn main -> i64\n",
            // present -> 18; absent -> none -> -1.
            "    unwrap(twice(true)) + unwrap(twice(false))\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(17));
    }

    #[test]
    fn nested_try_operator_runs_on_ast_backend() {
        // `checked(checked(n)? + n)?` nests two `?`s in one expression; both must
        // succeed for the value to flow through.
        let source = concat!(
            "fn checked n i64 -> result<i64, string>\n",
            "    if n < 0\n",
            "        return err(\"neg\")\n",
            "    ok(n)\n\n",
            "fn double_checked n i64 -> result<i64, string>\n",
            "    let v i64 = checked(checked(n)? + n)?\n",
            "    ok(v)\n\n",
            "fn unwrap r result<i64, string> -> i64\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> 0 - len(m)\n\n",
            "fn main -> i64\n",
            // double_checked(5) = 10; double_checked(-2) = err("neg") -> -3.
            "    unwrap(double_checked(5)) + unwrap(double_checked(-2))\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(7));
    }

    #[test]
    fn int_kind_normalize_wraps_each_width() {
        assert_eq!(IntKind::I8.normalize(128), -128);
        assert_eq!(IntKind::I16.normalize(32_768), -32_768);
        assert_eq!(IntKind::I32.normalize(2_147_483_648), -2_147_483_648);
        assert_eq!(IntKind::U16.normalize(-1), 65_535);
        assert_eq!(IntKind::U32.normalize(-1), 4_294_967_295);
        // The 64-bit unsigned kinds fill the cell; normalization keeps the bits.
        assert_eq!(IntKind::U64.normalize(-1), -1);
        assert_eq!(IntKind::Usize.normalize(-1), -1);
    }

    #[test]
    fn int_div_and_cmp_respect_signedness_at_64_bit() {
        // `to_u64(0 - 1)` is stored as the bit pattern of -1, i.e. u64::MAX.
        let umax = IntKind::U64.normalize(-1);
        // Unsigned division divides on the magnitude, not the signed -1.
        assert_eq!(int_div(umax, 2, IntKind::U64), (u64::MAX / 2) as i64);
        // Signed i64-style division of the same bits would be 0 (-1 / 2).
        assert_eq!(int_div(-1, 2, IntKind::Isize), 0);
        // Unsigned ordering treats the cell as u64::MAX (greater than 1).
        assert!(int_cmp(umax, 1, IntKind::U64).is_gt());
        // Signed ordering of the same bits (-1) is less than 1.
        assert!(int_cmp(-1, 1, IntKind::Isize).is_lt());
    }

    #[test]
    fn signed_division_min_over_neg_one_wraps_not_panics() {
        // `i64::MIN / -1` is the one signed-overflow case: raw `/` panics, but the
        // language wraps it to `i64::MIN` on every backend. `int_div` on the
        // 64-bit signed kind (`isize`) yields `i64::MIN`; the plain-`i64`
        // interpreter path is covered end-to-end by the `run_div_overflow`
        // fixture. A narrower signed kind never reaches the overflow (the
        // sign-extended cell is not `i64::MIN`), but still divides correctly.
        assert_eq!(int_div(i64::MIN, -1, IntKind::Isize), i64::MIN);
        assert_eq!(int_div(-128, -1, IntKind::I8), 128);
    }

    #[test]
    fn runs_unsigned_64_bit_wraparound_end_to_end() {
        // `to_u64(0 - 1)` is u64::MAX; dividing by 2 uses unsigned semantics, and
        // `to_i64` reinterprets the resulting bits back into an i64.
        let source = concat!(
            "fn main -> i64\n",
            "    let big u64 = to_u64(0 - 1)\n",
            "    let half u64 = big / to_u64(2)\n",
            "    to_i64(half)\n",
        );
        assert_eq!(
            run_source(source).expect("run"),
            Value::I64((u64::MAX / 2) as i64)
        );
    }

    #[test]
    fn overflow_arith_checked_saturating_wrapping() {
        // checked_add overflows i8 (127 + 1) -> none.
        let none = overflow_arith(
            "checked_add",
            vec![Value::int(127, IntKind::I8), Value::int(1, IntKind::I8)],
            ArithOp::Add,
            OverflowMode::Checked,
        )
        .expect("checked_add");
        assert_eq!(none, option_value(None));
        // checked_add in range -> some(120).
        let some = overflow_arith(
            "checked_add",
            vec![Value::int(100, IntKind::I8), Value::int(20, IntKind::I8)],
            ArithOp::Add,
            OverflowMode::Checked,
        )
        .expect("checked_add");
        assert_eq!(some, option_value(Some(Value::int(120, IntKind::I8))));
        // saturating_mul clamps to u32::MAX.
        let sat = overflow_arith(
            "saturating_mul",
            vec![
                Value::int(100_000, IntKind::U32),
                Value::int(100_000, IntKind::U32),
            ],
            ArithOp::Mul,
            OverflowMode::Saturating,
        )
        .expect("saturating_mul");
        assert_eq!(sat, Value::int(4_294_967_295, IntKind::U32));
        // wrapping_add wraps u32::MAX + 1 -> 0.
        let wrap = overflow_arith(
            "wrapping_add",
            vec![
                Value::int(4_294_967_295, IntKind::U32),
                Value::int(1, IntKind::U32),
            ],
            ArithOp::Add,
            OverflowMode::Wrapping,
        )
        .expect("wrapping_add");
        assert_eq!(wrap, Value::int(0, IntKind::U32));
    }

    #[test]
    fn gcd_is_total_and_non_negative() {
        assert_eq!(gcd_i64(0, 0), 0);
        assert_eq!(gcd_i64(0, 7), 7);
        assert_eq!(gcd_i64(7, 0), 7);
        assert_eq!(gcd_i64(12, 18), 6);
        assert_eq!(gcd_i64(-12, 18), 6);
        assert_eq!(gcd_i64(-12, -18), 6);
        assert_eq!(gcd_i64(21, 14), 7);
        // Coprime -> 1.
        assert_eq!(gcd_i64(17, 4), 1);
        // `i64::MIN` must not panic; |MIN| shares the value 2^63 whose divisors
        // give a positive result with a positive operand, and `gcd(MIN, 0)`
        // wraps its own magnitude back to `i64::MIN` (documented total edge).
        assert_eq!(gcd_i64(i64::MIN, 4), 4);
        assert_eq!(gcd_i64(i64::MIN, i64::MIN), i64::MIN);
        assert_eq!(gcd_i64(i64::MIN, 0), i64::MIN);
    }

    #[test]
    fn list_sum_and_extreme_helpers() {
        // Empty list sums to 0 and has no extreme.
        assert_eq!(list_sum_values("t", vec![]).unwrap(), Value::I64(0));
        assert_eq!(list_extreme("t", vec![], false).unwrap(), None);
        assert_eq!(list_extreme("t", vec![], true).unwrap(), None);
        // i64 list.
        let ints = vec![Value::I64(3), Value::I64(9), Value::I64(1), Value::I64(7)];
        assert_eq!(list_sum_values("t", ints.clone()).unwrap(), Value::I64(20));
        assert_eq!(
            list_extreme("t", ints.clone(), false).unwrap(),
            Some(Value::I64(1))
        );
        assert_eq!(list_extreme("t", ints, true).unwrap(), Some(Value::I64(9)));
        // Wrapping i64 sum matches `+` (i64::MAX + 1 -> i64::MIN).
        let wrap = vec![Value::I64(i64::MAX), Value::I64(1)];
        assert_eq!(list_sum_values("t", wrap).unwrap(), Value::I64(i64::MIN));
        // f64 list.
        let floats = vec![Value::F64(1.5), Value::F64(0.5), Value::F64(3.0)];
        assert_eq!(
            list_sum_values("t", floats.clone()).unwrap(),
            Value::F64(5.0)
        );
        assert_eq!(
            list_extreme("t", floats.clone(), false).unwrap(),
            Some(Value::F64(0.5))
        );
        assert_eq!(
            list_extreme("t", floats, true).unwrap(),
            Some(Value::F64(3.0))
        );
        // A non-numeric element is a runtime type error.
        assert!(list_sum_values("t", vec![Value::Bool(true)]).is_err());
    }

    #[test]
    fn closure_captures_enclosing_local_by_value() {
        // `add_n` captures `n = 10` when the literal evaluates; `apply(add_n, 5)`
        // is 15 and `add_n(2)` is 12, so the canonical example returns 27.
        let source = concat!(
            "fn apply f fn(i64) -> i64 v i64 -> i64\n",
            "    f(v)\n\n",
            "fn main -> i64\n",
            "    let n i64 = 10\n",
            "    let add_n fn(i64) -> i64 = fn x i64 -> x + n\n",
            "    apply(add_n, 5) + add_n(2)\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(27));
    }

    #[test]
    fn closure_capture_is_a_snapshot_not_a_reference() {
        // Capture is by value at literal-evaluation time: mutating the enclosing
        // local after the closure is built does not change the captured value.
        let source = concat!(
            "fn main -> i64\n",
            "    let seed i64 = 7\n",
            "    let grab fn(i64) -> i64 = fn x i64 -> x + seed\n",
            "    let early i64 = grab(1)\n",
            "    seed = 1000\n",
            "    let late i64 = grab(1)\n",
            "    early + late\n",
        );
        // 8 + 8 = 16 (both reads see the snapshotted seed = 7).
        assert_eq!(run_source(source).expect("run"), Value::I64(16));
    }

    #[test]
    fn closure_returned_from_function_is_callable_later() {
        // A closure returned from `make_adder` carries its captured `base`, so it
        // stays callable at its call site: add10(5) = 15, add100(3) = 103 -> 118.
        let source = concat!(
            "fn make_adder base i64 -> fn(i64) -> i64\n",
            "    fn x i64 -> x + base\n\n",
            "fn main -> i64\n",
            "    let add10 fn(i64) -> i64 = make_adder(10)\n",
            "    let add100 fn(i64) -> i64 = make_adder(100)\n",
            "    add10(5) + add100(3)\n",
        );
        assert_eq!(run_source(source).expect("run"), Value::I64(118));
    }
}
