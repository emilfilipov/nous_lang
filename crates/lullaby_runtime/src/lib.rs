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
            Value::String(s) => Some(MapKey::Str(s.to_string())),
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
        Value::String(text) => Ok((text).into()),
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
        Value::Array(values) => Ok((values).into()),
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

/// Backing implementation of the `read_line() -> option<string>` stdin builtin,
/// shared verbatim by the AST and IR/bytecode interpreters so line-reading is
/// byte-for-byte identical across backends. Reads one line from the process's
/// standard input through the shared, buffered global `Stdin` handle (so
/// consecutive calls consume consecutive lines without dropping buffered bytes),
/// strips the trailing line terminator, and distinguishes end-of-input from a
/// blank line:
///
/// - End of input (no bytes read) yields `none`.
/// - A line yields `some(text)` with the trailing `\n` removed, and a preceding
///   `\r` also removed so Windows CRLF input round-trips like LF input.
/// - A blank input line yields `some("")`, keeping EOF and an empty line
///   distinct.
///
/// A genuine read failure (for example, non-UTF-8 bytes on stdin) is the
/// resource error `L0419`, the standard-stream I/O family.
pub fn read_stdin_line() -> Result<Value, RuntimeError> {
    use std::io::BufRead;
    let mut buffer = String::new();
    let read = std::io::stdin()
        .lock()
        .read_line(&mut buffer)
        .map_err(|error| {
            RuntimeError::resource(
                "L0419",
                format!("failed to read a line from stdin: {error}"),
            )
        })?;
    if read == 0 {
        return Ok(option_value(None));
    }
    if buffer.ends_with('\n') {
        buffer.pop();
        if buffer.ends_with('\r') {
            buffer.pop();
        }
    }
    Ok(option_value(Some(Value::String(buffer.into()))))
}

