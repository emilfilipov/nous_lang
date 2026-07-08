//! WebAssembly backend — the scalar subset, the linear-memory step, and heap
//! types (strings and fixed aggregates) laid out in linear memory.
//!
//! This module compiles the typed IR (`IrModule`) directly to a binary `.wasm`
//! module using only the Rust standard library: it implements the WASM binary
//! encoding (magic, version, Type/Import/Function/Memory/Global/Export/Code/Data
//! sections, LEB128, and the stack-machine opcodes it needs) from scratch.
//!
//! Scalar subset: functions whose parameter and return types are all scalars in
//! {`i64`, `f32`, `f64`, `bool`, `char`, `byte`} (plus the fixed-width integer
//! kinds) compile to WASM. `i64` maps to wasm `i64`, `f32` to `f32`, `f64` to
//! `f64`, and `bool`/`char`/`byte` to `i32`. `void` return means no result.
//! Supported bodies: integer/float/bool literals, variables (params + `let`
//! locals), arithmetic (`+ - * /`; signed integer `/` uses a guarded `i64.div_s`
//! that traps on a zero divisor but wraps `i64::MIN / -1` to `i64::MIN` to match
//! the interpreters;
//! `f32`/`f64` use the single/double SSE-equivalent `f32.*`/`f64.*` ops, so f32
//! stays single precision), comparisons (float compares are IEEE-754 NaN-aware),
//! `and`/`or`/`not`, `if`/`elif`/`else`, `while`, `loop` with `break`/`continue`,
//! range `for` (lowered to a loop), `return`, calls to other compiled functions
//! (including recursion), the float conversions `to_f32` (`f32.demote_f64`) and
//! `to_f64` (`f64.promote_f32`), and the host log builtin
//! `wasm_log(x i64) -> void`.
//!
//! Heap types (this increment): `string`, `struct`, and fixed `array` values are
//! **pointers** (`i32`) into linear memory.
//! - A `string` is a pointer to `[len: i32 (char count)][utf8 bytes]`. String
//!   literals are laid out once in the Data section (their pointer is a constant
//!   static offset); `len(s)` loads the leading `i32`.
//! - A `struct` is a pointer to a contiguous run of 8-byte slots, one per field in
//!   declared order. Positional construction (a call whose name is the struct)
//!   `__alloc`s the run and stores each field; `.field` reads a slot; `p.field =
//!   v` writes a slot.
//! - A fixed `array` literal is a pointer to `[len: i32][elem slots...]` with one
//!   8-byte slot per element. `a[i]` loads a slot (WASM traps on out-of-bounds
//!   memory access); `a[i] = v` stores one; `len(a)` loads the leading `i32`.
//!   Element/field values may themselves be scalars or pointers (nested
//!   strings/structs/arrays), stored by their WASM slot type.
//! - An `enum` value with SCALAR payloads is a pointer to
//!   `[tag: i32 (padded to 8)][slot0][slot1]...]`: an `i32` discriminant (the
//!   variant's index in declaration order, matching the interpreters) plus one
//!   8-byte slot per payload position, sized for the widest variant. Construction
//!   (`some(x)`/`none`/`ok(x)`/`err(e)` and user `Variant(payload...)`) `__alloc`s
//!   the record, stores the tag, and stores each payload; `match` loads the tag,
//!   dispatches on it with an `if`/`else` chain (a `Wildcard` arm is the final
//!   `else`), binds each arm's payload slots into locals, and yields the arm value
//!   (a value match) or nothing (a void match). This covers the built-in
//!   `option<T>`/`result<T, E>` when `T`/`E` are scalar, and user enums whose every
//!   variant payload is scalar.
//!
//! Aggregates cross function boundaries as values. A `struct`, fixed `array`, or
//! supported `enum` may be a parameter, a return value, or a call argument: at the
//! WASM level the `i32` pointer is passed/returned directly. To preserve Lullaby
//! value semantics (an aggregate passed by value is an independent snapshot — a
//! callee mutating its parameter must not change the caller's copy), a MUTABLE
//! aggregate argument is **deep-copied at the call site** before the call: a fresh
//! record is `__alloc`'d and every word is copied, recursing into nested mutable
//! aggregate fields/elements, exactly mirroring the interpreters' recursive
//! `Value::clone`. A `string` argument is NOT copied — strings are immutable, so
//! sharing the pointer is already value-equivalent. A returned aggregate is the
//! callee's own fresh record, so no extra copy is needed. Enum payloads are always
//! scalar (see `enum_layout`), so an enum copy is a flat word copy.
//!
//! A function that uses a different builtin, `list`/`map`, an enum with a HEAP
//! payload (`string`/`list`/`array`/`map` — notably `result<i64, string>`), or any
//! type still outside this set is SKIPPED with a reason (it still runs on the
//! interpreters).
//!
//! Linear-memory infrastructure: the module exports a `"memory"` (min 1 page),
//! imports the host functions `env.log_i64 (func (param i64))` (surfaced as
//! `wasm_log`), `env.console_log (func (param i32 i32))` (surfaced as
//! `console_log`), and `env.dom_set_text (func (param i32 i32 i32 i32))`
//! (surfaced as `dom_set_text`) — the JS/DOM browser-host interop layer —
//! declares a mutable `i32` global bump pointer, writes a Data section seeding the
//! reserved region and the string-literal pool, and emits an internal
//! `__alloc(size i32) -> i32` bump-allocator helper used to build structs/arrays
//! at runtime. Imported functions occupy the LOW function indices, so every
//! internally-defined function's index is shifted by the import count; call
//! targets and exports are fixed up accordingly. The string host imports take a
//! `(ptr, len)` pair per string decoded out of `memory`. Enums with heap payloads
//! and `list`/`map` remain deferred.

use std::collections::HashMap;

use lullaby_parser::{BinaryOp, TypeRef, UnaryOp};
use lullaby_runtime::IntKind;

use crate::{IrEnumDef, IrExpr, IrExprKind, IrFunction, IrModule, IrStmt, IrStructDef};

/// The fixed-width integer kind named by a Lullaby type name (`i8`/`u32`/…), or
/// `None` for `i64` and every non-fixed-width type. Like the native backend, the
/// WASM backend keeps a fixed-width value in a 64-bit `i64` local holding the
/// same normalized cell the interpreters use (see [`IntKind::normalize`]): signed
/// kinds sign-extended, unsigned kinds zero-extended, 64-bit kinds filling the
/// whole cell.
fn fixed_int_kind(type_name: &str) -> Option<IntKind> {
    match type_name {
        "i8" => Some(IntKind::I8),
        "i16" => Some(IntKind::I16),
        "i32" => Some(IntKind::I32),
        "u8" => Some(IntKind::U8),
        "u16" => Some(IntKind::U16),
        "u32" => Some(IntKind::U32),
        "u64" => Some(IntKind::U64),
        "isize" => Some(IntKind::Isize),
        "usize" => Some(IntKind::Usize),
        _ => None,
    }
}

/// The target [`IntKind`] of a `to_<T>` conversion builtin (`to_i8`/`to_u32`/…),
/// or `None` for any other name. `to_i64` and same-width conversions are handled
/// separately (they are the identity on the normalized cell).
fn to_int_conversion_kind(name: &str) -> Option<IntKind> {
    match name {
        "to_i8" => Some(IntKind::I8),
        "to_i16" => Some(IntKind::I16),
        "to_i32" => Some(IntKind::I32),
        "to_u8" => Some(IntKind::U8),
        "to_u16" => Some(IntKind::U16),
        "to_u32" => Some(IntKind::U32),
        "to_u64" => Some(IntKind::U64),
        "to_isize" => Some(IntKind::Isize),
        "to_usize" => Some(IntKind::Usize),
        _ => None,
    }
}

/// Emit the WASM instructions that normalize the `i64` value on top of the stack
/// into `kind`'s width, matching [`IntKind::normalize`] exactly. Signed 8/16/32
/// kinds use the dedicated sign-extension opcodes; unsigned 8/16/32 kinds mask
/// to the width; the 64-bit kinds (`u64`/`usize`/`isize`) already fill the cell,
/// so normalization is a no-op.
fn emit_normalize_i64(kind: IntKind, out: &mut Vec<u8>) {
    match kind {
        // i64.extend8_s / extend16_s / extend32_s: sign-extend the low bits.
        IntKind::I8 => out.push(0xc2),
        IntKind::I16 => out.push(0xc3),
        IntKind::I32 => out.push(0xc4),
        // Unsigned 8/16/32: mask to the width (zero-extend).
        IntKind::U8 | IntKind::U16 | IntKind::U32 => {
            let mask: i64 = match kind {
                IntKind::U8 => 0xff,
                IntKind::U16 => 0xffff,
                IntKind::U32 => 0xffff_ffff,
                _ => unreachable!(),
            };
            out.push(0x42); // i64.const mask
            write_sleb(out, mask);
            out.push(0x83); // i64.and
        }
        // 64-bit kinds fill the whole cell: normalization is the identity.
        IntKind::U64 | IntKind::Isize | IntKind::Usize => {}
    }
}

/// The Lullaby builtin that lowers to the imported host log function.
const WASM_LOG: &str = "wasm_log";

/// The Lullaby builtin that lowers to the imported host `env.console_log`.
const CONSOLE_LOG: &str = "console_log";

/// The Lullaby builtin that lowers to the imported host `env.dom_set_text`.
const DOM_SET_TEXT: &str = "dom_set_text";

/// Number of imported functions. They occupy WASM function indices `0..IMPORTS`,
/// so every internally-defined function's index is offset by this amount. The
/// imports are, in order: `env.log_i64`, `env.console_log`, `env.dom_set_text`.
const IMPORT_FUNC_COUNT: u32 = 3;

/// WASM function index of the imported `env.log_i64` (the first import).
/// Internal functions are numbered from `IMPORT_FUNC_COUNT` up.
const LOG_I64_FUNC_INDEX: u32 = 0;

/// WASM function index of the imported `env.console_log(ptr i32, len i32)`
/// (the second import) — the JS/DOM console host primitive.
const CONSOLE_LOG_FUNC_INDEX: u32 = 1;

/// WASM function index of the imported
/// `env.dom_set_text(id_ptr i32, id_len i32, text_ptr i32, text_len i32)`
/// (the third import) — the DOM-write host primitive.
const DOM_SET_TEXT_FUNC_INDEX: u32 = 2;

/// The first byte offset in linear memory reserved before any user data. Bytes
/// below this are a reserved region (seeded by the Data section) so a pointer is
/// never null (0) and low addresses stay reserved. String literals are laid out
/// starting at this offset; the bump allocator's global is initialized past both
/// the reserved region and the whole string-literal pool.
const RESERVED_BASE: i32 = 16;

/// Bytes per aggregate slot: struct fields and array elements each occupy one
/// 8-byte slot regardless of their WASM value type. Uniform 8-byte slots keep the
/// layout naturally aligned for `i64`/`f64` loads and stores and make offset math
/// a simple `slot_index * 8`.
const SLOT_SIZE: i32 = 8;

/// Bytes for the leading `i32` length header shared by strings and arrays.
const LEN_HEADER: i32 = 4;

/// The builtin that reads a string's or array's length header.
const LEN_BUILTIN: &str = "len";

// -- Growable list layout ----------------------------------------------------
//
// A growable `list<T>` (scalar `T`) is an `i32` pointer to a header
// `[len: i32][cap: i32][elem slots...]`: the current element count, the allocated
// capacity, then `cap` 8-byte element slots (uniform `SLOT_SIZE` slots like
// struct/array elements, so a scalar element is naturally aligned and the offset
// of element `i` is a simple `LIST_DATA_OFF + i * SLOT_SIZE`). `len` shares the
// leading `i32` offset with strings/arrays, so `len(l)` reuses the array path.
//
// Value semantics: Lullaby lists are value-semantic (`l = push(l, x)` returns a
// NEW list). Every mutating op (`push`/`set`/`pop`) deep-copies the source list
// first and mutates the fresh copy, and a list crossing a call boundary is
// deep-copied like any other mutable aggregate — so mutating one binding can
// never be observed through another. The bump allocator never reclaims, so a
// list that grows (or is copied) orphans its old block, exactly like the existing
// string/struct/array heap growth.

/// Byte offset of a list's `len` header (element count). Shares offset 0 with the
/// string/array length header so `len(l)` reuses the array length path.
const LIST_LEN_OFF: i32 = 0;

/// Byte offset of a list's `cap` header (allocated capacity, in elements).
const LIST_CAP_OFF: i32 = 4;

/// Byte offset of a list's first element slot: past the two `i32` headers.
const LIST_DATA_OFF: i32 = 8;

/// Initial capacity a `list_new()` header (and the first growth of an empty list)
/// allocates, so a handful of pushes do not each trigger a realloc.
const LIST_INITIAL_CAP: i32 = 4;

/// Builtins that construct or read growable lists, matched by name in call
/// lowering (the arity/element type is validated there against the IR types).
const LIST_NEW_BUILTIN: &str = "list_new";
const LIST_PUSH_BUILTIN: &str = "push";
const LIST_GET_BUILTIN: &str = "get";
const LIST_SET_BUILTIN: &str = "set";
const LIST_POP_BUILTIN: &str = "pop";

/// Byte offset of an enum's first payload slot. The leading discriminant tag is
/// an `i32` at offset 0; the first payload slot starts at `SLOT_SIZE` so every
/// 8-byte payload slot stays naturally aligned for `i64`/`f64` loads and stores,
/// exactly like struct/array slots.
const ENUM_PAYLOAD_BASE: i32 = SLOT_SIZE;

// -- Enum layout -------------------------------------------------------------

/// The linear-memory layout of an enum-typed value: an ordered variant table
/// (the discriminant index of a variant is its position here, matching the
/// interpreters' declaration order) plus the payload slot count (the maximum
/// payload arity across variants). An enum value is a pointer to
/// `[tag i32 (padded to SLOT_SIZE)][slot0][slot1]...]`, one 8-byte slot per
/// payload position; a variant stores its payload into the leading slots and
/// leaves the rest unused.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EnumLayout {
    /// Variants in discriminant order; each is `(name, payload scalar types)`.
    variants: Vec<(String, Vec<TypeRef>)>,
    /// Maximum payload arity across variants — the number of payload slots.
    slot_count: usize,
}

impl EnumLayout {
    /// Total byte size: the padded tag plus one 8-byte slot per payload position.
    fn size_bytes(&self) -> i32 {
        ENUM_PAYLOAD_BASE + self.slot_count as i32 * SLOT_SIZE
    }

    /// The discriminant index (tag) of a variant by name.
    fn tag_of(&self, variant: &str) -> Option<i32> {
        self.variants
            .iter()
            .position(|(name, _)| name == variant)
            .map(|position| position as i32)
    }

    /// The payload types of a variant by name.
    fn payload_of(&self, variant: &str) -> Option<&[TypeRef]> {
        self.variants
            .iter()
            .find(|(name, _)| name == variant)
            .map(|(_, payload)| payload.as_slice())
    }
}

/// Resolve the [`EnumLayout`] of an enum-typed `TypeRef`, or `None` if `ty` is
/// not an enum the WASM backend can lay out. The supported enums are the
/// built-in `option<T>` (variants `some(T)`, `none`) and `result<T, E>`
/// (variants `ok(T)`, `err(E)`) with scalar `T`/`E`, and any user enum whose
/// every variant payload is a scalar. An enum with a heap payload
/// (`string`/`list`/`array`/`map`) — notably `result<i64, string>` — is NOT
/// supported and yields `None` so the enclosing function is skipped.
fn enum_layout(ty: &TypeRef, enums: &HashMap<String, IrEnumDef>) -> Option<EnumLayout> {
    // Built-in `option<T>`: variants `some(T)`, `none`, in that order. `?` bails
    // (unsupported enum) when `T` is not a scalar (e.g. a heap payload).
    if let Some(inner) = ty.option_element() {
        scalar_val_type(&inner)?;
        return Some(build_layout(vec![
            ("some".to_string(), vec![inner]),
            ("none".to_string(), Vec::new()),
        ]));
    }
    // Built-in `result<T, E>`: variants `ok(T)`, `err(E)`, in that order. Both
    // payload types must be scalar; `?` bails on a heap payload.
    if let Some((ok, err)) = ty.result_args() {
        scalar_val_type(&ok)?;
        scalar_val_type(&err)?;
        return Some(build_layout(vec![
            ("ok".to_string(), vec![ok]),
            ("err".to_string(), vec![err]),
        ]));
    }
    // A user enum: every variant payload must be a scalar (`?` bails otherwise).
    let def = enums.get(&ty.name)?;
    let mut variants = Vec::with_capacity(def.variants.len());
    for variant in &def.variants {
        for payload_ty in &variant.payload {
            scalar_val_type(payload_ty)?;
        }
        variants.push((variant.name.clone(), variant.payload.clone()));
    }
    Some(build_layout(variants))
}

/// Build an [`EnumLayout`] from an ordered variant table, computing the payload
/// slot count as the maximum payload arity across variants.
fn build_layout(variants: Vec<(String, Vec<TypeRef>)>) -> EnumLayout {
    let slot_count = variants
        .iter()
        .map(|(_, payload)| payload.len())
        .max()
        .unwrap_or(0);
    EnumLayout {
        variants,
        slot_count,
    }
}

/// A compiled `.wasm` module plus the record of which functions compiled and
/// which were skipped (with a reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmArtifact {
    /// The binary `.wasm` module (starts with `\0asm` + version 1).
    pub bytes: Vec<u8>,
    /// Names of functions that compiled to WASM, in module order.
    pub compiled: Vec<String>,
    /// Functions skipped for WASM, each with a human-readable reason.
    pub skipped: Vec<SkippedFunction>,
}

/// A function that was not eligible for the WASM scalar subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFunction {
    pub name: String,
    pub reason: String,
}

/// A failure while emitting the WASM module. Currently the only hard error is
/// "no functions were eligible", which the CLI surfaces as diagnostic `L0338`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmError {
    pub code: &'static str,
    pub message: String,
    /// Functions skipped, so the CLI can still report why nothing compiled.
    pub skipped: Vec<SkippedFunction>,
}

/// WASM value types used by the scalar subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WasmValType {
    I32,
    I64,
    F32,
    F64,
}

impl WasmValType {
    /// The binary encoding byte for this value type.
    fn byte(self) -> u8 {
        match self {
            WasmValType::I32 => 0x7f,
            WasmValType::I64 => 0x7e,
            WasmValType::F32 => 0x7d,
            WasmValType::F64 => 0x7c,
        }
    }
}

/// Map a Lullaby scalar `TypeRef` to a WASM value type, or `None` if the type is
/// not in the scalar subset.
fn scalar_val_type(ty: &TypeRef) -> Option<WasmValType> {
    match ty.name.as_str() {
        "i64" => Some(WasmValType::I64),
        // `f32` maps to WASM's native single-precision `f32` type, so every
        // arithmetic op rounds to single precision (`f32.add`/…) and stays
        // bit-identical to the interpreter's real `f32`, exactly like the native
        // backend keeps f32 in a `single`.
        "f32" => Some(WasmValType::F32),
        "f64" => Some(WasmValType::F64),
        "bool" | "char" | "byte" => Some(WasmValType::I32),
        // A fixed-width integer (`i8`…`usize`) is stored as its normalized `i64`
        // cell, exactly like the interpreters and the native backend.
        name if fixed_int_kind(name).is_some() => Some(WasmValType::I64),
        _ => None,
    }
}

/// Whether a type is a heap/aggregate type represented as an `i32` pointer into
/// linear memory: `string`, a named struct (resolved via `structs`), a fixed
/// `array<T>` whose element is itself a supported slot type, or a supported enum
/// (`option`/`result`/user enum with scalar payloads — see [`enum_layout`]).
fn is_pointer_type(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
) -> bool {
    if ty.name == "string" {
        return true;
    }
    if structs.contains_key(&ty.name) {
        return true;
    }
    if enum_layout(ty, enums).is_some() {
        return true;
    }
    if let Some(elem) = ty.array_element() {
        return slot_val_type(&elem, structs, enums).is_some();
    }
    if supported_list_element(ty).is_some() {
        return true;
    }
    false
}

/// The scalar element type of a supported growable `list<T>`, or `None` if `ty` is
/// not a list or its element is not a scalar. Lists of heap elements
/// (`list<string>`/`list<struct>`/`list<list<…>>`/`list<map<…>>`) are DEFERRED —
/// the WASM backend only lays out scalar-element lists this increment — so such a
/// list is unsupported and its enclosing function is skipped (still runs on the
/// interpreters).
fn supported_list_element(ty: &TypeRef) -> Option<TypeRef> {
    let elem = ty.list_element()?;
    scalar_val_type(&elem)?;
    Some(elem)
}

/// Whether `ty` is a MUTABLE aggregate whose value semantics require a
/// snapshotting deep copy when it crosses a call boundary: a named `struct`, a
/// fixed `array`, or a supported `enum` (`option`/`result`/user enum with scalar
/// payloads). A `string` is a pointer too but is immutable in Lullaby, so sharing
/// its pointer is semantically identical to the interpreters' `Value::String`
/// clone (no callee can mutate it); scalars need no copy at all. This is the set
/// of argument types the call lowering deep-copies (see [`emit_deep_copy`]) so a
/// callee mutating its parameter cannot alter the caller's copy.
fn is_mutable_aggregate(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
) -> bool {
    if ty.name == "string" {
        return false;
    }
    if structs.contains_key(&ty.name) {
        return true;
    }
    if enum_layout(ty, enums).is_some() {
        return true;
    }
    if let Some(elem) = ty.array_element() {
        return slot_val_type(&elem, structs, enums).is_some();
    }
    // A scalar-element growable `list<T>` is a mutable aggregate: it is deep-copied
    // when it crosses a call boundary so a callee mutating its parameter cannot
    // alter the caller's list, exactly like the interpreters' `Value::clone`.
    if supported_list_element(ty).is_some() {
        return true;
    }
    false
}

/// The WASM value type an aggregate slot (struct field / array element / enum
/// payload) holds: the scalar type for a scalar, or `i32` for any pointer
/// (string/struct/array/enum). `None` for a type the WASM backend cannot lay out
/// (e.g. `list`, `map`, or an enum with a heap payload), which makes the
/// enclosing aggregate ineligible.
fn slot_val_type(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
) -> Option<WasmValType> {
    if let Some(vt) = scalar_val_type(ty) {
        return Some(vt);
    }
    if is_pointer_type(ty, structs, enums) {
        return Some(WasmValType::I32);
    }
    None
}

/// The WASM value type used for a first-class value of `ty`: a scalar's own type,
/// or `i32` for a pointer (string/struct/array/enum). `None` for anything else.
fn value_val_type(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
) -> Option<WasmValType> {
    slot_val_type(ty, structs, enums)
}

/// The result type for a function: empty for `void`, else one value type.
/// `Err(())` means the return type is not a supported WASM value type.
fn return_val_type(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
) -> Result<Option<WasmValType>, ()> {
    if ty.is_void() {
        return Ok(None);
    }
    value_val_type(ty, structs, enums).map(Some).ok_or(())
}

/// A resolved local: its WASM index and value type.
#[derive(Debug, Clone, Copy)]
struct Local {
    index: u32,
    ty: WasmValType,
}

// -- Public entry point ------------------------------------------------------

/// Emit a binary `.wasm` module for the scalar-subset functions of `module`.
///
/// Every top-level function is examined: an eligible one is lowered and exported
/// by its Lullaby name; an ineligible one is recorded in `skipped` with a reason.
/// If no function is eligible, this returns `Err(WasmError)` with code `L0338`.
pub fn emit_wasm_module(module: &IrModule) -> Result<WasmArtifact, WasmError> {
    // A struct name -> ordered `(field, type)` map, used everywhere we classify a
    // type (pointer vs scalar) or compute a struct's field layout.
    let structs = struct_table(&module.structs);
    // A user-enum name -> its IR definition, used to classify an enum type and to
    // resolve its variant table / payload layout (see `enum_layout`). Built-in
    // `option`/`result` are resolved structurally from the type spelling, so they
    // are not in this map.
    let enums = enum_table(&module.enums);

    // First pass: decide signature eligibility and assign WASM function indices
    // to the functions we will compile. Calls between compiled functions resolve
    // against this index map.
    let mut compiled_names: Vec<String> = Vec::new();
    let mut skipped: Vec<SkippedFunction> = Vec::new();
    let mut func_index: HashMap<String, u32> = HashMap::new();
    let mut eligible: Vec<&IrFunction> = Vec::new();

    for function in &module.functions {
        match eligibility(function, &structs, &enums) {
            Ok(()) => {
                // Imports occupy the low indices, so internal functions are
                // numbered from `IMPORT_FUNC_COUNT` up.
                let index = IMPORT_FUNC_COUNT + eligible.len() as u32;
                func_index.insert(function.name.clone(), index);
                eligible.push(function);
                compiled_names.push(function.name.clone());
            }
            Err(reason) => skipped.push(SkippedFunction {
                name: function.name.clone(),
                reason,
            }),
        }
    }

    if eligible.is_empty() {
        return Err(WasmError {
            code: "L0338",
            message: "no functions were eligible for the WebAssembly scalar subset".to_string(),
            skipped,
        });
    }

    // The internal `__alloc` helper is appended after the user functions in
    // `encode_module`, so its WASM function index is fixed here. Record it so
    // aggregate construction can `call` it.
    func_index.insert(
        ALLOC_HELPER_NAME.to_string(),
        IMPORT_FUNC_COUNT + eligible.len() as u32,
    );

    // Second pass: lower each eligible function's body into a shared string-literal
    // pool (so identical literals share one static offset). A lowering failure (a
    // construct we cannot compile) demotes that function to skipped. Because that
    // changes index assignment, we re-run the whole emission over the reduced set.
    // This converges quickly (each pass removes at least one function).
    let mut pool = StringPool::new();
    let mut lowered: Vec<LoweredFunction> = Vec::new();
    for function in &eligible {
        match lower_function(function, &func_index, &structs, &enums, &mut pool) {
            Ok(l) => lowered.push(l),
            Err(reason) => {
                let demoted = SkippedFunction {
                    name: function.name.clone(),
                    reason,
                };
                let mut reduced = module.clone();
                reduced.functions.retain(|f| f.name != demoted.name);
                return match emit_wasm_module(&reduced) {
                    Ok(mut artifact) => {
                        artifact.compiled.retain(|n| n != &demoted.name);
                        merge_skip(&mut artifact.skipped, demoted);
                        for s in &skipped {
                            merge_skip(&mut artifact.skipped, s.clone());
                        }
                        Ok(artifact)
                    }
                    Err(mut err) => {
                        merge_skip(&mut err.skipped, demoted);
                        for s in &skipped {
                            merge_skip(&mut err.skipped, s.clone());
                        }
                        Err(err)
                    }
                };
            }
        }
    }

    let bytes = encode_module(&lowered, &pool);
    Ok(WasmArtifact {
        bytes,
        compiled: compiled_names,
        skipped,
    })
}

/// Build the struct name -> ordered `(field, type)` map from the IR struct defs.
fn struct_table(defs: &[IrStructDef]) -> HashMap<String, Vec<(String, TypeRef)>> {
    defs.iter()
        .map(|d| (d.name.clone(), d.fields.clone()))
        .collect()
}

/// Build the user-enum name -> IR definition map from the IR enum defs. Built-in
/// `option`/`result` are not user enums and are resolved structurally in
/// [`enum_layout`], so they never appear here.
fn enum_table(defs: &[IrEnumDef]) -> HashMap<String, IrEnumDef> {
    defs.iter().map(|d| (d.name.clone(), d.clone())).collect()
}

/// The static data pool for string literals. Each distinct literal is laid out
/// once as `[len: i32 char-count][utf8 bytes]` starting at `RESERVED_BASE`; the
/// value of the literal is the byte offset of its length header.
struct StringPool {
    /// Literal text -> its pointer (offset of the length header).
    offsets: HashMap<String, i32>,
    /// The concatenated pool bytes, laid out from `RESERVED_BASE` upward.
    bytes: Vec<u8>,
}

impl StringPool {
    fn new() -> Self {
        Self {
            offsets: HashMap::new(),
            bytes: Vec::new(),
        }
    }

    /// Intern a literal, returning its pointer (a constant static offset).
    fn intern(&mut self, text: &str) -> i32 {
        if let Some(&offset) = self.offsets.get(text) {
            return offset;
        }
        let offset = RESERVED_BASE + self.bytes.len() as i32;
        let char_count = text.chars().count() as i32;
        self.bytes.extend_from_slice(&char_count.to_le_bytes());
        self.bytes.extend_from_slice(text.as_bytes());
        self.offsets.insert(text.to_string(), offset);
        offset
    }

    /// The byte offset one past the end of the pool: the first address the bump
    /// allocator may hand out (its global's initial value).
    fn heap_base(&self) -> i32 {
        RESERVED_BASE + self.bytes.len() as i32
    }
}

/// Append a skip record unless one with that name is already present.
fn merge_skip(skips: &mut Vec<SkippedFunction>, skip: SkippedFunction) {
    if !skips.iter().any(|s| s.name == skip.name) {
        skips.push(skip);
    }
}

// -- Eligibility -------------------------------------------------------------

/// Check whether a function's signature is entirely in the supported WASM value
/// set: scalars, or pointer types (`string`, struct, fixed `array`).
fn eligibility(
    function: &IrFunction,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
) -> Result<(), String> {
    for param in &function.params {
        if value_val_type(&param.ty, structs, enums).is_none() {
            return Err(format!(
                "parameter `{}` has unsupported type `{}`",
                param.name, param.ty.name
            ));
        }
    }
    if return_val_type(&function.return_type, structs, enums).is_err() {
        return Err(format!(
            "return type `{}` is not a supported WASM value type",
            function.return_type.name
        ));
    }
    Ok(())
}

// -- Lowering ----------------------------------------------------------------

/// A function lowered to WASM: its signature value types, extra (non-parameter)
/// local declarations, and the encoded body instruction bytes (without the final
/// `end`).
#[derive(Clone)]
struct LoweredFunction {
    name: String,
    params: Vec<WasmValType>,
    result: Option<WasmValType>,
    /// Locals beyond the parameters, in index order.
    extra_locals: Vec<WasmValType>,
    body: Vec<u8>,
}

/// Per-function lowering state.
struct LowerCtx<'a> {
    /// name -> (index, type) for every param and `let`/`for` local.
    locals: HashMap<String, Local>,
    /// Value types of the extra (non-param) locals, in index order.
    extra_locals: Vec<WasmValType>,
    /// Number of parameters (locals 0..param_count are params).
    param_count: u32,
    /// Function-name -> WASM function index, for calls.
    func_index: &'a HashMap<String, u32>,
    /// Struct name -> ordered `(field, type)` fields, for aggregate layout.
    structs: &'a HashMap<String, Vec<(String, TypeRef)>>,
    /// User-enum name -> IR definition, for enum classification and layout.
    enums: &'a HashMap<String, IrEnumDef>,
    /// name -> IR type for every param and `let`/`for` local, so path assignment
    /// can walk aggregate field/element types (the `Local` map only keeps the
    /// WASM value type).
    local_ir_types: HashMap<String, TypeRef>,
    /// Shared static string-literal pool (assigns constant offsets).
    pool: &'a mut StringPool,
}

impl<'a> LowerCtx<'a> {
    fn new(
        function: &IrFunction,
        func_index: &'a HashMap<String, u32>,
        structs: &'a HashMap<String, Vec<(String, TypeRef)>>,
        enums: &'a HashMap<String, IrEnumDef>,
        pool: &'a mut StringPool,
    ) -> Result<Self, String> {
        let mut locals = HashMap::new();
        let mut local_ir_types = HashMap::new();
        for (i, param) in function.params.iter().enumerate() {
            let ty = value_val_type(&param.ty, structs, enums)
                .ok_or_else(|| format!("parameter `{}` has an unsupported type", param.name))?;
            locals.insert(
                param.name.clone(),
                Local {
                    index: i as u32,
                    ty,
                },
            );
            local_ir_types.insert(param.name.clone(), param.ty.clone());
        }
        Ok(Self {
            locals,
            extra_locals: Vec::new(),
            param_count: function.params.len() as u32,
            func_index,
            structs,
            enums,
            local_ir_types,
            pool,
        })
    }

    /// Resolve the [`EnumLayout`] of an enum-typed `TypeRef` in this context.
    fn enum_layout(&self, ty: &TypeRef) -> Option<EnumLayout> {
        enum_layout(ty, self.enums)
    }

    /// Allocate a fresh non-parameter local of the given type; return its index.
    fn add_local(&mut self, ty: WasmValType) -> u32 {
        let index = self.param_count + self.extra_locals.len() as u32;
        self.extra_locals.push(ty);
        index
    }

    /// The recorded IR type of a named local, if any.
    fn local_ir_type(&self, name: &str) -> Option<TypeRef> {
        self.local_ir_types.get(name).cloned()
    }
}

fn lower_function(
    function: &IrFunction,
    func_index: &HashMap<String, u32>,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
    pool: &mut StringPool,
) -> Result<LoweredFunction, String> {
    let result = match return_val_type(&function.return_type, structs, enums) {
        Ok(result) => result,
        Err(()) => return Err("return type is not a supported WASM value type".to_string()),
    };
    let params = function
        .params
        .iter()
        .map(|p| value_val_type(&p.ty, structs, enums).expect("checked eligible"))
        .collect();

    let mut ctx = LowerCtx::new(function, func_index, structs, enums, pool)?;

    let mut body = Vec::new();
    lower_stmts(&mut ctx, &function.body, &mut body, &LoopCtx::none())?;

    // A non-void function must leave a value on every path. A trailing `Return`
    // or a value-producing tail `Expr` guarantees this; otherwise reject (the
    // interpreter still runs it) so WASM validation cannot fail.
    if result.is_some() && !body_guarantees_value(&function.body) {
        return Err(
            "non-void function may fall through without a return value (unsupported in WASM)"
                .to_string(),
        );
    }

    Ok(LoweredFunction {
        name: function.name.clone(),
        params,
        result,
        extra_locals: ctx.extra_locals,
        body,
    })
}

/// Loop context: branch depths (relative to the current point) for `break` and
/// `continue`. WASM `br N` targets the N-th enclosing structured block.
#[derive(Clone, Copy)]
struct LoopCtx {
    /// Relative depth of the enclosing `block` whose `end` is past the loop
    /// (target of `break`), or `None` if not in a loop.
    break_depth: Option<u32>,
    /// Relative depth of the enclosing `loop` (target of `continue`).
    continue_depth: Option<u32>,
}

impl LoopCtx {
    fn none() -> Self {
        Self {
            break_depth: None,
            continue_depth: None,
        }
    }

    /// Enter a structured block that does not change the loop targets but adds a
    /// level of nesting (e.g. an `if` block). Increments existing depths by 1.
    fn nest(self) -> Self {
        Self {
            break_depth: self.break_depth.map(|d| d + 1),
            continue_depth: self.continue_depth.map(|d| d + 1),
        }
    }
}

fn lower_stmts(
    ctx: &mut LowerCtx,
    stmts: &[IrStmt],
    out: &mut Vec<u8>,
    loops: &LoopCtx,
) -> Result<(), String> {
    for stmt in stmts {
        lower_stmt(ctx, stmt, out, loops)?;
    }
    Ok(())
}

fn lower_stmt(
    ctx: &mut LowerCtx,
    stmt: &IrStmt,
    out: &mut Vec<u8>,
    loops: &LoopCtx,
) -> Result<(), String> {
    match stmt {
        IrStmt::Let {
            name, ty, value, ..
        } => {
            let vty = value_val_type(ty, ctx.structs, ctx.enums)
                .ok_or_else(|| format!("`let {name}` has an unsupported type `{}`", ty.name))?;
            lower_expr(ctx, value, out)?;
            let index = ctx.add_local(vty);
            ctx.locals.insert(name.clone(), Local { index, ty: vty });
            ctx.local_ir_types.insert(name.clone(), ty.clone());
            set_local(out, index);
            Ok(())
        }
        IrStmt::Assign {
            name,
            path,
            op,
            value,
            ..
        } => {
            if path.is_empty() {
                return lower_local_assign(ctx, name, *op, value, out);
            }
            lower_path_assign(ctx, name, path, *op, value, out)
        }
        IrStmt::Return(value) => {
            if let Some(expr) = value {
                lower_expr(ctx, expr, out)?;
            }
            out.push(0x0f); // return
            Ok(())
        }
        IrStmt::Break(_) => {
            let depth = loops
                .break_depth
                .ok_or_else(|| "`break` outside a loop".to_string())?;
            out.push(0x0c); // br
            write_uleb(out, depth as u64);
            Ok(())
        }
        IrStmt::Continue(_) => {
            let depth = loops
                .continue_depth
                .ok_or_else(|| "`continue` outside a loop".to_string())?;
            out.push(0x0c); // br
            write_uleb(out, depth as u64);
            Ok(())
        }
        IrStmt::Expr(expr) => {
            // In the supported subset a value-producing expression only appears
            // as the tail of a non-void function (handled by the implicit `end`).
            // A void expression (e.g. a call returning void) pushes nothing.
            // Anything else (a value-producing statement not in tail position) is
            // rejected so the stack stays balanced.
            if expr_val_type(ctx, expr)?.is_some() {
                // Tail value: leave it on the stack for the function `end`.
                lower_expr(ctx, expr, out)?;
                Ok(())
            } else {
                lower_expr(ctx, expr, out)?;
                Ok(())
            }
        }
        IrStmt::If {
            branches,
            else_body,
            ..
        } => lower_if(ctx, branches, else_body, out, loops),
        IrStmt::While {
            condition, body, ..
        } => lower_while(ctx, condition, body, out),
        IrStmt::Loop { body, .. } => lower_loop(ctx, body, out),
        IrStmt::For {
            name,
            start,
            end,
            step,
            body,
            ..
        } => lower_for(ctx, name, start, end, step.as_ref(), body, out),
        IrStmt::Throw { .. } | IrStmt::Try { .. } => {
            Err("throw/try is not supported by the WASM backend".to_string())
        }
        // Inline `asm` emits raw x86-64 machine code and is native-only; a WASM
        // module cannot host it, so the function is demoted to the interpreters
        // (which then reject the `asm` with `L0425`).
        IrStmt::Asm { .. } => {
            Err("inline `asm` is native-only and not supported by the WASM backend".to_string())
        }
        IrStmt::Match {
            scrutinee, arms, ..
        } => lower_match(ctx, scrutinee, arms, out, loops),
    }
}

/// Lower a plain local assignment `name = value` or `name op= value`.
fn lower_local_assign(
    ctx: &mut LowerCtx,
    name: &str,
    op: lullaby_parser::AssignOp,
    value: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let local = *ctx
        .locals
        .get(name)
        .ok_or_else(|| format!("assignment to unknown local `{name}`"))?;
    match op {
        lullaby_parser::AssignOp::Replace => {
            lower_expr(ctx, value, out)?;
        }
        other => {
            // Compound assignment: local = local <op> value. A fixed-width local
            // uses the width-normalizing path so `+=`/`-=`/`*=`/`/=` wrap and
            // divide exactly like the interpreters.
            let ir_name = ctx.local_ir_type(name).map(|t| t.name);
            get_local(out, local.index);
            lower_expr(ctx, value, out)?;
            let bop = assign_binop(other);
            match ir_name.as_deref().and_then(fixed_int_kind) {
                Some(kind) => emit_fixed_binop(ctx, bop, kind, out)?,
                // Plain `i64` signed division uses the wrapping guard so
                // `i64::MIN /= -1` yields `i64::MIN` instead of trapping.
                None if matches!((bop, local.ty), (BinaryOp::Divide, WasmValType::I64)) => {
                    emit_i64_signed_div_guarded(ctx, out)
                }
                None => emit_binary_op_typed(bop, local.ty, out)?,
            }
        }
    }
    set_local(out, local.index);
    Ok(())
}