/// Backing implementation of the `read_all() -> string` stdin builtin, shared by
/// both interpreters. Reads the whole of standard input to EOF into one `string`
/// (empty when stdin is empty or already closed). A read failure (for example,
/// non-UTF-8 bytes on stdin) is the resource error `L0419`.
pub fn read_stdin_all() -> Result<Value, RuntimeError> {
    use std::io::Read;
    let mut buffer = String::new();
    std::io::stdin()
        .lock()
        .read_to_string(&mut buffer)
        .map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to read stdin: {error}"))
        })?;
    Ok(Value::String(buffer.into()))
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
/// Read one element from an indexable value (`string` char or `array` element)
/// by **borrowing** the container and cloning only the element. Used on the
/// `a[i]` hot path so a bare-variable index does not clone the whole container.
fn index_into(container: &Value, index: i64) -> Result<Value, RuntimeError> {
    match container {
        Value::String(text) => {
            if index < 0 {
                return Err(RuntimeError::new(
                    "L0413",
                    format!("string index `{index}` is out of bounds"),
                ));
            }
            text.chars()
                .nth(index as usize)
                .map(Value::Char)
                .ok_or_else(|| {
                    RuntimeError::new("L0413", format!("string index `{index}` is out of bounds"))
                })
        }
        Value::Array(values) => {
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
        _ => Err(RuntimeError::new("L0412", "index target is not an array")),
    }
}

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
        return Ok(Value::Array((Vec::new()).into()));
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
                    Value::String(s) => strs.push((s).into()),
                    other => return Err(mixed_numeric_list_error(name, &other)),
                }
            }
            strs.sort();
            Ok(Value::Array(
                strs.into_iter().map(|s| Value::String(s.into())).collect(),
            ))
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
    result_value(Err(Value::String((error.to_string()).into())))
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
        None => return result_value(Err(Value::String(("invalid url".to_string()).into()))),
    };
    if scheme.eq_ignore_ascii_case("https") {
        return result_value(Err(Value::String(
            ("https not supported".to_string()).into(),
        )));
    }
    if !scheme.eq_ignore_ascii_case("http") {
        return result_value(Err(Value::String(
            format!("unsupported scheme `{scheme}`").into(),
        )));
    }

    // Split `host[:port]` from the path (default `/`).
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return result_value(Err(Value::String(("missing host".to_string()).into())));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port_text)) => match port_text.parse::<u16>() {
            Ok(port) => (host, port),
            Err(_) => {
                return result_value(Err(Value::String(
                    format!("invalid port `{port_text}`").into(),
                )));
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
                ("malformed response: no header terminator".to_string()).into(),
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
        Some(code) if (200..400).contains(&code) => {
            result_value(Ok(Value::String((body_text).into())))
        }
        Some(code) => {
            let first_line = body_text.lines().next().unwrap_or("");
            result_value(Err(Value::String(
                format!("http {code}: {first_line}").into(),
            )))
        }
        None => result_value(Err(Value::String(
            format!("malformed status line `{status_line}`").into(),
        ))),
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
        Stmt::ForEach { iterable, body, .. } => {
            collect_closures_in_expr(iterable, table);
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
        ExprKind::Conditional {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_closures_in_expr(cond, table);
            collect_closures_in_expr(then_branch, table);
            collect_closures_in_expr(else_branch, table);
        }
        ExprKind::In { value, collection } => {
            collect_closures_in_expr(value, table);
            collect_closures_in_expr(collection, table);
        }
        ExprKind::Slice { target, start, end } => {
            collect_closures_in_expr(target, table);
            if let Some(start) = start {
                collect_closures_in_expr(start, table);
            }
            if let Some(end) = end {
                collect_closures_in_expr(end, table);
            }
        }
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
            "read_line" => Self::builtin_read_line(args),
            "read_all" => Self::builtin_read_all(args),
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
            "array_fill" => Self::builtin_array_fill(args),
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
            // `words`/`count` yield to a user-defined function of the same name, so
            // adding these common stdlib names never breaks existing user code.
            "words" if !self.functions.contains_key("words") => Self::builtin_words(args),
            "count" if !self.functions.contains_key("count") => Self::builtin_count(args),
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
}

#[path = "runtime_builtins.rs"]
mod builtins;

#[path = "runtime_eval.rs"]
mod eval;

enum Control {
    Return(Value),
    Break,
    Continue,
    Value(Value),
}

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
        | Stmt::ForEach { span, .. }
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
        ExprKind::Conditional {
            cond,
            then_branch,
            else_branch,
        } => {
            expr_mentions_var(cond, name)
                || expr_mentions_var(then_branch, name)
                || expr_mentions_var(else_branch, name)
        }
        ExprKind::In { value, collection } => {
            expr_mentions_var(value, name) || expr_mentions_var(collection, name)
        }
        ExprKind::Slice { target, start, end } => {
            expr_mentions_var(target, name)
                || start
                    .as_deref()
                    .is_some_and(|start| expr_mentions_var(start, name))
                || end
                    .as_deref()
                    .is_some_and(|end| expr_mentions_var(end, name))
        }
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
        Stmt::ForEach { iterable, body, .. } => {
            expr_mentions_var(iterable, name) || body.iter().any(|s| stmt_mentions_var(s, name))
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

    /// Update the loop variable's binding in the innermost scope in place. The
    /// range-`for` lowering calls this each iteration with the loop-variable scope
    /// innermost (the body scope has been popped), so it never allocates or clones
    /// the name — the hot-path replacement for a per-iteration `define`.
    fn set_loop_var(&mut self, name: &str, value: Value) {
        let scope = self.scopes.last_mut().expect("env always has a scope");
        for (existing, slot) in scope.iter_mut() {
            if existing == name {
                *slot = value;
                return;
            }
        }
        scope.push((name.to_string(), value));
    }

    /// Borrow the nearest binding of `name` mutably, for in-place mutation of an
    /// element/field (`a[i] = v`, `s.field = v`) without cloning the whole
    /// container and writing it back. Resolves nearest-first like `assign`.
    fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        for scope in self.scopes.iter_mut().rev() {
            for (existing, slot) in scope.iter_mut() {
                if existing == name {
                    return Some(slot);
                }
            }
        }
        None
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