/// Lower an assignment to a struct field or array element, `name<path> = value`
/// (and the compound forms). The address of the target slot is computed once,
/// stashed in a scratch `i32` local, then a load-op-store (compound) or a plain
/// store writes the value.
fn lower_path_assign(
    ctx: &mut LowerCtx,
    name: &str,
    path: &[crate::IrPlace],
    op: lullaby_parser::AssignOp,
    value: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // Reconstruct the target's leaf type by walking the path from the base local.
    let base = *ctx
        .locals
        .get(name)
        .ok_or_else(|| format!("assignment to unknown local `{name}`"))?;
    if base.ty != WasmValType::I32 {
        return Err("field/index assignment requires a pointer (aggregate) base".to_string());
    }
    let base_ty = ctx
        .local_ir_type(name)
        .ok_or_else(|| format!("no IR type recorded for local `{name}`"))?;

    // Push the base pointer, then fold each hop into the running address. For a
    // non-final hop the slot holds a nested aggregate POINTER, so load it before
    // applying the next hop's offset; the final hop leaves the slot ADDRESS so we
    // can store into it.
    get_local(out, base.index);
    let mut cur_ty = base_ty;
    for (i, place) in path.iter().enumerate() {
        cur_ty = lower_place_address(ctx, &cur_ty, place, out)?;
        if i + 1 < path.len() {
            let slot_ty = slot_val_type(&cur_ty, ctx.structs, ctx.enums)
                .ok_or_else(|| "intermediate path slot has unsupported type".to_string())?;
            emit_load(slot_ty, out);
        }
    }
    // The target slot address is now on the stack. Stash it in a scratch local so
    // we can reuse it for the load (compound) and the store.
    let addr = ctx.add_local(WasmValType::I32);
    set_local(out, addr);

    let slot_ty = slot_val_type(&cur_ty, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("assignment target has unsupported type `{}`", cur_ty.name))?;

    match op {
        lullaby_parser::AssignOp::Replace => {
            get_local(out, addr);
            lower_expr(ctx, value, out)?;
            emit_store(slot_ty, out);
        }
        other => {
            // addr; load; value; op; then store at addr. A fixed-width slot uses
            // the width-normalizing path.
            get_local(out, addr);
            get_local(out, addr);
            emit_load(slot_ty, out);
            lower_expr(ctx, value, out)?;
            let bop = assign_binop(other);
            match fixed_int_kind(cur_ty.name.as_str()) {
                Some(kind) => emit_fixed_binop(ctx, bop, kind, out)?,
                // Plain `i64` signed division uses the wrapping guard so
                // `i64::MIN /= -1` yields `i64::MIN` instead of trapping.
                None if matches!((bop, slot_ty), (BinaryOp::Divide, WasmValType::I64)) => {
                    emit_i64_signed_div_guarded(ctx, out)
                }
                None => emit_binary_op_typed(bop, slot_ty, out)?,
            }
            emit_store(slot_ty, out);
        }
    }
    Ok(())
}

/// The `BinaryOp` a compound `AssignOp` desugars to.
fn assign_binop(op: lullaby_parser::AssignOp) -> BinaryOp {
    match op {
        lullaby_parser::AssignOp::Add => BinaryOp::Add,
        lullaby_parser::AssignOp::Subtract => BinaryOp::Subtract,
        lullaby_parser::AssignOp::Multiply => BinaryOp::Multiply,
        lullaby_parser::AssignOp::Divide => BinaryOp::Divide,
        lullaby_parser::AssignOp::Replace => unreachable!("Replace handled by caller"),
    }
}

/// Lower an `if`/`elif`/`else` chain to nested WASM `if`/`else` blocks (void
/// result type — the branches are statement blocks).
fn lower_if(
    ctx: &mut LowerCtx,
    branches: &[crate::IrIfBranch],
    else_body: &[IrStmt],
    out: &mut Vec<u8>,
    loops: &LoopCtx,
) -> Result<(), String> {
    lower_if_from(ctx, branches, 0, else_body, out, loops)
}

fn lower_if_from(
    ctx: &mut LowerCtx,
    branches: &[crate::IrIfBranch],
    idx: usize,
    else_body: &[IrStmt],
    out: &mut Vec<u8>,
    loops: &LoopCtx,
) -> Result<(), String> {
    let branch = &branches[idx];
    lower_expr(ctx, &branch.condition, out)?; // condition (i32)
    out.push(0x04); // if
    out.push(0x40); // void block type
    let inner = loops.nest();
    lower_stmts(ctx, &branch.body, out, &inner)?;
    out.push(0x05); // else
    if idx + 1 < branches.len() {
        lower_if_from(ctx, branches, idx + 1, else_body, out, &inner)?;
    } else {
        lower_stmts(ctx, else_body, out, &inner)?;
    }
    out.push(0x0b); // end
    Ok(())
}

/// Lower a `match` over a supported enum. The scrutinee is evaluated once to an
/// enum pointer stashed in a scratch local; its `i32` discriminant tag is loaded
/// into another scratch local, and the arms are dispatched by a chain of nested
/// WASM `if`/`else` blocks comparing the tag against each variant's discriminant.
/// A `Variant` arm binds its payload slots into fresh locals before its body; a
/// `Wildcard` arm becomes the chain's final `else`. If no wildcard is present the
/// last variant arm is emitted as the final `else` unconditionally (exhaustiveness
/// is enforced by semantics, so every remaining tag is that arm's variant), which
/// keeps every path value-producing for a value match without an `unreachable`.
///
/// A value-producing match (every arm's tail is a value expression of the same
/// scalar type) yields that value via each `if`'s typed block; a void match (arms
/// are statement blocks) uses void `if` blocks and yields nothing. All arm bodies
/// see `loops` re-based for the extra `if` nesting so `break`/`continue` inside an
/// arm still target the enclosing loop correctly.
fn lower_match(
    ctx: &mut LowerCtx,
    scrutinee: &IrExpr,
    arms: &[crate::IrMatchArm],
    out: &mut Vec<u8>,
    loops: &LoopCtx,
) -> Result<(), String> {
    let layout = ctx.enum_layout(&scrutinee.ty).ok_or_else(|| {
        format!(
            "match scrutinee type `{}` is not a supported WASM enum",
            scrutinee.ty.name
        )
    })?;
    if arms.is_empty() {
        return Err("match with no arms is unsupported".to_string());
    }

    // The match's result type: a value match has every arm ending in a
    // value-producing tail expression of the same type; a void match's arms are
    // statement blocks. Derive it from the first arm and trust semantics for the
    // agreement of the rest (exhaustiveness and arm-type uniformity are enforced
    // there). The block-type byte is the value type, or void (0x40).
    let result_ty = match_result_type(ctx, arms)?;
    let block_type = match result_ty {
        Some(vt) => vt.byte(),
        None => 0x40,
    };

    // Evaluate the scrutinee once to an enum pointer, stash it, and load the tag.
    lower_expr(ctx, scrutinee, out)?; // enum pointer (i32)
    let scrut = ctx.add_local(WasmValType::I32);
    set_local(out, scrut);
    let tag = ctx.add_local(WasmValType::I32);
    get_local(out, scrut);
    emit_load_at(WasmValType::I32, 0, out); // i32.load [scrut + 0] -> tag
    set_local(out, tag);

    lower_match_arms(ctx, &layout, scrut, tag, arms, 0, block_type, out, loops)
}

/// Emit the nested `if`/`else` dispatch chain for match arms starting at `idx`.
/// `scrut` holds the enum pointer, `tag` its loaded discriminant; `block_type` is
/// the WASM block-type byte for the value each arm yields (or `0x40` for void).
#[allow(clippy::too_many_arguments)]
fn lower_match_arms(
    ctx: &mut LowerCtx,
    layout: &EnumLayout,
    scrut: u32,
    tag: u32,
    arms: &[crate::IrMatchArm],
    idx: usize,
    block_type: u8,
    out: &mut Vec<u8>,
    loops: &LoopCtx,
) -> Result<(), String> {
    let arm = &arms[idx];
    let is_last = idx + 1 == arms.len();

    // The last arm — a wildcard, or (with exhaustiveness guaranteed) the final
    // variant — is emitted unconditionally so every tag reaches an arm body and a
    // value match always leaves a value. Earlier arms guard on `tag == disc`.
    match &arm.pattern {
        crate::IrMatchPattern::Wildcard => {
            // A wildcard binds nothing; evaluate its body directly.
            debug_assert!(is_last, "wildcard is always the final arm");
            lower_match_arm_body(ctx, layout, scrut, None, &arm.body, out, loops)
        }
        crate::IrMatchPattern::Variant { name, bindings } if is_last => lower_match_arm_body(
            ctx,
            layout,
            scrut,
            Some((name, bindings)),
            &arm.body,
            out,
            loops,
        ),
        crate::IrMatchPattern::Variant { name, bindings } => {
            let disc = layout.tag_of(name).ok_or_else(|| {
                format!("match arm variant `{name}` is not a variant of the enum")
            })?;
            // tag == disc ?
            get_local(out, tag);
            out.push(0x41); // i32.const disc
            write_sleb(out, disc as i64);
            out.push(0x46); // i32.eq
            out.push(0x04); // if
            out.push(block_type);
            let inner = loops.nest();
            lower_match_arm_body(
                ctx,
                layout,
                scrut,
                Some((name, bindings)),
                &arm.body,
                out,
                &inner,
            )?;
            out.push(0x05); // else
            lower_match_arms(
                ctx,
                layout,
                scrut,
                tag,
                arms,
                idx + 1,
                block_type,
                out,
                &inner,
            )?;
            out.push(0x0b); // end
            Ok(())
        }
    }
}

/// Bind a variant arm's payload slots into fresh locals (loading each from the
/// scrutinee record), then lower the arm body. A `None` binding (wildcard) binds
/// nothing. A value arm leaves its tail value on the stack (handled by the tail
/// `IrStmt::Expr` lowering); a void arm leaves nothing.
fn lower_match_arm_body(
    ctx: &mut LowerCtx,
    layout: &EnumLayout,
    scrut: u32,
    binding: Option<(&str, &[String])>,
    body: &[IrStmt],
    out: &mut Vec<u8>,
    loops: &LoopCtx,
) -> Result<(), String> {
    if let Some((variant, bindings)) = binding {
        let payload = layout
            .payload_of(variant)
            .ok_or_else(|| format!("variant `{variant}` is not a variant of the enum"))?
            .to_vec();
        // Bind each named payload position to a fresh local loaded from its slot.
        // Extra payload positions without a binding name are simply ignored, and a
        // binding without a payload slot is a lowering error (semantics forbids it).
        for (i, bind_name) in bindings.iter().enumerate() {
            let payload_ty = payload.get(i).ok_or_else(|| {
                format!("variant `{variant}` binding `{bind_name}` has no payload slot")
            })?;
            let slot_ty = slot_val_type(payload_ty, ctx.structs, ctx.enums)
                .ok_or_else(|| format!("variant `{variant}` payload has unsupported type"))?;
            get_local(out, scrut);
            emit_load_at(slot_ty, ENUM_PAYLOAD_BASE + i as i32 * SLOT_SIZE, out);
            let index = ctx.add_local(slot_ty);
            set_local(out, index);
            ctx.locals
                .insert(bind_name.clone(), Local { index, ty: slot_ty });
            ctx.local_ir_types
                .insert(bind_name.clone(), payload_ty.clone());
        }
    }
    lower_stmts(ctx, body, out, loops)
}

/// The WASM value type a `match` yields, or `None` for a void match. Derived from
/// the first arm whose body has a value-producing tail expression; when no arm
/// does, the match is void. Semantics guarantees every arm agrees, so the first
/// value-producing arm fixes the type for the whole match.
fn match_result_type(
    ctx: &LowerCtx,
    arms: &[crate::IrMatchArm],
) -> Result<Option<WasmValType>, String> {
    for arm in arms {
        if let Some(IrStmt::Expr(tail)) = arm.body.last()
            && !tail.ty.is_void()
        {
            return expr_val_type(ctx, tail);
        }
    }
    Ok(None)
}

/// Lower a `while`: `block { loop { br_if(!cond) end; body; br loop } }`.
fn lower_while(
    ctx: &mut LowerCtx,
    condition: &IrExpr,
    body: &[IrStmt],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // depth 0 = loop (continue), depth 1 = block (break).
    let inner = LoopCtx {
        break_depth: Some(1),
        continue_depth: Some(0),
    };
    lower_expr(ctx, condition, out)?;
    out.push(0x45); // i32.eqz
    out.push(0x0d); // br_if 1 (break when condition is false)
    write_uleb(out, 1);
    lower_stmts(ctx, body, out, &inner)?;
    out.push(0x0c); // br 0 (repeat)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
    Ok(())
}

/// Lower an infinite `loop` with `break`/`continue`:
/// `block { loop { body; br loop } }`.
fn lower_loop(ctx: &mut LowerCtx, body: &[IrStmt], out: &mut Vec<u8>) -> Result<(), String> {
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    let inner = LoopCtx {
        break_depth: Some(1),
        continue_depth: Some(0),
    };
    lower_stmts(ctx, body, out, &inner)?;
    out.push(0x0c); // br 0 (repeat)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
    Ok(())
}

/// Lower a range `for` to a loop over an `i64` induction variable, mirroring the
/// interpreter's inclusive range with an optional step: ascending stops when
/// `i > end`, descending when `i < end`.
#[allow(clippy::too_many_arguments)]
fn lower_for(
    ctx: &mut LowerCtx,
    name: &str,
    start: &IrExpr,
    end: &IrExpr,
    step: Option<&IrExpr>,
    body: &[IrStmt],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let i_index = ctx.add_local(WasmValType::I64);
    ctx.locals.insert(
        name.to_string(),
        Local {
            index: i_index,
            ty: WasmValType::I64,
        },
    );
    ctx.local_ir_types
        .insert(name.to_string(), TypeRef::new("i64"));
    let end_index = ctx.add_local(WasmValType::I64);
    let step_index = ctx.add_local(WasmValType::I64);

    // i = start
    lower_expr(ctx, start, out)?;
    set_local(out, i_index);
    // end_local = end
    lower_expr(ctx, end, out)?;
    set_local(out, end_index);
    // step_local = step (default 1)
    match step {
        Some(step_expr) => lower_expr(ctx, step_expr, out)?,
        None => {
            out.push(0x42); // i64.const
            write_sleb(out, 1);
        }
    }
    set_local(out, step_index);

    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    let inner = LoopCtx {
        break_depth: Some(1),
        continue_depth: Some(0),
    };

    // cond = (step >= 0) ? (i <= end) : (i >= end)
    get_local(out, step_index);
    out.push(0x42); // i64.const 0
    write_sleb(out, 0);
    out.push(0x59); // i64.ge_s
    out.push(0x04); // if
    out.push(0x7f); // result i32
    get_local(out, i_index);
    get_local(out, end_index);
    out.push(0x57); // i64.le_s
    out.push(0x05); // else
    get_local(out, i_index);
    get_local(out, end_index);
    out.push(0x59); // i64.ge_s
    out.push(0x0b); // end if -> i32 cond on stack
    out.push(0x45); // i32.eqz
    out.push(0x0d); // br_if 1 (break when cond false)
    write_uleb(out, 1);

    lower_stmts(ctx, body, out, &inner)?;

    // i += step
    get_local(out, i_index);
    get_local(out, step_index);
    out.push(0x7c); // i64.add
    set_local(out, i_index);

    out.push(0x0c); // br 0
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
    Ok(())
}

// -- Expression lowering -----------------------------------------------------

fn lower_expr(ctx: &mut LowerCtx, expr: &IrExpr, out: &mut Vec<u8>) -> Result<(), String> {
    match &expr.kind {
        IrExprKind::Integer(value) => {
            out.push(0x42); // i64.const
            write_sleb(out, *value);
            Ok(())
        }
        IrExprKind::Float(value) => {
            // A float literal's static type pins it to `f32` or `f64` (the type
            // checker resolves every literal to a concrete float type). An `f32`
            // literal rounds `value` to single precision first so its bits match
            // the interpreter's real `f32` store, then emits `f32.const`.
            if expr.ty.name == "f32" {
                out.push(0x43); // f32.const
                out.extend_from_slice(&(*value as f32).to_le_bytes());
            } else {
                out.push(0x44); // f64.const
                out.extend_from_slice(&value.to_le_bytes());
            }
            Ok(())
        }
        IrExprKind::Bool(value) => {
            out.push(0x41); // i32.const
            write_sleb(out, if *value { 1 } else { 0 });
            Ok(())
        }
        IrExprKind::Char(value) => {
            out.push(0x41); // i32.const
            write_sleb(out, *value as i64);
            Ok(())
        }
        IrExprKind::Variable(name) => {
            let local = ctx
                .locals
                .get(name)
                .ok_or_else(|| format!("unknown variable `{name}`"))?;
            get_local(out, local.index);
            Ok(())
        }
        IrExprKind::Unary { op, expr: inner } => match op {
            UnaryOp::Not => {
                lower_expr(ctx, inner, out)?;
                out.push(0x45); // i32.eqz (bool not)
                Ok(())
            }
            // Integer bitwise NOT (`~`): one's complement, implemented as
            // `x xor -1` (WASM has no `i64.not`). On a fixed-width kind the result
            // is re-normalized to the width, matching the interpreter's
            // `Value::int(!v, ty)`; on plain `i64` the full-width complement is
            // exact. Any other operand type is rejected (falls back to the
            // interpreters).
            UnaryOp::BitNot => {
                let kind = fixed_int_kind(inner.ty.name.as_str());
                if kind.is_none() && inner.ty.name != "i64" {
                    return Err(format!(
                        "bitwise `~` on unsupported type `{}` (wasm backend)",
                        inner.ty.name
                    ));
                }
                lower_expr(ctx, inner, out)?;
                out.push(0x42); // i64.const -1
                write_sleb(out, -1);
                out.push(0x85); // i64.xor
                if let Some(kind) = kind {
                    emit_normalize_i64(kind, out);
                }
                Ok(())
            }
        },
        IrExprKind::Binary { left, op, right } => lower_binary(ctx, left, *op, right, out),
        IrExprKind::String(text) => {
            // A string literal is a constant pointer to its interned Data-section
            // layout `[len i32][utf8 bytes]`.
            let offset = ctx.pool.intern(text);
            out.push(0x41); // i32.const
            write_sleb(out, offset as i64);
            Ok(())
        }
        IrExprKind::Array(elements) => lower_array_literal(ctx, expr, elements, out),
        IrExprKind::Index { target, index } => lower_index_read(ctx, target, index, out),
        IrExprKind::Field { target, field } => lower_field_read(ctx, target, field, out),
        IrExprKind::Call { name, args } => {
            // The host log builtin lowers to a `call` of the imported
            // `env.log_i64` (WASM function index `LOG_I64_FUNC_INDEX`).
            if name == WASM_LOG {
                if args.len() != 1 {
                    return Err(format!("wasm_log expects 1 argument, got {}", args.len()));
                }
                lower_expr(ctx, &args[0], out)?;
                out.push(0x10); // call
                write_uleb(out, LOG_I64_FUNC_INDEX as u64);
                return Ok(());
            }
            // `console_log(s)` lowers to `env.console_log(ptr, len)`: push the
            // string's linear-memory pointer and its length header, then call the
            // imported host function. A browser host implements it as
            // `console.log` over the (ptr, len) slice of `memory`.
            if name == CONSOLE_LOG {
                if args.len() != 1 {
                    return Err(format!(
                        "console_log expects 1 argument, got {}",
                        args.len()
                    ));
                }
                lower_string_ptr_len(ctx, &args[0], out)?;
                out.push(0x10); // call
                write_uleb(out, CONSOLE_LOG_FUNC_INDEX as u64);
                return Ok(());
            }
            // `dom_set_text(id, text)` lowers to
            // `env.dom_set_text(id_ptr, id_len, text_ptr, text_len)`: push each
            // string's pointer and length, then call the import. A browser host
            // implements it as `document.getElementById(id).textContent = text`.
            if name == DOM_SET_TEXT {
                if args.len() != 2 {
                    return Err(format!(
                        "dom_set_text expects 2 arguments, got {}",
                        args.len()
                    ));
                }
                lower_string_ptr_len(ctx, &args[0], out)?;
                lower_string_ptr_len(ctx, &args[1], out)?;
                out.push(0x10); // call
                write_uleb(out, DOM_SET_TEXT_FUNC_INDEX as u64);
                return Ok(());
            }
            // Fixed-width integer conversions are inlined, not real calls.
            // `to_<T>(x)` normalizes the argument's `i64` cell into `T`'s width
            // (truncate + sign/zero-extend), matching the interpreter's
            // `Value::int(x, T)`. This is the same encoding the native backend
            // uses.
            if let Some(kind) = to_int_conversion_kind(name) {
                if args.len() != 1 {
                    return Err(format!("`{name}` takes exactly one argument"));
                }
                lower_expr(ctx, &args[0], out)?;
                emit_normalize_i64(kind, out);
                return Ok(());
            }
            // `to_f32(x f64) -> f32` rounds an f64 to single precision with
            // `f32.demote_f64`; `to_f64(x f32) -> f64` widens an f32 with
            // `f64.promote_f32` (exact). These builtins are inlined, not real
            // calls — the same encoding the native backend uses (`cvtsd2ss` /
            // `cvtss2sd`), so the WASM result is bit-identical to the interpreter.
            if name == "to_f32" {
                if args.len() != 1 {
                    return Err("`to_f32` takes exactly one argument".to_string());
                }
                lower_expr(ctx, &args[0], out)?;
                out.push(0xb6); // f32.demote_f64
                return Ok(());
            }
            if name == "to_f64" {
                if args.len() != 1 {
                    return Err("`to_f64` takes exactly one argument".to_string());
                }
                lower_expr(ctx, &args[0], out)?;
                out.push(0xbb); // f64.promote_f32
                return Ok(());
            }
            // `to_i64(x)` widens a fixed-width cell to `i64`; the source cell is
            // already normalized, so this is the identity on the bits.
            if name == "to_i64" {
                if args.len() != 1 {
                    return Err("`to_i64` takes exactly one argument".to_string());
                }
                lower_expr(ctx, &args[0], out)?;
                return Ok(());
            }
            // Growable `list<T>` (scalar `T`) builtins. `list_new()` allocates an
            // empty header; `push`/`get`/`set`/`pop` operate on a `list`-typed
            // first argument (checked so these names cannot shadow a user function
            // or an array op). `len(l)` is NOT special-cased here — a list's `len`
            // shares offset 0 with the string/array length header, so the generic
            // `len` path below reads it. A list op whose element is a heap type is
            // deferred: `supported_list_element` returns `None`, so lowering errors
            // and the function is demoted to the interpreters.
            if name == LIST_NEW_BUILTIN {
                return lower_list_new(ctx, args, out);
            }
            if name == LIST_PUSH_BUILTIN && args.len() == 2 && args[0].ty.list_element().is_some() {
                return lower_list_push(ctx, &args[0], &args[1], out);
            }
            if name == LIST_GET_BUILTIN && args.len() == 2 && args[0].ty.list_element().is_some() {
                return lower_list_get(ctx, &args[0], &args[1], out);
            }
            if name == LIST_SET_BUILTIN && args.len() == 3 && args[0].ty.list_element().is_some() {
                return lower_list_set(ctx, &args[0], &args[1], &args[2], out);
            }
            if name == LIST_POP_BUILTIN && args.len() == 1 && args[0].ty.list_element().is_some() {
                return lower_list_pop(ctx, &args[0], out);
            }
            // `len(s)`/`len(a)`/`len(l)` reads the leading i32 length header.
            if name == LEN_BUILTIN {
                return lower_len(ctx, args, out);
            }
            // A call whose name is a declared struct is a struct construction: the
            // IR lowerer emits struct literals as positional `Call`s.
            if ctx.structs.contains_key(name) {
                return lower_struct_construction(ctx, name, args, out);
            }
            // A call whose result type is a supported enum and whose name is one of
            // its variants is enum construction: `some(x)`/`ok(x)`/`err(e)`/`none`
            // (the built-ins) or a user `Variant(payload...)`. The IR lowerer emits
            // these as positional `Call`s (with empty `args` for a unit variant),
            // carrying the constructed enum type as `expr.ty`.
            if let Some(layout) = ctx.enum_layout(&expr.ty)
                && layout.tag_of(name).is_some()
            {
                return lower_enum_construction(ctx, &layout, name, args, out);
            }
            let index = *ctx.func_index.get(name).ok_or_else(|| {
                format!("call to unsupported builtin or unknown function `{name}`")
            })?;
            for arg in args {
                lower_expr(ctx, arg, out)?;
                // Preserve Lullaby value semantics across the call boundary: an
                // aggregate is an `i32` pointer, so passing it raw would let the
                // callee mutate the caller's record through a shared pointer. A
                // mutable aggregate argument (struct/array/enum — never an
                // immutable `string`) is deep-copied into a fresh record here, so
                // the callee receives an independent snapshot exactly like the
                // interpreters clone the argument value. A returned aggregate is
                // the callee's own fresh record, so no copy is needed there.
                if is_mutable_aggregate(&arg.ty, ctx.structs, ctx.enums) {
                    emit_deep_copy(ctx, &arg.ty, out)?;
                }
            }
            out.push(0x10); // call
            write_uleb(out, index as u64);
            Ok(())
        }
        IrExprKind::Await { .. } => Err("await is not supported by the WASM backend".to_string()),
        // Closures are not compiled to WASM in this increment: a function that
        // constructs or calls a closure is skipped (this `Err`) and falls back to
        // the interpreters, exactly like other unsupported constructs.
        IrExprKind::Closure { .. } => {
            Err("closures are not supported by the WASM backend".to_string())
        }
    }
}

/// Lower a `string` argument to the two host-import operands `[ptr, len]`: push
/// the string's linear-memory pointer, then its length header. The pointer is
/// evaluated once into a scratch `i32` local so a non-trivial string expression
/// is not lowered twice; the length is the leading `i32` header of the interned
/// `[len i32][utf8 bytes]` layout (the char count, equal to the byte length for
/// ASCII, which is what the host decodes out of `memory`).
fn lower_string_ptr_len(ctx: &mut LowerCtx, arg: &IrExpr, out: &mut Vec<u8>) -> Result<(), String> {
    if value_val_type(&arg.ty, ctx.structs, ctx.enums) != Some(WasmValType::I32)
        || arg.ty.name != "string"
    {
        return Err(format!(
            "console_log/dom_set_text expect a string but got `{}`",
            arg.ty.name
        ));
    }
    lower_expr(ctx, arg, out)?; // string pointer (i32)
    let ptr = ctx.add_local(WasmValType::I32);
    set_local(out, ptr);
    get_local(out, ptr); // operand: ptr
    get_local(out, ptr); // base for the length load
    out.push(0x28); // i32.load
    out.push(0x02); // align 2 (4-byte)
    write_uleb(out, 0); // offset 0 (the length header) -> operand: len
    Ok(())
}

/// Lower `len(x)` where `x` is a `string` or `array`: load the leading `i32`
/// length header (char count for strings, element count for arrays), then extend
/// to `i64` (the builtin's result type on the interpreters).
fn lower_len(ctx: &mut LowerCtx, args: &[IrExpr], out: &mut Vec<u8>) -> Result<(), String> {
    if args.len() != 1 {
        return Err(format!("len expects 1 argument, got {}", args.len()));
    }
    let arg = &args[0];
    if value_val_type(&arg.ty, ctx.structs, ctx.enums) != Some(WasmValType::I32) {
        return Err(format!(
            "len expects a string or array but got `{}`",
            arg.ty.name
        ));
    }
    lower_expr(ctx, arg, out)?; // pointer (i32)
    out.push(0x28); // i32.load
    out.push(0x02); // align 2 (4-byte)
    write_uleb(out, 0); // offset 0 (the length header)
    // i64.extend_i32_s -> the builtin returns i64.
    out.push(0xac);
    Ok(())
}

/// Lower a struct construction `Struct(f0, f1, ...)`: `__alloc` a run of one
/// 8-byte slot per field, then store each field value at its slot offset. Leaves
/// the base pointer on the stack.
fn lower_struct_construction(
    ctx: &mut LowerCtx,
    name: &str,
    args: &[IrExpr],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let fields = ctx
        .structs
        .get(name)
        .ok_or_else(|| format!("`{name}` is not a struct"))?
        .clone();
    if args.len() != fields.len() {
        return Err(format!(
            "struct `{name}` expects {} fields, got {}",
            fields.len(),
            args.len()
        ));
    }
    let ptr = alloc_bytes(ctx, fields.len() as i32 * SLOT_SIZE, out);
    for (slot, ((_, field_ty), arg)) in fields.iter().zip(args).enumerate() {
        let slot_ty = slot_val_type(field_ty, ctx.structs, ctx.enums)
            .ok_or_else(|| format!("struct `{name}` field has unsupported type"))?;
        get_local(out, ptr); // base pointer
        lower_expr(ctx, arg, out)?; // field value
        emit_store_at(slot_ty, slot as i32 * SLOT_SIZE, out);
    }
    get_local(out, ptr);
    Ok(())
}

/// Lower an enum construction (`some(x)`/`none`/`ok(x)`/`err(e)` or a user
/// `Variant(payload...)`): `__alloc` a `[tag i32 (padded)][slot0][slot1]...]`
/// record sized for the enum's widest variant, store the variant's discriminant
/// tag at offset 0, store each payload value into its leading slot, and leave the
/// base pointer (the enum value) on the stack. The discriminant is the variant's
/// index in the enum's declaration order, matching the interpreters (which
/// dispatch `match` by variant name against this same ordered layout).
fn lower_enum_construction(
    ctx: &mut LowerCtx,
    layout: &EnumLayout,
    variant: &str,
    args: &[IrExpr],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let tag = layout
        .tag_of(variant)
        .ok_or_else(|| format!("`{variant}` is not a variant of the enum"))?;
    let payload = layout
        .payload_of(variant)
        .ok_or_else(|| format!("`{variant}` is not a variant of the enum"))?
        .to_vec();
    if args.len() != payload.len() {
        return Err(format!(
            "enum variant `{variant}` expects {} payload value(s), got {}",
            payload.len(),
            args.len()
        ));
    }
    let ptr = alloc_bytes(ctx, layout.size_bytes(), out);
    // Tag at offset 0 (i32 discriminant).
    get_local(out, ptr);
    out.push(0x41); // i32.const tag
    write_sleb(out, tag as i64);
    emit_store_at(WasmValType::I32, 0, out);
    // Payload values into the leading slots (offset ENUM_PAYLOAD_BASE + i*SLOT).
    for (slot, (payload_ty, arg)) in payload.iter().zip(args).enumerate() {
        let slot_ty = slot_val_type(payload_ty, ctx.structs, ctx.enums)
            .ok_or_else(|| format!("enum variant `{variant}` payload has unsupported type"))?;
        get_local(out, ptr); // base pointer
        lower_expr(ctx, arg, out)?; // payload value
        emit_store_at(slot_ty, ENUM_PAYLOAD_BASE + slot as i32 * SLOT_SIZE, out);
    }
    get_local(out, ptr);
    Ok(())
}

/// Lower a fixed array literal `[e0, e1, ...]`: `__alloc` a `[len i32][slots]`
/// block, write the length header and each element slot, and leave the base
/// pointer on the stack.
fn lower_array_literal(
    ctx: &mut LowerCtx,
    expr: &IrExpr,
    elements: &[IrExpr],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = expr
        .ty
        .array_element()
        .ok_or_else(|| format!("array literal has non-array type `{}`", expr.ty.name))?;
    let slot_ty = slot_val_type(&elem_ty, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("array element type `{}` is unsupported", elem_ty.name))?;
    let total = LEN_HEADER + elements.len() as i32 * SLOT_SIZE;
    let ptr = alloc_bytes(ctx, total, out);
    // Length header: i32.store [ptr + 0] = element count.
    get_local(out, ptr);
    out.push(0x41); // i32.const
    write_sleb(out, elements.len() as i64);
    out.push(0x36); // i32.store
    out.push(0x02); // align 2
    write_uleb(out, 0);
    for (i, element) in elements.iter().enumerate() {
        get_local(out, ptr);
        lower_expr(ctx, element, out)?;
        emit_store_at(slot_ty, LEN_HEADER + i as i32 * SLOT_SIZE, out);
    }
    get_local(out, ptr);
    Ok(())
}

/// Lower a struct field read `target.field`: push the target pointer, add the
/// field's slot offset, and load the slot.
fn lower_field_read(
    ctx: &mut LowerCtx,
    target: &IrExpr,
    field: &str,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (offset, slot_ty) = struct_field_slot(ctx, &target.ty, field)?;
    lower_expr(ctx, target, out)?; // base pointer
    emit_load_at(slot_ty, offset, out);
    Ok(())
}

/// Lower an array element read `target[index]`: compute the slot address, then
/// load it. WASM traps on out-of-bounds memory access (no explicit bounds check
/// this increment).
fn lower_index_read(
    ctx: &mut LowerCtx,
    target: &IrExpr,
    index: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = target
        .ty
        .array_element()
        .ok_or_else(|| format!("indexing a non-array type `{}`", target.ty.name))?;
    let slot_ty = slot_val_type(&elem_ty, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("array element type `{}` is unsupported", elem_ty.name))?;
    lower_expr(ctx, target, out)?; // base pointer (i32)
    lower_array_slot_offset(ctx, index, out)?; // += header + index*SLOT_SIZE
    emit_load(slot_ty, out);
    Ok(())
}

fn lower_binary(
    ctx: &mut LowerCtx,
    left: &IrExpr,
    op: BinaryOp,
    right: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // Short-circuit `and`/`or` via WASM `if`/`else` producing i32.
    match op {
        BinaryOp::And => {
            lower_expr(ctx, left, out)?;
            out.push(0x04); // if
            out.push(0x7f); // result i32
            lower_expr(ctx, right, out)?;
            out.push(0x05); // else
            out.push(0x41); // i32.const 0
            write_sleb(out, 0);
            out.push(0x0b); // end
            return Ok(());
        }
        BinaryOp::Or => {
            lower_expr(ctx, left, out)?;
            out.push(0x04); // if
            out.push(0x7f); // result i32
            out.push(0x41); // i32.const 1
            write_sleb(out, 1);
            out.push(0x05); // else
            lower_expr(ctx, right, out)?;
            out.push(0x0b); // end
            return Ok(());
        }
        _ => {}
    }

    // A fixed-width operand kind (both operands share it; the type checker forbids
    // mixing widths) selects width- and signedness-correct codegen that
    // re-normalizes width-producing results, mirroring the interpreter free
    // functions and the native backend.
    if let Some(kind) = fixed_int_kind(left.ty.name.as_str()) {
        lower_expr(ctx, left, out)?;
        lower_expr(ctx, right, out)?;
        return emit_fixed_binop(ctx, op, kind, out);
    }

    // Integer bitwise/shift operators on plain `i64` map directly to the WASM
    // opcodes (no width normalization needed). f64/bool/char/byte cannot carry
    // them, so a bitwise/shift op on a non-integer type is rejected (the function
    // falls back to the interpreters).
    if matches!(
        op,
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Shl | BinaryOp::Shr
    ) {
        if left.ty.name != "i64" {
            return Err(format!(
                "bitwise/shift operator on unsupported type `{}` (wasm backend)",
                left.ty.name
            ));
        }
        lower_expr(ctx, left, out)?;
        lower_expr(ctx, right, out)?;
        return emit_i64_bitwise_or_shift(op, out);
    }

    // The operand value type drives the opcode family. For a FLOAT operand this
    // must be derived structurally: the IR annotates a float ARITHMETIC node with
    // `i64` (see the IR binary lowerer), so `if a + b > c` would otherwise pick an
    // integer compare over f32/f64 values. `float_val_type_of` looks through
    // arithmetic to the reliably-typed leaves (float literals, float locals, and
    // the `to_f32`/`to_f64` conversions); when neither operand is a float it falls
    // back to the left operand's own value type (i64/i32).
    let operand_ty = match float_val_type_of(ctx, left).or_else(|| float_val_type_of(ctx, right)) {
        Some(ft) => ft,
        None => expr_val_type(ctx, left)?
            .ok_or_else(|| "binary operand has no scalar value".to_string())?,
    };
    lower_expr(ctx, left, out)?;
    lower_expr(ctx, right, out)?;
    // Plain `i64` signed division goes through the wrapping guard so `i64::MIN /
    // -1` yields `i64::MIN` instead of trapping, matching the interpreters.
    if matches!((op, operand_ty), (BinaryOp::Divide, WasmValType::I64)) {
        emit_i64_signed_div_guarded(ctx, out);
        return Ok(());
    }
    emit_binary_op_typed(op, operand_ty, out)
}

/// The WASM float value type (`F32`/`F64`) an expression evaluates to, or `None`
/// if it is not a float. Mirrors the native backend's `float_width_of_expr`: it
/// reads only the leaf nodes the IR types correctly — float literals, float
/// locals/params, and the `to_f32`/`to_f64` conversions — and recurses through
/// float arithmetic (`+ - * /`), whose own node type the IR annotates `i64`. A
/// comparison yields a `bool` (not a float), so it reports `None`.
fn float_val_type_of(ctx: &LowerCtx, expr: &IrExpr) -> Option<WasmValType> {
    match &expr.kind {
        IrExprKind::Float(_) => match scalar_val_type(&expr.ty) {
            Some(ft @ (WasmValType::F32 | WasmValType::F64)) => Some(ft),
            _ => None,
        },
        IrExprKind::Variable(name) => match ctx.locals.get(name)?.ty {
            ft @ (WasmValType::F32 | WasmValType::F64) => Some(ft),
            _ => None,
        },
        IrExprKind::Call { name, .. } => match name.as_str() {
            "to_f32" => Some(WasmValType::F32),
            "to_f64" => Some(WasmValType::F64),
            _ => None,
        },
        IrExprKind::Binary {
            left,
            op: BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide,
            right,
        } => float_val_type_of(ctx, left).or_else(|| float_val_type_of(ctx, right)),
        _ => None,
    }
}

/// Emit a fixed-width binary op whose operands (both normalized `i64` cells of
/// `kind`) are already on the stack (left then right), leaving the result (a
/// normalized cell for arithmetic/bitwise/shift, a canonical `0`/`1` for
/// comparisons) on the stack. This mirrors the interpreter free functions
/// exactly: arithmetic wraps then re-normalizes (`Value::int`), division and
/// comparison are signedness-aware (`int_div`/`int_cmp`), and shifts mask the
/// count to the width and honor signedness (`int_shl`/`int_shr`).
fn emit_fixed_binop(
    ctx: &mut LowerCtx,
    op: BinaryOp,
    kind: IntKind,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    match op {
        BinaryOp::Add => {
            out.push(0x7c); // i64.add
            emit_normalize_i64(kind, out);
        }
        BinaryOp::Subtract => {
            out.push(0x7d); // i64.sub
            emit_normalize_i64(kind, out);
        }
        BinaryOp::Multiply => {
            out.push(0x7e); // i64.mul
            emit_normalize_i64(kind, out);
        }
        BinaryOp::Divide => {
            // Divide on the full 64-bit cell (signedness-correct: signed cells are
            // sign-extended, unsigned cells zero-extended), matching `int_div`.
            // WASM `div_s`/`div_u` traps on a zero divisor, exactly like the
            // existing `i64` divide path.
            if kind.is_unsigned() {
                out.push(0x80); // i64.div_u
            } else {
                // Signed division guards `i64::MIN / -1` (and, after
                // normalization, each width's MIN / -1) against the WASM trap.
                emit_i64_signed_div_guarded(ctx, out);
            }
            emit_normalize_i64(kind, out);
        }
        // Equality is width-agnostic on the normalized cells.
        BinaryOp::Equal => out.push(0x51),    // i64.eq
        BinaryOp::NotEqual => out.push(0x52), // i64.ne
        // Ordering uses unsigned comparisons for unsigned kinds, signed for
        // signed kinds, on the normalized cells.
        BinaryOp::Less => out.push(if kind.is_unsigned() { 0x54 } else { 0x53 }), // lt_u/lt_s
        BinaryOp::LessEqual => out.push(if kind.is_unsigned() { 0x58 } else { 0x57 }), // le_u/le_s
        BinaryOp::Greater => out.push(if kind.is_unsigned() { 0x56 } else { 0x55 }), // gt_u/gt_s
        BinaryOp::GreaterEqual => out.push(if kind.is_unsigned() { 0x5a } else { 0x59 }), // ge_u/ge_s
        BinaryOp::BitAnd => {
            out.push(0x83); // i64.and
            emit_normalize_i64(kind, out);
        }
        BinaryOp::BitOr => {
            out.push(0x84); // i64.or
            emit_normalize_i64(kind, out);
        }
        BinaryOp::BitXor => {
            out.push(0x85); // i64.xor
            emit_normalize_i64(kind, out);
        }
        BinaryOp::Shl | BinaryOp::Shr => {
            // Mask the shift count to `width-1` (matching `int_shl`/`int_shr`):
            // `right & (width-1)`. The count is already on the stack; AND it with
            // the mask, then shift the left operand and re-normalize. `<<` is
            // `shl`; `>>` is `shr_u` (logical) for unsigned kinds, `shr_s`
            // (arithmetic) for signed kinds.
            let mask = i64::from(kind.width_bits() - 1); // 7/15/31/63
            out.push(0x42); // i64.const mask
            write_sleb(out, mask);
            out.push(0x83); // i64.and (masked count)
            let shift_opcode = match (op, kind.is_unsigned()) {
                (BinaryOp::Shl, _) => 0x86,     // i64.shl
                (BinaryOp::Shr, true) => 0x88,  // i64.shr_u (logical)
                (BinaryOp::Shr, false) => 0x87, // i64.shr_s (arithmetic)
                _ => unreachable!("outer match restricts to shifts"),
            };
            out.push(shift_opcode);
            emit_normalize_i64(kind, out);
        }
        BinaryOp::And | BinaryOp::Or => {
            return Err("logical and/or must be short-circuited".to_string());
        }
    }
    Ok(())
}

/// Emit a bitwise/shift binary op on plain `i64` operands already on the stack
/// (left then right). No width normalization is needed: `i64` fills the cell.
fn emit_i64_bitwise_or_shift(op: BinaryOp, out: &mut Vec<u8>) -> Result<(), String> {
    let opcode = match op {
        BinaryOp::BitAnd => 0x83, // i64.and
        BinaryOp::BitOr => 0x84,  // i64.or
        BinaryOp::BitXor => 0x85, // i64.xor
        BinaryOp::Shl => 0x86,    // i64.shl (WASM masks the count modulo 64)
        BinaryOp::Shr => 0x87,    // i64.shr_s (arithmetic, matching the i64 shift)
        _ => unreachable!("caller restricts to bitwise/shift"),
    };
    out.push(opcode);
    Ok(())
}

// -- Linear-memory helpers ---------------------------------------------------

/// `__alloc(size)` a run of `size` bytes and stash the returned pointer in a
/// fresh scratch `i32` local; return that local's index. The pointer is reused
/// for each field/element store and finally re-pushed as the aggregate value.
fn alloc_bytes(ctx: &mut LowerCtx, size: i32, out: &mut Vec<u8>) -> u32 {
    let alloc_index = *ctx
        .func_index
        .get(ALLOC_HELPER_NAME)
        .expect("__alloc index recorded");
    out.push(0x41); // i32.const size
    write_sleb(out, size as i64);
    out.push(0x10); // call __alloc
    write_uleb(out, alloc_index as u64);
    let ptr = ctx.add_local(WasmValType::I32);
    set_local(out, ptr);
    ptr
}

/// Deep-copy the aggregate whose pointer is on top of the stack, leaving a fresh,
/// independent record's pointer on the stack. This is the WASM realization of the
/// interpreters' recursive `Value::clone` on a struct/array/enum: a snapshot whose
/// mutation cannot be observed through the original pointer.
///
/// - A `struct` copies each field slot; a scalar/string slot is copied word-for-
///   word, and a nested MUTABLE aggregate slot (struct/array/enum) is itself
///   deep-copied so nested mutation stays isolated too.
/// - A fixed `array` reads its `[len]` header, allocates a fresh `[len][slots]`
///   block, and copies each element in a runtime loop (deep-copying nested
///   aggregate elements).
/// - An `enum` copies its `[tag][payload slots]` record word-for-word: enum
///   payloads are always scalar (see [`enum_layout`]), so no nested aggregate can
///   hide inside, and a flat copy of the whole record is an exact deep copy.
///
/// `ty` must be a mutable aggregate ([`is_mutable_aggregate`]); the caller checks
/// this before invoking. An `i32`-pointer source, an `i32`-pointer result.
fn emit_deep_copy(ctx: &mut LowerCtx, ty: &TypeRef, out: &mut Vec<u8>) -> Result<(), String> {
    if ctx.structs.contains_key(&ty.name) {
        return emit_deep_copy_struct(ctx, ty, out);
    }
    if let Some(layout) = ctx.enum_layout(ty) {
        return emit_deep_copy_enum(ctx, &layout, out);
    }
    if ty.array_element().is_some() {
        return emit_deep_copy_array(ctx, ty, out);
    }
    if supported_list_element(ty).is_some() {
        return emit_list_deep_copy(ctx, out);
    }
    Err(format!(
        "cannot deep-copy non-aggregate type `{}` (wasm backend)",
        ty.name
    ))
}

/// Deep-copy a struct: the source pointer is on the stack. Allocate a fresh run of
/// one 8-byte slot per field, copy each field (deep-copying nested mutable
/// aggregate fields), and leave the fresh pointer on the stack.
fn emit_deep_copy_struct(
    ctx: &mut LowerCtx,
    ty: &TypeRef,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let fields = ctx
        .structs
        .get(&ty.name)
        .ok_or_else(|| format!("`{}` is not a struct", ty.name))?
        .clone();
    // Stash the source pointer; allocate the destination.
    let src = ctx.add_local(WasmValType::I32);
    set_local(out, src);
    let dst = alloc_bytes(ctx, fields.len() as i32 * SLOT_SIZE, out);
    for (slot, (_, field_ty)) in fields.iter().enumerate() {
        let offset = slot as i32 * SLOT_SIZE;
        emit_copy_slot(ctx, field_ty, src, dst, offset, out)?;
    }
    get_local(out, dst);
    Ok(())
}

/// Deep-copy an enum: the source pointer is on the stack. Enum payloads are always
/// scalar (see [`enum_layout`]), so the `[tag][payload slots]` record contains no
/// nested aggregate pointer; copying every 8-byte word of `size_bytes()` (the tag,
/// padded to a slot, plus each payload slot) is an exact deep copy. Leaves the
/// fresh pointer on the stack.
fn emit_deep_copy_enum(
    ctx: &mut LowerCtx,
    layout: &EnumLayout,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let src = ctx.add_local(WasmValType::I32);
    set_local(out, src);
    let dst = alloc_bytes(ctx, layout.size_bytes(), out);
    // The record is `size_bytes()` bytes = one padded tag slot + slot_count payload
    // slots, every one 8-byte aligned. Copy word by word with i64 loads/stores.
    let words = layout.size_bytes() / SLOT_SIZE;
    for word in 0..words {
        let offset = word * SLOT_SIZE;
        get_local(out, dst);
        get_local(out, src);
        emit_load_at(WasmValType::I64, offset, out);
        emit_store_at(WasmValType::I64, offset, out);
    }
    get_local(out, dst);
    Ok(())
}

/// Deep-copy a fixed array: the source pointer is on the stack. Read the `[len]`
/// header, allocate a fresh `[len][slots]` block, store the header, and copy each
/// element in a runtime loop (deep-copying nested mutable aggregate elements).
/// Leaves the fresh pointer on the stack.
fn emit_deep_copy_array(ctx: &mut LowerCtx, ty: &TypeRef, out: &mut Vec<u8>) -> Result<(), String> {
    let elem_ty = ty
        .array_element()
        .ok_or_else(|| format!("`{}` is not an array", ty.name))?;
    // Validate the element is a supported slot type up front (the caller already
    // classified this array as a mutable aggregate, but re-check for the copy loop).
    slot_val_type(&elem_ty, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("array element type `{}` is unsupported", elem_ty.name))?;

    let src = ctx.add_local(WasmValType::I32);
    set_local(out, src);
    // len = i32.load [src + 0]
    let len = ctx.add_local(WasmValType::I32);
    get_local(out, src);
    emit_load_at(WasmValType::I32, 0, out);
    set_local(out, len);
    // Allocate LEN_HEADER + len * SLOT_SIZE bytes: push the runtime size, call
    // __alloc directly (alloc_bytes only takes a constant size), stash the dst.
    let alloc_index = *ctx
        .func_index
        .get(ALLOC_HELPER_NAME)
        .expect("__alloc index recorded");
    get_local(out, len);
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul  -> len * SLOT_SIZE
    out.push(0x41); // i32.const LEN_HEADER
    write_sleb(out, LEN_HEADER as i64);
    out.push(0x6a); // i32.add  -> LEN_HEADER + len * SLOT_SIZE
    out.push(0x10); // call __alloc
    write_uleb(out, alloc_index as u64);
    let dst = ctx.add_local(WasmValType::I32);
    set_local(out, dst);
    // Store the length header at dst + 0.
    get_local(out, dst);
    get_local(out, len);
    emit_store_at(WasmValType::I32, 0, out);

    // Runtime loop: for i in 0..len { copy element i }.
    let i = ctx.add_local(WasmValType::I32);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, i);
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // break when i >= len
    get_local(out, i);
    get_local(out, len);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of the block)
    write_uleb(out, 1);
    // Element slot offset from the base: LEN_HEADER + i * SLOT_SIZE, in a local.
    let off = ctx.add_local(WasmValType::I32);
    get_local(out, i);
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul
    out.push(0x41); // i32.const LEN_HEADER
    write_sleb(out, LEN_HEADER as i64);
    out.push(0x6a); // i32.add
    set_local(out, off);
    // Copy element: dst_addr = dst + off, src_addr = src + off.
    emit_copy_element_at(ctx, &elem_ty, src, dst, off, out)?;
    // i += 1
    get_local(out, i);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, i);
    out.push(0x0c); // br 0 (repeat)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block

    get_local(out, dst);
    Ok(())
}

// -- Growable list codegen ---------------------------------------------------

/// `__alloc(size)` where `size` (an `i32`) is already on the stack, stashing the
/// returned pointer in a fresh scratch `i32` local; return that local's index.
/// The constant-size companion is [`alloc_bytes`]; this variant handles a runtime
/// byte count (a list's `LIST_DATA_OFF + cap * SLOT_SIZE`).
fn alloc_runtime(ctx: &mut LowerCtx, out: &mut Vec<u8>) -> u32 {
    let alloc_index = *ctx
        .func_index
        .get(ALLOC_HELPER_NAME)
        .expect("__alloc index recorded");
    out.push(0x10); // call __alloc (size already on the stack)
    write_uleb(out, alloc_index as u64);
    let ptr = ctx.add_local(WasmValType::I32);
    set_local(out, ptr);
    ptr
}

/// Push `LIST_DATA_OFF + cap * SLOT_SIZE` (the byte size of a list backing block
/// with `cap` element slots) onto the stack, given an `i32` local holding `cap`.
fn emit_list_block_size(cap_local: u32, out: &mut Vec<u8>) {
    get_local(out, cap_local);
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul  -> cap * SLOT_SIZE
    out.push(0x41); // i32.const LIST_DATA_OFF
    write_sleb(out, LIST_DATA_OFF as i64);
    out.push(0x6a); // i32.add  -> LIST_DATA_OFF + cap * SLOT_SIZE
}

/// Copy the first `count` element slots (each `SLOT_SIZE` bytes) from `src` to
/// `dst` list backing blocks in a runtime loop. `count` is an `i32` local; `src`
/// and `dst` are `i32` locals holding list base pointers. Element slots are copied
/// word-for-word by `SLOT_SIZE`-aligned `i64` load/store — list elements are
/// always scalar (see [`supported_list_element`]), so a flat word copy is an exact
/// deep copy and needs no per-element type dispatch.
fn emit_list_copy_elems(ctx: &mut LowerCtx, src: u32, dst: u32, count: u32, out: &mut Vec<u8>) {
    let i = ctx.add_local(WasmValType::I32);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, i);
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // break when i >= count
    get_local(out, i);
    get_local(out, count);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of the block)
    write_uleb(out, 1);
    // off = LIST_DATA_OFF + i * SLOT_SIZE
    let off = ctx.add_local(WasmValType::I32);
    get_local(out, i);
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul
    out.push(0x41); // i32.const LIST_DATA_OFF
    write_sleb(out, LIST_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    set_local(out, off);
    // memory[dst + off] = memory[src + off] (one 8-byte word)
    get_local(out, dst);
    get_local(out, off);
    out.push(0x6a); // i32.add -> dst addr
    get_local(out, src);
    get_local(out, off);
    out.push(0x6a); // i32.add -> src addr
    emit_load(WasmValType::I64, out);
    emit_store(WasmValType::I64, out);
    // i += 1
    get_local(out, i);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, i);
    out.push(0x0c); // br 0 (repeat)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
}

/// Deep-copy the growable `list<T>` whose pointer is on the stack, leaving a fresh
/// independent `[len][cap][slots]` block's pointer on the stack. The copy keeps the
/// source's `len` and `cap` and duplicates its `len` element slots. This is the
/// WASM realization of the interpreters' `Value::clone` on a list: mutating the
/// copy (or the original) is never observable through the other pointer.
fn emit_list_deep_copy(ctx: &mut LowerCtx, out: &mut Vec<u8>) -> Result<(), String> {
    let src = ctx.add_local(WasmValType::I32);
    set_local(out, src);
    // len = load [src + LIST_LEN_OFF], cap = load [src + LIST_CAP_OFF]
    let len = ctx.add_local(WasmValType::I32);
    get_local(out, src);
    emit_load_at(WasmValType::I32, LIST_LEN_OFF, out);
    set_local(out, len);
    let cap = ctx.add_local(WasmValType::I32);
    get_local(out, src);
    emit_load_at(WasmValType::I32, LIST_CAP_OFF, out);
    set_local(out, cap);
    // dst = __alloc(LIST_DATA_OFF + cap * SLOT_SIZE)
    emit_list_block_size(cap, out);
    let dst = alloc_runtime(ctx, out);
    // dst.len = len; dst.cap = cap
    get_local(out, dst);
    get_local(out, len);
    emit_store_at(WasmValType::I32, LIST_LEN_OFF, out);
    get_local(out, dst);
    get_local(out, cap);
    emit_store_at(WasmValType::I32, LIST_CAP_OFF, out);
    // copy the `len` live element slots
    emit_list_copy_elems(ctx, src, dst, len, out);
    get_local(out, dst);
    Ok(())
}

/// Lower `list_new() -> list<T>`: `__alloc` an empty `[len=0][cap=LIST_INITIAL_CAP]
/// [slots...]` block and leave its pointer on the stack. Allocating a small initial
/// capacity means the first few `push`es do not each realloc.
fn lower_list_new(ctx: &mut LowerCtx, args: &[IrExpr], out: &mut Vec<u8>) -> Result<(), String> {
    if !args.is_empty() {
        return Err(format!("list_new expects 0 arguments, got {}", args.len()));
    }
    let ptr = alloc_bytes(ctx, LIST_DATA_OFF + LIST_INITIAL_CAP * SLOT_SIZE, out);
    get_local(out, ptr);
    out.push(0x41); // i32.const 0 (len)
    write_sleb(out, 0);
    emit_store_at(WasmValType::I32, LIST_LEN_OFF, out);
    get_local(out, ptr);
    out.push(0x41); // i32.const LIST_INITIAL_CAP (cap)
    write_sleb(out, LIST_INITIAL_CAP as i64);
    emit_store_at(WasmValType::I32, LIST_CAP_OFF, out);
    get_local(out, ptr);
    Ok(())
}

/// Lower `push(l, x) -> list<T>` (value-semantic append): deep-copy `l`, grow the
/// copy when it is full (double the capacity, or seed `LIST_INITIAL_CAP` from an
/// empty list, reallocating and copying the live elements — the old block is
/// orphaned in the no-reclaim bump heap), store `x` into slot `len`, bump `len`,
/// and leave the fresh list pointer on the stack. Because `push` always returns a
/// NEW list, `l = push(l, x)` matches the interpreters' `Value::clone`-then-append.
fn lower_list_push(
    ctx: &mut LowerCtx,
    list: &IrExpr,
    value: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = supported_list_element(&list.ty).ok_or_else(|| {
        format!(
            "push expects a scalar-element list but got `{}`",
            list.ty.name
        )
    })?;
    let slot_ty = scalar_val_type(&elem_ty)
        .ok_or_else(|| format!("list element type `{}` is unsupported", elem_ty.name))?;
    // Deep-copy the source list into a fresh, independent block (value semantics).
    lower_expr(ctx, list, out)?;
    emit_list_deep_copy(ctx, out)?;
    let lst = ctx.add_local(WasmValType::I32);
    set_local(out, lst);
    // Grow if len == cap.
    let len = ctx.add_local(WasmValType::I32);
    get_local(out, lst);
    emit_load_at(WasmValType::I32, LIST_LEN_OFF, out);
    set_local(out, len);
    let cap = ctx.add_local(WasmValType::I32);
    get_local(out, lst);
    emit_load_at(WasmValType::I32, LIST_CAP_OFF, out);
    set_local(out, cap);
    // if len >= cap { grow }
    get_local(out, len);
    get_local(out, cap);
    out.push(0x4e); // i32.ge_s
    out.push(0x04); // if
    out.push(0x40); // void block type
    emit_list_grow(ctx, lst, len, cap, out);
    out.push(0x0b); // end if
    // slot address of element `len`: lst + LIST_DATA_OFF + len * SLOT_SIZE
    get_local(out, lst);
    get_local(out, len);
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul
    out.push(0x41); // i32.const LIST_DATA_OFF
    write_sleb(out, LIST_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    out.push(0x6a); // i32.add -> element slot address
    lower_expr(ctx, value, out)?; // the value to append
    emit_store(slot_ty, out);
    // len += 1
    get_local(out, lst);
    get_local(out, len);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    emit_store_at(WasmValType::I32, LIST_LEN_OFF, out);
    get_local(out, lst);
    Ok(())
}

/// Grow the list in `lst` (an `i32` local) so it has room past `len` (== `cap`):
/// compute `new_cap = if cap == 0 { LIST_INITIAL_CAP } else { cap * 2 }`, `__alloc`
/// a fresh block, copy the `len` live elements, write the new `cap` and preserved
/// `len`, and update `lst` to the new pointer. `len` and `cap` locals are refreshed
/// so the caller sees the post-grow capacity; the old block is orphaned.
fn emit_list_grow(ctx: &mut LowerCtx, lst: u32, len: u32, cap: u32, out: &mut Vec<u8>) {
    // new_cap = cap == 0 ? LIST_INITIAL_CAP : cap * 2
    let new_cap = ctx.add_local(WasmValType::I32);
    get_local(out, cap);
    out.push(0x45); // i32.eqz
    out.push(0x04); // if -> i32
    out.push(0x7f); // result i32
    out.push(0x41); // i32.const LIST_INITIAL_CAP
    write_sleb(out, LIST_INITIAL_CAP as i64);
    out.push(0x05); // else
    get_local(out, cap);
    out.push(0x41); // i32.const 2
    write_sleb(out, 2);
    out.push(0x6c); // i32.mul
    out.push(0x0b); // end if
    set_local(out, new_cap);
    // dst = __alloc(LIST_DATA_OFF + new_cap * SLOT_SIZE)
    emit_list_block_size(new_cap, out);
    let dst = alloc_runtime(ctx, out);
    // dst.len = len; dst.cap = new_cap
    get_local(out, dst);
    get_local(out, len);
    emit_store_at(WasmValType::I32, LIST_LEN_OFF, out);
    get_local(out, dst);
    get_local(out, new_cap);
    emit_store_at(WasmValType::I32, LIST_CAP_OFF, out);
    // copy the `len` live elements from the old block (lst) to dst
    emit_list_copy_elems(ctx, lst, dst, len, out);
    // lst = dst; cap = new_cap (len is unchanged)
    get_local(out, dst);
    set_local(out, lst);
    get_local(out, new_cap);
    set_local(out, cap);
}

/// Lower `get(l, i) -> T`: load element `i` from `l + LIST_DATA_OFF + i*SLOT_SIZE`.
/// The interpreters bounds-check and raise `L0413`; the WASM backend relies on
/// linear-memory trapping for a truly out-of-range index, so in-bounds reads match
/// the interpreters exactly and an OOB read traps (a consistent, documented
/// behavior) instead of returning a poisoned value.
fn lower_list_get(
    ctx: &mut LowerCtx,
    list: &IrExpr,
    index: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = supported_list_element(&list.ty).ok_or_else(|| {
        format!(
            "get expects a scalar-element list but got `{}`",
            list.ty.name
        )
    })?;
    let slot_ty = scalar_val_type(&elem_ty)
        .ok_or_else(|| format!("list element type `{}` is unsupported", elem_ty.name))?;
    lower_expr(ctx, list, out)?; // base pointer
    emit_list_elem_offset(ctx, index, out)?; // += LIST_DATA_OFF + index * SLOT_SIZE
    emit_load(slot_ty, out);
    Ok(())
}

/// Lower `set(l, i, x) -> list<T>` (value-semantic replace): deep-copy `l`, store
/// `x` into element slot `i` of the copy, and leave the fresh list pointer on the
/// stack. In-bounds writes match the interpreters; an OOB index traps on the
/// linear-memory store, consistent with `get`.
fn lower_list_set(
    ctx: &mut LowerCtx,
    list: &IrExpr,
    index: &IrExpr,
    value: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = supported_list_element(&list.ty).ok_or_else(|| {
        format!(
            "set expects a scalar-element list but got `{}`",
            list.ty.name
        )
    })?;
    let slot_ty = scalar_val_type(&elem_ty)
        .ok_or_else(|| format!("list element type `{}` is unsupported", elem_ty.name))?;
    lower_expr(ctx, list, out)?;
    emit_list_deep_copy(ctx, out)?;
    let lst = ctx.add_local(WasmValType::I32);
    set_local(out, lst);
    // element slot address in the copy
    get_local(out, lst);
    emit_list_elem_offset(ctx, index, out)?;
    lower_expr(ctx, value, out)?;
    emit_store(slot_ty, out);
    get_local(out, lst);
    Ok(())
}

/// Lower `pop(l) -> list<T>` (value-semantic remove-last): deep-copy `l`, decrement
/// the copy's `len` (dropping the last element in place — the slot stays allocated,
/// exactly like the interpreters' `Vec::pop` shrinks the length), and leave the
/// fresh list pointer on the stack. Popping an empty list is `L0413` on the
/// interpreters; the WASM path decrements to `-1` len, so the enclosing program is
/// expected to keep the same non-empty precondition the interpreters require.
fn lower_list_pop(ctx: &mut LowerCtx, list: &IrExpr, out: &mut Vec<u8>) -> Result<(), String> {
    supported_list_element(&list.ty).ok_or_else(|| {
        format!(
            "pop expects a scalar-element list but got `{}`",
            list.ty.name
        )
    })?;
    lower_expr(ctx, list, out)?;
    emit_list_deep_copy(ctx, out)?;
    let lst = ctx.add_local(WasmValType::I32);
    set_local(out, lst);
    // len -= 1
    get_local(out, lst);
    get_local(out, lst);
    emit_load_at(WasmValType::I32, LIST_LEN_OFF, out);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6b); // i32.sub
    emit_store_at(WasmValType::I32, LIST_LEN_OFF, out);
    get_local(out, lst);
    Ok(())
}

/// After a list base pointer is on the stack, add `LIST_DATA_OFF + index*SLOT_SIZE`
/// so the top of stack is the element slot address. The `index` expression is an
/// `i64`, truncated to `i32` (`i32.wrap_i64`) exactly like array indexing.
fn emit_list_elem_offset(
    ctx: &mut LowerCtx,
    index: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    lower_expr(ctx, index, out)?; // index (i64)
    out.push(0xa7); // i32.wrap_i64
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul -> index * SLOT_SIZE
    out.push(0x41); // i32.const LIST_DATA_OFF
    write_sleb(out, LIST_DATA_OFF as i64);
    out.push(0x6a); // i32.add -> LIST_DATA_OFF + index * SLOT_SIZE
    out.push(0x6a); // i32.add base + that offset
    Ok(())
}

/// Copy one struct field slot from `src + offset` to `dst + offset` (both are
/// `i32` local indices holding base pointers; `offset` is a constant byte offset).
/// A scalar/string slot is copied word-for-word (by its own WASM slot type); a
/// nested MUTABLE aggregate slot is deep-copied so nested mutation stays isolated.
fn emit_copy_slot(
    ctx: &mut LowerCtx,
    slot_ir_ty: &TypeRef,
    src: u32,
    dst: u32,
    offset: i32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let slot_ty = slot_val_type(slot_ir_ty, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("slot type `{}` is unsupported", slot_ir_ty.name))?;
    get_local(out, dst); // base for the store
    if is_mutable_aggregate(slot_ir_ty, ctx.structs, ctx.enums) {
        // Load the nested pointer, deep-copy it, store the fresh pointer.
        get_local(out, src);
        emit_load_at(WasmValType::I32, offset, out);
        emit_deep_copy(ctx, slot_ir_ty, out)?;
    } else {
        // Scalar or immutable string: copy the slot word by its own type.
        get_local(out, src);
        emit_load_at(slot_ty, offset, out);
    }
    emit_store_at(slot_ty, offset, out);
    Ok(())
}

/// Copy one array element from `src + off` to `dst + off`, where `off` is an `i32`
/// local holding the runtime byte offset (`LEN_HEADER + i * SLOT_SIZE`). A
/// scalar/string element is copied word-for-word; a nested MUTABLE aggregate
/// element is deep-copied. Mirrors [`emit_copy_slot`] but with a runtime offset.
fn emit_copy_element_at(
    ctx: &mut LowerCtx,
    elem_ir_ty: &TypeRef,
    src: u32,
    dst: u32,
    off: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let slot_ty = slot_val_type(elem_ir_ty, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("array element type `{}` is unsupported", elem_ir_ty.name))?;
    // dst_addr = dst + off
    get_local(out, dst);
    get_local(out, off);
    out.push(0x6a); // i32.add
    if is_mutable_aggregate(elem_ir_ty, ctx.structs, ctx.enums) {
        // src_addr = src + off; load the nested pointer; deep-copy; store fresh.
        get_local(out, src);
        get_local(out, off);
        out.push(0x6a); // i32.add
        emit_load(WasmValType::I32, out);
        emit_deep_copy(ctx, elem_ir_ty, out)?;
    } else {
        // src_addr = src + off; load the element word by its own type.
        get_local(out, src);
        get_local(out, off);
        out.push(0x6a); // i32.add
        emit_load(slot_ty, out);
    }
    emit_store(slot_ty, out);
    Ok(())
}

/// The `(byte offset, slot WASM type)` of a struct field, given the struct's
/// type and the field name.
fn struct_field_slot(
    ctx: &LowerCtx,
    struct_ty: &TypeRef,
    field: &str,
) -> Result<(i32, WasmValType), String> {
    let fields = ctx
        .structs
        .get(&struct_ty.name)
        .ok_or_else(|| format!("`{}` is not a struct", struct_ty.name))?;
    let position = fields
        .iter()
        .position(|(name, _)| name == field)
        .ok_or_else(|| format!("unknown field `{field}` on `{}`", struct_ty.name))?;
    let slot_ty = slot_val_type(&fields[position].1, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("field `{field}` has an unsupported type"))?;
    Ok((position as i32 * SLOT_SIZE, slot_ty))
}

/// Given a base pointer already on the stack, add `LEN_HEADER + index*SLOT_SIZE`
/// so the top of stack is the element slot address. The `index` expression is an
/// `i64`; it is truncated to `i32` for the address arithmetic.
fn lower_array_slot_offset(
    ctx: &mut LowerCtx,
    index: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // offset = LEN_HEADER + index * SLOT_SIZE (index is i64 -> i32).
    lower_expr(ctx, index, out)?;
    out.push(0xa7); // i32.wrap_i64
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul
    out.push(0x41); // i32.const LEN_HEADER
    write_sleb(out, LEN_HEADER as i64);
    out.push(0x6a); // i32.add
    out.push(0x6a); // i32.add  (base + offset)
    Ok(())
}

/// Fold one assignment-path hop into the running address on the stack, returning
/// the hop's leaf IR type. On entry the current base/element pointer is on the
/// stack; on exit the slot address for this hop is on the stack.
fn lower_place_address(
    ctx: &mut LowerCtx,
    cur_ty: &TypeRef,
    place: &crate::IrPlace,
    out: &mut Vec<u8>,
) -> Result<TypeRef, String> {
    match place {
        crate::IrPlace::Field(field) => {
            let (offset, _) = struct_field_slot(ctx, cur_ty, field)?;
            if offset != 0 {
                out.push(0x41); // i32.const offset
                write_sleb(out, offset as i64);
                out.push(0x6a); // i32.add
            }
            let fields = ctx
                .structs
                .get(&cur_ty.name)
                .ok_or_else(|| format!("`{}` is not a struct", cur_ty.name))?;
            let field_ty = fields
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, ty)| ty.clone())
                .ok_or_else(|| format!("unknown field `{field}`"))?;
            Ok(field_ty)
        }
        crate::IrPlace::Index(index) => {
            let elem_ty = cur_ty
                .array_element()
                .ok_or_else(|| format!("indexing a non-array type `{}`", cur_ty.name))?;
            lower_array_slot_offset(ctx, index, out)?;
            Ok(elem_ty)
        }
    }
}

/// A non-mid-path store at a base pointer already on the stack followed by the
/// value: `emit_store` picks the opcode. Alignment `2` = 4-byte for `i32`, `3` =
/// 8-byte for `i64`/`f64` (offset 0).
fn emit_store(ty: WasmValType, out: &mut Vec<u8>) {
    emit_store_at(ty, 0, out);
}

/// Store the value on the stack (with the base pointer pushed just before it) at
/// `base + offset`.
fn emit_store_at(ty: WasmValType, offset: i32, out: &mut Vec<u8>) {
    let (opcode, align) = match ty {
        WasmValType::I32 => (0x36u8, 2u64), // i32.store
        WasmValType::I64 => (0x37, 3),      // i64.store
        WasmValType::F32 => (0x38, 2),      // f32.store (4-byte)
        WasmValType::F64 => (0x39, 3),      // f64.store
    };
    out.push(opcode);
    write_uleb(out, align);
    write_uleb(out, offset as u64);
}

/// Load a slot value from the address on the stack.
fn emit_load(ty: WasmValType, out: &mut Vec<u8>) {
    emit_load_at(ty, 0, out);
}

/// Load a slot value from `base + offset` (base pointer on the stack).
fn emit_load_at(ty: WasmValType, offset: i32, out: &mut Vec<u8>) {
    let (opcode, align) = match ty {
        WasmValType::I32 => (0x28u8, 2u64), // i32.load
        WasmValType::I64 => (0x29, 3),      // i64.load
        WasmValType::F32 => (0x2a, 2),      // f32.load (4-byte)
        WasmValType::F64 => (0x2b, 3),      // f64.load
    };
    out.push(opcode);
    write_uleb(out, align);
    write_uleb(out, offset as u64);
}

/// Emit the opcode(s) for a binary op given the operand WASM type.
fn emit_binary_op_typed(op: BinaryOp, ty: WasmValType, out: &mut Vec<u8>) -> Result<(), String> {
    match ty {
        WasmValType::I64 => emit_i64_binop(op, out),
        WasmValType::F32 => emit_f32_binop(op, out),
        WasmValType::F64 => emit_f64_binop(op, out),
        WasmValType::I32 => emit_i32_binop(op, out),
    }
}

fn emit_i64_binop(op: BinaryOp, out: &mut Vec<u8>) -> Result<(), String> {
    let opcode = match op {
        BinaryOp::Add => 0x7c,
        BinaryOp::Subtract => 0x7d,
        BinaryOp::Multiply => 0x7e,
        BinaryOp::Divide => 0x7f, // i64.div_s (traps on 0)
        BinaryOp::Equal => 0x51,
        BinaryOp::NotEqual => 0x52,
        BinaryOp::Less => 0x53,         // lt_s
        BinaryOp::LessEqual => 0x57,    // le_s
        BinaryOp::Greater => 0x55,      // gt_s
        BinaryOp::GreaterEqual => 0x59, // ge_s
        // `and`/`or` short-circuit and the integer bitwise ops are deferred on
        // this backend; both are routed away before reaching this opcode table.
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr => unreachable!("handled by caller"),
    };
    out.push(opcode);
    Ok(())
}

/// Emit a signed `i64` division that matches the interpreters' wrapping
/// semantics on the one overflow case `i64::MIN / -1`. WASM `i64.div_s` traps on
/// that input (as well as on a zero divisor), but the interpreters use
/// `wrapping_div`, which yields `i64::MIN`. Operands (dividend then divisor) must
/// already be on the stack. Stash both, and when the divisor is `-1` compute
/// `0 - dividend` — wrapping negation is exactly `x / -1` across the whole `i64`
/// range, including `i64::MIN / -1 == i64::MIN`. Otherwise divide normally, which
/// still traps on a zero divisor exactly like before (division-by-zero behavior
/// is unchanged). Used for both the plain-`i64` and fixed-width signed division
/// paths, so the WASM backend stays bit-for-bit with the interpreters and the
/// native backend.
fn emit_i64_signed_div_guarded(ctx: &mut LowerCtx, out: &mut Vec<u8>) {
    let divisor = ctx.add_local(WasmValType::I64);
    let dividend = ctx.add_local(WasmValType::I64);
    // Stack holds [dividend, divisor]; pop them into locals (divisor is on top).
    set_local(out, divisor);
    set_local(out, dividend);
    // divisor == -1 ?
    get_local(out, divisor);
    out.push(0x42); // i64.const -1
    write_sleb(out, -1);
    out.push(0x51); // i64.eq
    out.push(0x04); // if
    out.push(0x7e); // block type: i64 result
    // then: 0 - dividend (wrapping negation == dividend / -1)
    out.push(0x42); // i64.const 0
    write_sleb(out, 0);
    get_local(out, dividend);
    out.push(0x7d); // i64.sub
    out.push(0x05); // else
    // else: dividend / divisor (traps only on a zero divisor)
    get_local(out, dividend);
    get_local(out, divisor);
    out.push(0x7f); // i64.div_s
    out.push(0x0b); // end
}

fn emit_f64_binop(op: BinaryOp, out: &mut Vec<u8>) -> Result<(), String> {
    let opcode = match op {
        BinaryOp::Add => 0xa0,
        BinaryOp::Subtract => 0xa1,
        BinaryOp::Multiply => 0xa2,
        BinaryOp::Divide => 0xa3,
        BinaryOp::Equal => 0x61,
        BinaryOp::NotEqual => 0x62,
        BinaryOp::Less => 0x63,
        BinaryOp::LessEqual => 0x65,
        BinaryOp::Greater => 0x64,
        BinaryOp::GreaterEqual => 0x66,
        // `and`/`or` short-circuit and the integer bitwise ops are deferred on
        // this backend; both are routed away before reaching this opcode table.
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr => unreachable!("handled by caller"),
    };
    out.push(opcode);
    Ok(())
}

/// Emit a single-precision `f32` binary op. Arithmetic (`f32.add/sub/mul/div`)
/// keeps the result in single precision, so it is bit-identical to the
/// interpreter's real `f32`. The comparison ops (`f32.eq/ne/lt/le/gt/ge`) are
/// IEEE-754: relational compares and `==` are false when either operand is NaN,
/// `!=` is true — exactly the interpreter's (Rust `f32`) semantics.
fn emit_f32_binop(op: BinaryOp, out: &mut Vec<u8>) -> Result<(), String> {
    let opcode = match op {
        BinaryOp::Add => 0x92,
        BinaryOp::Subtract => 0x93,
        BinaryOp::Multiply => 0x94,
        BinaryOp::Divide => 0x95,
        BinaryOp::Equal => 0x5b,
        BinaryOp::NotEqual => 0x5c,
        BinaryOp::Less => 0x5d,
        BinaryOp::Greater => 0x5e,
        BinaryOp::LessEqual => 0x5f,
        BinaryOp::GreaterEqual => 0x60,
        // `and`/`or` short-circuit and the integer bitwise ops are deferred on
        // this backend; both are routed away before reaching this opcode table.
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr => unreachable!("handled by caller"),
    };
    out.push(opcode);
    Ok(())
}

/// `i32` operands are `bool`/`char`/`byte`. Comparisons use the signed opcodes;
/// arithmetic is supported defensively.
fn emit_i32_binop(op: BinaryOp, out: &mut Vec<u8>) -> Result<(), String> {
    let opcode = match op {
        BinaryOp::Add => 0x6a,
        BinaryOp::Subtract => 0x6b,
        BinaryOp::Multiply => 0x6c,
        BinaryOp::Divide => 0x6d, // i32.div_s
        BinaryOp::Equal => 0x46,
        BinaryOp::NotEqual => 0x47,
        BinaryOp::Less => 0x48,         // lt_s
        BinaryOp::LessEqual => 0x4c,    // le_s
        BinaryOp::Greater => 0x4a,      // gt_s
        BinaryOp::GreaterEqual => 0x4e, // ge_s
        // `and`/`or` short-circuit and the integer bitwise ops are deferred on
        // this backend; both are routed away before reaching this opcode table.
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr => unreachable!("handled by caller"),
    };
    out.push(opcode);
    Ok(())
}

/// The WASM value type an expression leaves on the stack, using the IR's type
/// annotation. `None` for a `void` expression. A pointer type (string/struct/
/// array) reports `i32`.
fn expr_val_type(ctx: &LowerCtx, expr: &IrExpr) -> Result<Option<WasmValType>, String> {
    if expr.ty.is_void() {
        return Ok(None);
    }
    if let Some(vt) = value_val_type(&expr.ty, ctx.structs, ctx.enums) {
        return Ok(Some(vt));
    }
    Err(format!(
        "expression has unsupported type `{}`",
        expr.ty.name
    ))
}

/// Whether a non-void function body always leaves a value / returns on every
/// path. Conservative: accept a trailing `Return(Some)`, a value-producing tail
/// `Expr`, an exhaustive `If` whose branches all guarantee a value, an exhaustive
/// `Match` whose arms all guarantee a value, or a `loop` whose body contains a
/// `Return`.
fn body_guarantees_value(body: &[IrStmt]) -> bool {
    match body.last() {
        Some(IrStmt::Return(Some(_))) => true,
        Some(IrStmt::Expr(expr)) => !expr.ty.is_void(),
        Some(IrStmt::If {
            branches,
            else_body,
            ..
        }) => {
            !else_body.is_empty()
                && body_guarantees_value(else_body)
                && branches.iter().all(|b| body_guarantees_value(&b.body))
        }
        // A `match` is exhaustive (semantics enforces it), so it guarantees a
        // value iff every arm body does. This is what makes a `match`-tail
        // function like `fn count o option<i64> -> i64` eligible.
        Some(IrStmt::Match { arms, .. }) => {
            !arms.is_empty() && arms.iter().all(|arm| body_guarantees_value(&arm.body))
        }
        Some(IrStmt::Loop { body, .. }) => stmts_contain_return(body),
        _ => false,
    }
}

fn stmts_contain_return(stmts: &[IrStmt]) -> bool {
    stmts.iter().any(|s| match s {
        IrStmt::Return(_) => true,
        IrStmt::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().any(|b| stmts_contain_return(&b.body))
                || stmts_contain_return(else_body)
        }
        IrStmt::Match { arms, .. } => arms.iter().any(|arm| stmts_contain_return(&arm.body)),
        IrStmt::While { body, .. } | IrStmt::Loop { body, .. } | IrStmt::For { body, .. } => {
            stmts_contain_return(body)
        }
        _ => false,
    })
}

// -- Local get/set helpers ---------------------------------------------------

fn get_local(out: &mut Vec<u8>, index: u32) {
    out.push(0x20);
    write_uleb(out, index as u64);
}

fn set_local(out: &mut Vec<u8>, index: u32) {
    out.push(0x21);
    write_uleb(out, index as u64);
}

// -- Binary encoder ----------------------------------------------------------

/// Unsigned LEB128.
fn write_uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Signed LEB128.
fn write_sleb(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7; // arithmetic shift
        let sign_bit = byte & 0x40;
        let done = (value == 0 && sign_bit == 0) || (value == -1 && sign_bit != 0);
        if !done {
            byte |= 0x80;
        }
        out.push(byte);
        if done {
            break;
        }
    }
}

/// A distinct signature (parameters + optional result). Functions with the same
/// signature share a type-section entry.
#[derive(Clone, PartialEq, Eq)]
struct FuncType {
    params: Vec<WasmValType>,
    result: Option<WasmValType>,
}

/// The internal, non-exported bump-allocator helper `__alloc(size i32) -> i32`.
/// It reads the mutable bump-pointer global, advances it by `size`, and returns
/// the old value (the freshly allocated offset). Struct/array construction calls
/// it to reserve their layout in linear memory.
fn alloc_helper() -> LoweredFunction {
    let mut body = Vec::new();
    body.push(0x23); // global.get
    write_uleb(&mut body, BUMP_GLOBAL_INDEX as u64); // old bump = return value
    body.push(0x23); // global.get
    write_uleb(&mut body, BUMP_GLOBAL_INDEX as u64);
    get_local(&mut body, 0); // size (param 0)
    body.push(0x6a); // i32.add
    body.push(0x24); // global.set
    write_uleb(&mut body, BUMP_GLOBAL_INDEX as u64);
    LoweredFunction {
        name: ALLOC_HELPER_NAME.to_string(),
        params: vec![WasmValType::I32],
        result: Some(WasmValType::I32),
        extra_locals: Vec::new(),
        body,
    }
}

/// The `env`-module field names of the imported host functions, in the order
/// that fixes their low WASM function indices (must match `*_FUNC_INDEX`).
const IMPORT_FIELD_NAMES: [&str; IMPORT_FUNC_COUNT as usize] =
    ["log_i64", "console_log", "dom_set_text"];

/// The signatures of the imported host functions, one per low function index,
/// reserved as the leading entries of the Type section so the Import section can
/// reference them by their index.
fn import_func_types() -> Vec<FuncType> {
    vec![
        // 0: env.log_i64(i64) -> void
        FuncType {
            params: vec![WasmValType::I64],
            result: None,
        },
        // 1: env.console_log(ptr i32, len i32) -> void
        FuncType {
            params: vec![WasmValType::I32, WasmValType::I32],
            result: None,
        },
        // 2: env.dom_set_text(id_ptr i32, id_len i32, text_ptr i32, text_len i32) -> void
        FuncType {
            params: vec![
                WasmValType::I32,
                WasmValType::I32,
                WasmValType::I32,
                WasmValType::I32,
            ],
            result: None,
        },
    ]
}

/// Index of the mutable `i32` bump-pointer global.
const BUMP_GLOBAL_INDEX: u32 = 0;

/// Export name of the internal bump-allocator helper. It is distinct from any
/// Lullaby identifier (double underscore prefix) so it cannot collide.
const ALLOC_HELPER_NAME: &str = "__alloc";

/// Encode the whole module: header + Type, Import, Function, Memory, Global,
/// Export, Code, and Data sections.
///
/// The imports (`env.log_i64`, `env.console_log`, `env.dom_set_text`) occupy WASM
/// function indices `0..IMPORT_FUNC_COUNT`, so every internally-defined function
/// is numbered from `IMPORT_FUNC_COUNT` up; the caller already assigned those
/// shifted indices. The internal `__alloc` helper
/// is appended after the user functions. `pool` supplies the interned
/// string-literal bytes seeded into the Data section and fixes the bump global's
/// initial value (past the reserved region and the whole literal pool).
fn encode_module(user_functions: &[LoweredFunction], pool: &StringPool) -> Vec<u8> {
    // All internally-defined functions, in module (index) order: the compiled
    // user functions, then the bump-allocator helper.
    let mut functions: Vec<LoweredFunction> = user_functions.to_vec();
    functions.push(alloc_helper());

    // Type table. Entries 0..IMPORT_FUNC_COUNT are reserved for the imported host
    // functions' signatures (so the Import section references them by index);
    // internal functions dedup against the whole table.
    //   0: env.log_i64      (i64) -> void
    //   1: env.console_log  (i32, i32) -> void
    //   2: env.dom_set_text (i32, i32, i32, i32) -> void
    let mut types: Vec<FuncType> = import_func_types();
    let mut type_of_func: Vec<u32> = Vec::with_capacity(functions.len());
    for f in &functions {
        let sig = FuncType {
            params: f.params.clone(),
            result: f.result,
        };
        let idx = match types.iter().position(|t| *t == sig) {
            Some(i) => i as u32,
            None => {
                types.push(sig);
                (types.len() - 1) as u32
            }
        };
        type_of_func.push(idx);
    }

    let mut module = Vec::new();
    // Magic + version.
    module.extend_from_slice(&[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);

    // Type section (id 1).
    {
        let mut section = Vec::new();
        write_uleb(&mut section, types.len() as u64);
        for t in &types {
            section.push(0x60); // func type
            write_uleb(&mut section, t.params.len() as u64);
            for p in &t.params {
                section.push(p.byte());
            }
            match t.result {
                Some(vt) => {
                    write_uleb(&mut section, 1);
                    section.push(vt.byte());
                }
                None => write_uleb(&mut section, 0),
            }
        }
        push_section(&mut module, 1, &section);
    }

    // Import section (id 2): the host functions from module `env`, in the fixed
    // order that defines their low WASM function indices (0, 1, 2). Each
    // references the reserved type-table entry with the same index.
    {
        let mut section = Vec::new();
        write_uleb(&mut section, IMPORT_FUNC_COUNT as u64);
        for (i, field) in IMPORT_FIELD_NAMES.iter().enumerate() {
            write_name(&mut section, "env");
            write_name(&mut section, field);
            section.push(0x00); // import kind: func
            write_uleb(&mut section, i as u64); // reserved type index
        }
        push_section(&mut module, 2, &section);
    }

    // Function section (id 3): type index per internal function.
    {
        let mut section = Vec::new();
        write_uleb(&mut section, functions.len() as u64);
        for &t in &type_of_func {
            write_uleb(&mut section, t as u64);
        }
        push_section(&mut module, 3, &section);
    }

    // Memory section (id 5): one memory, min 1 page, no maximum.
    {
        let mut section = Vec::new();
        write_uleb(&mut section, 1); // one memory
        section.push(0x00); // limits: flag 0 = min only
        write_uleb(&mut section, 1); // min 1 page (64 KiB)
        push_section(&mut module, 5, &section);
    }

    // Global section (id 6): the mutable `i32` bump pointer, initialized past the
    // reserved region AND the string-literal pool so `__alloc` never overwrites
    // static string data.
    {
        let mut section = Vec::new();
        write_uleb(&mut section, 1); // one global
        section.push(WasmValType::I32.byte()); // value type i32
        section.push(0x01); // mutable
        section.push(0x41); // i32.const (init expr)
        write_sleb(&mut section, pool.heap_base() as i64);
        section.push(0x0b); // end init expr
        push_section(&mut module, 6, &section);
    }

    // Export section (id 7): the linear memory, then every internal function by
    // name. Function export indices are the shifted (post-import) indices.
    {
        let mut section = Vec::new();
        write_uleb(&mut section, (functions.len() + 1) as u64); // +1 for memory
        write_name(&mut section, "memory");
        section.push(0x02); // export kind: mem
        write_uleb(&mut section, 0); // memory index 0
        for (i, f) in functions.iter().enumerate() {
            write_name(&mut section, &f.name);
            section.push(0x00); // export kind: func
            write_uleb(&mut section, IMPORT_FUNC_COUNT as u64 + i as u64);
        }
        push_section(&mut module, 7, &section);
    }

    // Code section (id 10).
    {
        let mut section = Vec::new();
        write_uleb(&mut section, functions.len() as u64);
        for f in &functions {
            let mut code = Vec::new();
            // Locals: run-length compressed consecutive same-type runs.
            let runs = compress_locals(&f.extra_locals);
            write_uleb(&mut code, runs.len() as u64);
            for (count, ty) in runs {
                write_uleb(&mut code, count as u64);
                code.push(ty.byte());
            }
            code.extend_from_slice(&f.body);
            code.push(0x0b); // end
            write_uleb(&mut section, code.len() as u64);
            section.extend_from_slice(&code);
        }
        push_section(&mut module, 10, &section);
    }

    // Data section (id 11): one active segment at offset 0 seeding the reserved
    // region [0, RESERVED_BASE) with zeros (so a handed-out pointer is never null)
    // followed by the interned string-literal pool starting at `RESERVED_BASE`.
    {
        let mut segment = vec![0u8; RESERVED_BASE as usize];
        segment.extend_from_slice(&pool.bytes);

        let mut section = Vec::new();
        write_uleb(&mut section, 1); // one data segment
        section.push(0x00); // segment kind 0: active, memory 0, offset expr
        section.push(0x41); // i32.const (offset expr)
        write_sleb(&mut section, 0);
        section.push(0x0b); // end offset expr
        write_uleb(&mut section, segment.len() as u64);
        section.extend_from_slice(&segment);
        push_section(&mut module, 11, &section);
    }

    module
}

/// Write a WASM name: length-prefixed UTF-8 bytes.
fn write_name(out: &mut Vec<u8>, name: &str) {
    let bytes = name.as_bytes();
    write_uleb(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// Run-length compress a local declaration list into `(count, type)` runs.
fn compress_locals(locals: &[WasmValType]) -> Vec<(u32, WasmValType)> {
    let mut runs: Vec<(u32, WasmValType)> = Vec::new();
    for &ty in locals {
        match runs.last_mut() {
            Some((count, last)) if *last == ty => *count += 1,
            _ => runs.push((1, ty)),
        }
    }
    runs
}

/// Append a section: `id`, byte length, then the section contents.
fn push_section(module: &mut Vec<u8>, id: u8, contents: &[u8]) {
    module.push(id);
    write_uleb(module, contents.len() as u64);
    module.extend_from_slice(contents);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IrEnumVariant, lower};
    use lullaby_lexer::lex;
    use lullaby_parser::parse;
    use lullaby_semantics::validate;

    fn module_for(source: &str) -> IrModule {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let checked = validate(&program).expect("semantic");
        lower(&checked).expect("lower")
    }

    #[test]
    fn header_is_wasm_magic_and_version() {
        let source = "fn add a i64 b i64 -> i64\n    a + b\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(
            &artifact.bytes[0..8],
            &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]
        );
        assert_eq!(artifact.compiled, vec!["add".to_string()]);
        assert!(artifact.skipped.is_empty());
    }

    #[test]
    fn expected_sections_are_present() {
        let source = "fn add a i64 b i64 -> i64\n    a + b\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        let ids = section_ids(&artifact.bytes);
        assert_eq!(
            ids,
            vec![1, 2, 3, 5, 6, 7, 10, 11],
            "type/import/function/memory/global/export/code/data sections in canonical order"
        );
    }

    #[test]
    fn imports_the_host_functions() {
        // The Import section (id 2) declares the three host imports: the log
        // primitive and the JS/DOM interop primitives.
        let source = "fn add a i64 b i64 -> i64\n    a + b\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        let import = section_body(&artifact.bytes, 2).expect("import section");
        let (count, _) = read_uleb(&import);
        assert_eq!(count, 3, "three host imports");
        // The import names include `env`, `log_i64`, `console_log`, `dom_set_text`.
        assert!(
            find_subslice(&import, b"env").is_some()
                && find_subslice(&import, b"log_i64").is_some()
                && find_subslice(&import, b"console_log").is_some()
                && find_subslice(&import, b"dom_set_text").is_some(),
            "env host import names present"
        );
    }

    #[test]
    fn function_section_counts_internal_functions() {
        // Two user functions plus the internal `__alloc` helper => 3 entries in
        // the Function section; the host imports are NOT counted there.
        let source =
            "fn add a i64 b i64 -> i64\n    a + b\n\nfn neg n i64 -> i64\n    return 0 - n\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        let func = section_body(&artifact.bytes, 3).expect("function section");
        let (count, _) = read_uleb(&func);
        assert_eq!(count, 3, "two user functions + __alloc helper");
    }

    #[test]
    fn exports_memory_and_functions() {
        let source = "fn add a i64 b i64 -> i64\n    a + b\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        let export = section_body(&artifact.bytes, 7).expect("export section");
        // memory + add + __alloc = 3 exports.
        let (count, _) = read_uleb(&export);
        assert_eq!(count, 3, "memory + add + __alloc exports");
        assert!(
            find_subslice(&export, b"memory").is_some(),
            "memory export present"
        );
        assert!(
            find_subslice(&export, b"__alloc").is_some(),
            "alloc helper export present"
        );
    }

    #[test]
    fn wasm_log_function_compiles_and_calls_the_import() {
        // A function that calls `wasm_log` is eligible; the emitted body contains
        // a `call 0` targeting the imported host function (index 0).
        let source = "fn shout n i64 -> void\n    wasm_log(n)\n    wasm_log(n + 1)\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(artifact.compiled.contains(&"shout".to_string()));
        // The whole module still has the host imports present.
        let import = section_body(&artifact.bytes, 2).expect("import section");
        let (count, _) = read_uleb(&import);
        assert_eq!(count, IMPORT_FUNC_COUNT as u64);
    }

    #[test]
    fn console_log_and_dom_set_text_call_their_imports() {
        // A function that calls the JS/DOM host builtins is eligible; its body
        // targets `env.console_log` (index 1) and `env.dom_set_text` (index 2).
        let source = concat!(
            "fn ui -> void\n",
            "    console_log(\"hi\")\n",
            "    dom_set_text(\"out\", \"done\")\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(artifact.compiled.contains(&"ui".to_string()));
        let code = section_body(&artifact.bytes, 10).expect("code section");
        assert!(
            find_subslice(&code, &[0x10, CONSOLE_LOG_FUNC_INDEX as u8]).is_some(),
            "console_log lowers to a call of its host import"
        );
        assert!(
            find_subslice(&code, &[0x10, DOM_SET_TEXT_FUNC_INDEX as u8]).is_some(),
            "dom_set_text lowers to a call of its host import"
        );
        // The string literals are seeded into the Data section.
        assert!(
            find_subslice(&artifact.bytes, b"hi").is_some()
                && find_subslice(&artifact.bytes, b"out").is_some()
                && find_subslice(&artifact.bytes, b"done").is_some(),
            "interop string literals seeded into the data section"
        );
    }

    #[test]
    fn call_target_indices_are_shifted_past_the_import() {
        // With an import present, a call between two user functions must target
        // the shifted index (import count + position), not the raw position.
        let source = "fn helper -> i64\n    7\n\nfn use_it -> i64\n    return helper()\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(
            artifact.compiled,
            vec!["helper".to_string(), "use_it".to_string()]
        );
        // `helper` is user function 0 => WASM index IMPORT_FUNC_COUNT (past the
        // host imports). The code for `use_it` must contain `call IMPORT_FUNC_COUNT`.
        let code = section_body(&artifact.bytes, 10).expect("code section");
        assert!(
            find_subslice(&code, &[0x10, IMPORT_FUNC_COUNT as u8]).is_some(),
            "call targets the shifted (post-import) index"
        );
    }

    #[test]
    fn scalar_and_nonscalar_split() {
        // `add` is scalar; `tally` returns `map<i64, i64>`, still outside the WASM
        // value set (strings/structs/arrays/enums and scalar-element `list`s are
        // supported; `map` is not), so it is skipped.
        let source = concat!(
            "fn add a i64 b i64 -> i64\n    a + b\n\n",
            "fn tally n i64 -> map<i64, i64>\n    map_set(map_new(), n, n)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["add".to_string()]);
        assert_eq!(artifact.skipped.len(), 1);
        assert_eq!(artifact.skipped[0].name, "tally");
        assert!(artifact.skipped[0].reason.contains("supported"));
    }

    #[test]
    fn string_returning_function_compiles() {
        // A function that takes and returns a `string` is now eligible: strings
        // are `i32` pointers into linear memory.
        let source =
            "fn pick b bool -> string\n    if b\n        return \"yes\"\n    return \"no\"\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["pick".to_string()]);
        // The literal bytes appear in the module's Data section.
        assert!(
            find_subslice(&artifact.bytes, b"yes").is_some()
                && find_subslice(&artifact.bytes, b"no").is_some(),
            "string literals seeded into the data section"
        );
    }

    #[test]
    fn struct_and_array_functions_compile() {
        // A struct constructed/read and a fixed array built/indexed both compile:
        // they lower to `__alloc` + typed loads/stores.
        let source = concat!(
            "struct Point\n    x i64\n    y i64\n\n",
            "fn make a i64 b i64 -> i64\n",
            "    let p Point = Point(a, b)\n",
            "    let xs array<i64> = [a, b, a + b]\n",
            "    p.x + xs[2] + len(xs)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(artifact.compiled.contains(&"make".to_string()));
    }

    #[test]
    fn recursive_function_compiles() {
        let source = "fn fib n i64 -> i64\n    if n < 2\n        return n\n    return fib(n - 1) + fib(n - 2)\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["fib".to_string()]);
    }

    // -- Growable `list<T>` (scalar element) -----------------------------------

    #[test]
    fn scalar_list_function_compiles_with_grow_and_copy_codegen() {
        // A function that builds a growable `list<i64>` via `list_new`/`push`,
        // reads it with `get`/`len`, replaces an element with `set`, and drops one
        // with `pop` must COMPILE to WASM (not skip to the interpreters). The list
        // is an `i32` pointer to a `[len][cap][slots]` block laid out in linear
        // memory, so both the signature (returning `list<i64>`) and the body are
        // eligible.
        let source = concat!(
            "fn build n i64 -> list<i64>\n",
            "    let xs list<i64> = list_new()\n",
            "    xs = push(xs, n)\n",
            "    xs = push(xs, n + 1)\n",
            "    let ys list<i64> = set(xs, 0, n + 2)\n",
            "    let zs list<i64> = pop(ys)\n",
            "    zs\n\n",
            "fn probe n i64 -> i64\n",
            "    let xs list<i64> = build(n)\n",
            "    len(xs) + get(xs, 0)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"build".to_string())
                && artifact.compiled.contains(&"probe".to_string()),
            "list build/probe functions should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());

        let code = section_body(&artifact.bytes, 10).expect("code section");
        // The grow decision `new_cap = cap == 0 ? LIST_INITIAL_CAP : cap * 2`
        // lowers to `i32.eqz` (0x45), then `if` producing an `i32` (0x04 0x7f) —
        // a signature unique to the list-grow path in this backend.
        assert!(
            find_subslice(&code, &[0x45, 0x04, 0x7f]).is_some(),
            "list `push` emits the capacity-doubling grow decision"
        );
        // The element copy (in the deep-copy and grow paths) copies each slot with
        // an `i64.load` (0x29) immediately followed by an `i64.store` (0x37) — the
        // 8-byte word copy of a list element slot.
        assert!(
            find_subslice(&code, &[0x29, 0x03, 0x00, 0x37, 0x03, 0x00]).is_some(),
            "list copy emits an 8-byte word load+store per element slot"
        );
    }

    #[test]
    fn list_new_allocates_len_cap_header() {
        // `list_new()` allocates a `[len=0][cap=LIST_INITIAL_CAP][slots]` header:
        // the body stores 0 at the len offset and LIST_INITIAL_CAP at the cap
        // offset. The `i32.const LIST_INITIAL_CAP` (0x41 0x04) capacity literal must
        // appear, and the function is eligible (returns a `list` pointer).
        let source = "fn empty -> list<i64>\n    list_new()\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["empty".to_string()]);
        let code = section_body(&artifact.bytes, 10).expect("code section");
        // i32.const 4 (LIST_INITIAL_CAP) then i32.store at the cap offset (4).
        assert!(
            find_subslice(&code, &[0x41, LIST_INITIAL_CAP as u8, 0x36, 0x02, 0x04]).is_some(),
            "list_new stores the initial capacity into the cap header slot"
        );
    }

    #[test]
    fn list_of_heap_element_is_skipped() {
        // A `list<string>` (heap element) is DEFERRED: `supported_list_element`
        // rejects a non-scalar element, so the function's signature is ineligible
        // and it is skipped (still runs on the interpreters), never miscompiled.
        let source = concat!(
            "fn names -> list<string>\n",
            "    push(list_new(), \"a\")\n\n",
            "fn ok n i64 -> i64\n    n + 1\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["ok".to_string()]);
        assert_eq!(artifact.skipped.len(), 1);
        assert_eq!(artifact.skipped[0].name, "names");
    }

    // -- Aggregates across call boundaries (params/returns) --------------------

    #[test]
    fn struct_param_and_return_functions_compile() {
        // A function TAKING a struct (reading its fields) and one RETURNING a
        // struct are both eligible — an aggregate is an `i32` pointer, so it is a
        // first-class WASM value at the boundary. Neither is skipped.
        let source = concat!(
            "struct Point\n    x i64\n    y i64\n\n",
            "fn sum_point p Point -> i64\n    p.x + p.y\n\n",
            "fn make_point a i64 b i64 -> Point\n    Point(a, b)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"sum_point".to_string())
                && artifact.compiled.contains(&"make_point".to_string()),
            "struct param/return functions should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());
    }

    #[test]
    fn array_param_and_return_functions_compile() {
        // A function taking AND returning a fixed `array<i64>` compiles: it reads
        // an element and returns the (copied) array pointer.
        let source = concat!(
            "fn first_of xs array<i64> -> i64\n    xs[0]\n\n",
            "fn identity xs array<i64> -> array<i64>\n    xs\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"first_of".to_string())
                && artifact.compiled.contains(&"identity".to_string()),
            "array param/return functions should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());
    }

    #[test]
    fn passing_a_struct_argument_deep_copies_it() {
        // Value semantics: an aggregate argument is deep-copied at the call site so
        // the callee cannot mutate the caller's record through the shared pointer.
        // The caller `use_it` constructs nothing itself; it only forwards its own
        // struct PARAMETER to `sum_point`. So the ONLY `__alloc` call in `use_it`'s
        // body is the copy-on-pass — its presence proves the snapshot is emitted.
        let source = concat!(
            "struct Point\n    x i64\n    y i64\n\n",
            "fn sum_point p Point -> i64\n    p.x + p.y\n\n",
            "fn use_it p Point -> i64\n    sum_point(p)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(artifact.compiled.contains(&"use_it".to_string()));
        // `__alloc` is the LAST internal function, so its WASM index is
        // IMPORT_FUNC_COUNT + (number of user functions). Here: sum_point(0),
        // use_it(1) => __alloc index = IMPORT_FUNC_COUNT + 2.
        let alloc_index = IMPORT_FUNC_COUNT as u8 + 2;
        let code = section_body(&artifact.bytes, 10).expect("code section");
        assert!(
            find_subslice(&code, &[0x10, alloc_index]).is_some(),
            "passing a struct argument emits a `call __alloc` copy-on-pass"
        );
    }

    #[test]
    fn passing_an_array_argument_deep_copies_it() {
        // The array copy-on-pass reads the `[len]` header (`i32.load` at offset 0),
        // allocates a fresh block (`call __alloc`), and copies elements in a loop.
        // `use_it` constructs no array of its own, so the `__alloc` call in its body
        // is the copy, and the header read appears before it.
        let source = concat!(
            "fn first_of xs array<i64> -> i64\n    xs[0]\n\n",
            "fn use_it xs array<i64> -> i64\n    first_of(xs)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(artifact.compiled.contains(&"use_it".to_string()));
        let alloc_index = IMPORT_FUNC_COUNT as u8 + 2; // first_of(0), use_it(1), __alloc(2)
        let code = section_body(&artifact.bytes, 10).expect("code section");
        assert!(
            find_subslice(&code, &[0x10, alloc_index]).is_some(),
            "passing an array argument emits a `call __alloc` copy-on-pass"
        );
    }

    #[test]
    fn passing_a_string_argument_is_not_copied() {
        // A `string` is an immutable pointer, so it is shared (never deep-copied):
        // the callee cannot mutate it, exactly matching the interpreters. A caller
        // that only forwards its string parameter allocates nothing, so its body
        // contains no `call __alloc`.
        let source = concat!(
            "fn take s string -> i64\n    len(s)\n\n",
            "fn use_it s string -> i64\n    take(s)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(artifact.compiled.contains(&"use_it".to_string()));
        let alloc_index = IMPORT_FUNC_COUNT as u8 + 2; // take(0), use_it(1), __alloc(2)
        let code = section_body(&artifact.bytes, 10).expect("code section");
        assert!(
            find_subslice(&code, &[0x10, alloc_index]).is_none(),
            "an immutable string argument must NOT be deep-copied"
        );
    }

    #[test]
    fn bool_returning_comparison_compiles() {
        let source = "fn is_pos n i64 -> bool\n    n > 0\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["is_pos".to_string()]);
    }

    #[test]
    fn no_eligible_functions_errors() {
        // `map<i64, i64>` is not in the supported WASM value set, so nothing is
        // eligible and the backend reports L0338. (A scalar-element `list<i64>` IS
        // supported now — see the growable-list tests below.)
        let source = "fn tally n i64 -> map<i64, i64>\n    map_set(map_new(), n, n)\n";
        let err = emit_wasm_module(&module_for(source)).expect_err("no eligible");
        assert_eq!(err.code, "L0338");
        assert_eq!(err.skipped.len(), 1);
    }

    #[test]
    fn fixed_width_integer_function_compiles() {
        // A function over `u8` (wrapping arithmetic + a fixed-width conversion) is
        // now eligible: fixed-width integers are stored as their normalized `i64`
        // cell and re-normalized after each width-producing op.
        let source = concat!("fn mix a u8 b u8 -> u8\n", "    (a + b) & to_u8(15)\n",);
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["mix".to_string()]);
        assert!(artifact.skipped.is_empty());
        let code = section_body(&artifact.bytes, 10).expect("code section");
        // The `+` re-normalizes to u8 by masking with 0xff (i64.const 0xff;
        // i64.and) and the `& to_u8(15)` masks again — the 0xff mask literal must
        // appear in the body.
        assert!(
            find_subslice(&code, &[0x42, 0xff, 0x01]).is_some(),
            "u8 normalization masks with 0xff (i64.const 0xff, i64.and)"
        );
    }

    #[test]
    fn signed_conversion_uses_sign_extension_opcode() {
        // `to_i8(x)` inlines to a normalize: for a signed 8-bit kind that is the
        // dedicated `i64.extend8_s` (0xc2), not a mask.
        let source = "fn narrow x i64 -> i8\n    to_i8(x)\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["narrow".to_string()]);
        let code = section_body(&artifact.bytes, 10).expect("code section");
        assert!(
            find_subslice(&code, &[0xc2]).is_some(),
            "to_i8 emits i64.extend8_s"
        );
    }

    #[test]
    fn unsigned_comparison_and_shift_pick_unsigned_opcodes() {
        // A `u32` comparison uses the unsigned opcode (`i64.gt_u`, 0x56) and a
        // `u32` right shift uses the logical `i64.shr_u` (0x88) plus a width mask.
        let source = concat!(
            "fn f a u32 b u32 -> u32\n",
            "    if a > b\n",
            "        return a >> to_u32(1)\n",
            "    b >> to_u32(1)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["f".to_string()]);
        let code = section_body(&artifact.bytes, 10).expect("code section");
        assert!(
            find_subslice(&code, &[0x56]).is_some(),
            "unsigned `>` uses i64.gt_u"
        );
        assert!(
            find_subslice(&code, &[0x88]).is_some(),
            "unsigned `>>` uses logical i64.shr_u"
        );
    }

    #[test]
    fn signed_right_shift_is_arithmetic() {
        // A signed `i32` right shift uses the arithmetic `i64.shr_s` (0x87).
        let source = "fn sar s i32 -> i32\n    s >> to_i32(1)\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["sar".to_string()]);
        let code = section_body(&artifact.bytes, 10).expect("code section");
        assert!(
            find_subslice(&code, &[0x87]).is_some(),
            "signed `>>` uses arithmetic i64.shr_s"
        );
    }

    #[test]
    fn f32_arithmetic_and_conversions_compile() {
        // An i64-returning function that computes with `f32` internally (the
        // `to_f32`/`to_f64` conversions plus `f32` arithmetic and a comparison)
        // now compiles: `f32` is a supported WASM scalar (single precision).
        let source = concat!(
            "fn main -> i64\n",
            "    let a f32 = to_f32(1.0)\n",
            "    let b f32 = to_f32(2.0)\n",
            "    let s f32 = a + b\n",
            "    let d f32 = s / to_f32(2.0)\n",
            "    if to_f64(d) < 2.0\n",
            "        return 1\n",
            "    0\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["main".to_string()]);
        assert!(artifact.skipped.is_empty());
        let code = section_body(&artifact.bytes, 10).expect("code section");
        // `f32.add` (0x92) and `f32.div` (0x95) for the single-precision arithmetic.
        assert!(find_subslice(&code, &[0x92]).is_some(), "expected f32.add");
        assert!(find_subslice(&code, &[0x95]).is_some(), "expected f32.div");
        // `f32.demote_f64` (0xb6) for `to_f32` and `f64.promote_f32` (0xbb) for
        // `to_f64` — the inlined conversions, not real calls.
        assert!(
            find_subslice(&code, &[0xb6]).is_some(),
            "expected f32.demote_f64 for to_f32"
        );
        assert!(
            find_subslice(&code, &[0xbb]).is_some(),
            "expected f64.promote_f32 for to_f64"
        );
    }

    #[test]
    fn f32_field_slot_uses_single_precision_memory_ops() {
        // An `f32` struct field is laid out as a single-precision slot: writing it
        // uses `f32.store` (0x38) and reading it uses `f32.load` (0x2a), so a
        // round-tripped f32 keeps its single-precision bits.
        let source = concat!(
            "struct Box\n",
            "    v f32\n\n",
            "fn main -> i64\n",
            "    let box Box = Box(to_f32(1.5))\n",
            "    if to_f64(box.v) < 2.0\n",
            "        return 1\n",
            "    0\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(artifact.compiled.contains(&"main".to_string()));
        let code = section_body(&artifact.bytes, 10).expect("code section");
        assert!(
            find_subslice(&code, &[0x38]).is_some(),
            "expected f32.store for an f32 struct field"
        );
        assert!(
            find_subslice(&code, &[0x2a]).is_some(),
            "expected f32.load for reading an f32 struct field"
        );
    }

    #[test]
    fn f32_comparison_over_float_arithmetic_uses_f32_compare() {
        // A comparison whose operand is a float ARITHMETIC subtree (annotated `i64`
        // in the IR) must still pick the single-precision `f32.gt` (0x5e), driven
        // by the structural float-width detection rather than the node's own `ty`.
        let source = concat!(
            "fn main -> i64\n",
            "    let a f32 = to_f32(1.0)\n",
            "    let b f32 = to_f32(2.0)\n",
            "    if a + b > to_f32(2.0)\n",
            "        return 1\n",
            "    0\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["main".to_string()]);
        let code = section_body(&artifact.bytes, 10).expect("code section");
        assert!(
            find_subslice(&code, &[0x5e]).is_some(),
            "float arithmetic compared with `>` must use f32.gt, not an integer compare"
        );
        // It must NOT fall back to the integer `i64.gt_s` (0x55) over f32 values.
        assert!(
            find_subslice(&code, &[0x55]).is_none(),
            "the f32 comparison must not use i64.gt_s"
        );
    }

    #[test]
    fn f32_parameter_and_return_compile_like_f64() {
        // `f32` is a first-class scalar, so a function taking and returning `f32`
        // is eligible exactly like the existing `f64` support; a `u16` companion
        // still compiles alongside it.
        let source = concat!(
            "fn scale x f32 -> f32\n",
            "    x * to_f32(2.0)\n\n",
            "fn ok a u16 -> u16\n",
            "    a + to_u16(1)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(
            artifact.compiled,
            vec!["scale".to_string(), "ok".to_string()]
        );
        assert!(artifact.skipped.is_empty());
        let code = section_body(&artifact.bytes, 10).expect("code section");
        assert!(
            find_subslice(&code, &[0x94]).is_some(),
            "expected f32.mul (0x94) in `scale`"
        );
    }

    #[test]
    fn float_math_builtin_skips_gracefully() {
        // A float math builtin (`sqrt`) is out of scope for the WASM backend (as on
        // native): the function is demoted to skipped and still runs on the
        // interpreters, while an f32-arithmetic companion compiles.
        let source = concat!(
            "fn root x f64 -> f64\n",
            "    sqrt(x)\n\n",
            "fn plain a f32 b f32 -> f32\n",
            "    a + b\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["plain".to_string()]);
        assert!(
            artifact.skipped.iter().any(|s| s.name == "root"),
            "the `sqrt` math builtin must skip gracefully"
        );
    }

    #[test]
    fn saturating_builtin_function_skips_gracefully() {
        // `saturating_add` is out of scope (matches native, which skips it): the
        // function is demoted to skipped, and the eligible companion still compiles.
        let source = concat!(
            "fn s a u8 b u8 -> u8\n",
            "    saturating_add(a, b)\n\n",
            "fn plain a u8 b u8 -> u8\n",
            "    a + b\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["plain".to_string()]);
        assert!(
            artifact
                .skipped
                .iter()
                .any(|s| s.name == "s" && s.reason.contains("saturating_add"))
        );
    }

    #[test]
    fn uleb_and_sleb_roundtrip() {
        let mut out = Vec::new();
        write_uleb(&mut out, 0);
        assert_eq!(out, vec![0x00]);
        out.clear();
        write_uleb(&mut out, 624485);
        assert_eq!(out, vec![0xe5, 0x8e, 0x26]);
        out.clear();
        write_sleb(&mut out, -123456);
        assert_eq!(out, vec![0xc0, 0xbb, 0x78]);
        out.clear();
        write_sleb(&mut out, 0);
        assert_eq!(out, vec![0x00]);
    }

    /// Parse the section ids present in a module (skipping the 8-byte header).
    fn section_ids(bytes: &[u8]) -> Vec<u8> {
        let mut ids = Vec::new();
        let mut i = 8;
        while i < bytes.len() {
            let id = bytes[i];
            i += 1;
            let (len, consumed) = read_uleb(&bytes[i..]);
            i += consumed;
            i += len as usize;
            ids.push(id);
        }
        ids
    }

    fn read_uleb(bytes: &[u8]) -> (u64, usize) {
        let mut result = 0u64;
        let mut shift = 0;
        let mut i = 0;
        loop {
            let byte = bytes[i];
            result |= ((byte & 0x7f) as u64) << shift;
            i += 1;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        (result, i)
    }

    /// Return the contents (payload) of the first section with the given id.
    fn section_body(bytes: &[u8], want: u8) -> Option<Vec<u8>> {
        let mut i = 8;
        while i < bytes.len() {
            let id = bytes[i];
            i += 1;
            let (len, consumed) = read_uleb(&bytes[i..]);
            i += consumed;
            let end = i + len as usize;
            if id == want {
                return Some(bytes[i..end].to_vec());
            }
            i = end;
        }
        None
    }

    /// Find the first occurrence of `needle` in `haystack`.
    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || needle.len() > haystack.len() {
            return None;
        }
        (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
    }

    // -- Enum + match ---------------------------------------------------------

    #[test]
    fn option_match_compiles_with_tag_load_and_branch() {
        // A function matching `option<i64>` compiles to WASM. Its body loads the
        // enum's discriminant tag (`i32.load` at offset 0) and dispatches with an
        // `i32.eq` + `if` on the tag.
        let source = concat!(
            "fn unwrap_or o option<i64> fallback i64 -> i64\n",
            "    match o\n",
            "        some(v) -> v\n",
            "        none -> fallback\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"unwrap_or".to_string()),
            "option match should compile, skipped: {:?}",
            artifact.skipped
        );
        let code = section_body(&artifact.bytes, 10).expect("code section");
        // Tag load: `i32.load` (0x28) align 2 (0x02) offset 0 (0x00).
        assert!(
            find_subslice(&code, &[0x28, 0x02, 0x00]).is_some(),
            "match loads the enum discriminant tag"
        );
        // Dispatch: `i32.eq` (0x46) then `if` (0x04) with the value result type
        // `i64` (0x7e) — the arms yield `i64`.
        assert!(
            find_subslice(&code, &[0x46, 0x04, 0x7e]).is_some(),
            "match dispatches on the tag with a typed `if`"
        );
    }

    #[test]
    fn result_scalar_match_and_construction_compile() {
        // A `result<i64, i64>` (scalar ok/err payloads) is a supported WASM enum:
        // both constructing it and matching it compile.
        let source = concat!(
            "fn divide n i64 d i64 -> result<i64, i64>\n",
            "    if d == 0\n",
            "        return err(0 - 1)\n",
            "    ok(n / d)\n\n",
            "fn describe r result<i64, i64> -> i64\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(e) -> e\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"divide".to_string())
                && artifact.compiled.contains(&"describe".to_string()),
            "result<i64,i64> construction and match should compile, skipped: {:?}",
            artifact.skipped
        );
    }

    #[test]
    fn user_enum_match_with_wildcard_compiles() {
        // A small user enum with a scalar payload and a wildcard arm compiles.
        let source = concat!(
            "enum Shape\n",
            "    Circle i64\n",
            "    Rect i64 i64\n",
            "    Empty\n\n",
            "fn area s Shape -> i64\n",
            "    match s\n",
            "        Circle(r) -> r * r\n",
            "        Rect(w, h) -> w * h\n",
            "        _ -> 0\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"area".to_string()),
            "user enum match with wildcard should compile, skipped: {:?}",
            artifact.skipped
        );
        let code = section_body(&artifact.bytes, 10).expect("code section");
        // Tag load and typed-`if` dispatch present.
        assert!(
            find_subslice(&code, &[0x28, 0x02, 0x00]).is_some(),
            "user-enum match loads the discriminant tag"
        );
    }

    #[test]
    fn enum_construction_stores_the_discriminant_tag() {
        // Constructing `some(x)` allocates a record and stores the variant's tag
        // (an `i32.const` + `i32.store` at offset 0). `some` is discriminant 0.
        let source = concat!("fn wrap x i64 -> option<i64>\n", "    some(x)\n",);
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"wrap".to_string()),
            "option construction should compile, skipped: {:?}",
            artifact.skipped
        );
        let code = section_body(&artifact.bytes, 10).expect("code section");
        // `i32.const 0` (tag) then `i32.store` (0x36) align 2 offset 0.
        assert!(
            find_subslice(&code, &[0x41, 0x00, 0x36, 0x02, 0x00]).is_some(),
            "construction stores the `some` discriminant tag (0) at offset 0"
        );
    }

    #[test]
    fn enum_with_heap_payload_is_skipped() {
        // `result<i64, string>` has a heap (`string`) payload, which the WASM
        // backend defers: the function is skipped (runs on the interpreters), never
        // miscompiled. With no other eligible function, emission reports L0338.
        let source = concat!(
            "fn describe r result<i64, string> -> i64\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> len(m)\n",
        );
        let error = emit_wasm_module(&module_for(source)).expect_err("no eligible functions");
        assert_eq!(error.code, "L0338");
        assert!(
            error.skipped.iter().any(|s| s.name == "describe"),
            "the heap-payload enum function is recorded as skipped: {:?}",
            error.skipped
        );
    }

    #[test]
    fn enum_layout_orders_builtin_and_user_variants() {
        // The discriminant ordering matches the interpreters: `option` is
        // `[some, none]`, `result` is `[ok, err]`, and a user enum follows its
        // declaration order. The payload slot count is the max payload arity.
        let enums = enum_table(&[IrEnumDef {
            name: "Shape".to_string(),
            variants: vec![
                IrEnumVariant {
                    name: "Circle".to_string(),
                    payload: vec![TypeRef::new("i64")],
                },
                IrEnumVariant {
                    name: "Rect".to_string(),
                    payload: vec![TypeRef::new("i64"), TypeRef::new("i64")],
                },
                IrEnumVariant {
                    name: "Empty".to_string(),
                    payload: Vec::new(),
                },
            ],
        }]);

        let option = enum_layout(&TypeRef::new("option<i64>"), &enums).expect("option layout");
        assert_eq!(option.tag_of("some"), Some(0));
        assert_eq!(option.tag_of("none"), Some(1));
        assert_eq!(option.slot_count, 1);

        let result = enum_layout(&TypeRef::new("result<i64, i64>"), &enums).expect("result layout");
        assert_eq!(result.tag_of("ok"), Some(0));
        assert_eq!(result.tag_of("err"), Some(1));

        let shape = enum_layout(&TypeRef::new("Shape"), &enums).expect("user layout");
        assert_eq!(shape.tag_of("Circle"), Some(0));
        assert_eq!(shape.tag_of("Rect"), Some(1));
        assert_eq!(shape.tag_of("Empty"), Some(2));
        assert_eq!(shape.slot_count, 2, "widest variant `Rect` has two slots");

        // A heap payload makes the enum unsupported for WASM.
        assert!(
            enum_layout(&TypeRef::new("result<i64, string>"), &enums).is_none(),
            "heap-payload result is not a supported WASM enum"
        );
    }
}
