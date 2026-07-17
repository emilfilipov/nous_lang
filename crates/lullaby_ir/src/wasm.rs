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
//! - A `string` is a pointer to `[char_len: i32][byte_len: i32][utf8 bytes]`: the
//!   Unicode scalar (char) count, then the UTF-8 byte length, then the encoded
//!   bytes. String literals are laid out once in the Data section (their pointer
//!   is a constant static offset); `len(s)` loads the leading `i32` (the char
//!   count). Runtime concatenation `a + b` on two strings allocates a fresh record
//!   via `__alloc`, writes the summed char/byte headers, and `memory.copy`s each
//!   operand's UTF-8 byte range into place — so it works for multi-byte text, not
//!   just ASCII, and the result is a normal string usable by `len`, `console_log`,
//!   and further `+`. Other string builtins (`to_string`, `substring`, `find`,
//!   `replace`, `upper`/`lower`, `split`/`join`) remain deferred to the
//!   interpreters.
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
//! - A growable `map<K, V>` with a SCALAR or `string` `K` and a scalar or `string`
//!   `V` is a pointer to `[len: i32][cap: i32][(key, value) slot pairs]`: an
//!   insertion-ordered association list mirroring the interpreters' `Value::Map`.
//!   `map_new` allocates an empty header; `map_set` deep-copies then updates an
//!   existing key's value in place or appends a new `(k, v)` entry (growing with
//!   capacity doubling like a list); `map_get` linear-scans and yields
//!   `some(v)`/`none` (reusing the option/enum layout); `map_has` scans to a `bool`;
//!   `map_len` reads the header. A `string` key/value occupies its slot as an `i32`
//!   pointer to the immutable `[char_len][byte_len][utf8]` record (shared on deep
//!   copy since strings are immutable). A scalar key is compared with an integer
//!   `i32.eq`/`i64.eq`; a `string` key is compared by CONTENT (equal `byte_len` and
//!   identical UTF-8 bytes), so two DISTINCT string objects with the same bytes are
//!   the SAME key — matching the interpreters' `Value` content equality, not
//!   pointer identity. Ordering and lookup match the interpreters bit-for-bit. A map
//!   with a non-string heap key or value (`map<list<…>, V>`, `map<K, list<…>>`, …)
//!   is DEFERRED.
//!
//! A function that uses a different builtin, a `list`/`map` with an unsupported
//! heap element (e.g. `list<struct>`, `map<K, list<…>>`, `map<list<…>, V>`), an
//! enum with a HEAP payload (`list`/`array`/`map` — notably a `result` carrying a
//! collection), or any type still outside this set is SKIPPED with a reason (it
//! still runs on the interpreters).
//!
//! Linear-memory infrastructure: the module exports a `"memory"` (min 1 page),
//! imports the host functions `env.log_i64 (func (param i64))` (surfaced as
//! `wasm_log`), `env.console_log (func (param i32 i32))` (surfaced as
//! `console_log`), and `env.dom_set_text (func (param i32 i32 i32 i32))`
//! (surfaced as `dom_set_text`) — the JS/DOM browser-host interop layer —
//! declares a mutable `i32` global bump pointer, writes a Data section seeding the
//! reserved region and the string-literal pool, and emits an internal
//! `__alloc(size i32) -> i32` bump-allocator helper used to build strings/structs/
//! arrays at runtime. Imported functions occupy the LOW function indices, so every
//! internally-defined function's index is shifted by the import count; call
//! targets and exports are fixed up accordingly. The string host imports take a
//! `(ptr, len)` pair per string, where `ptr` points at the string's first UTF-8
//! byte (`record + STR_DATA_OFF`) and `len` is its UTF-8 byte length, so the host
//! slices `[ptr, ptr + len)` of `memory` directly. Enums with heap payloads and
//! heap-element `list`/`map` remain deferred.

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

/// The builtin that formats a scalar value as its decimal/textual `string`. The
/// WASM backend compiles it for integer/bool/char/byte/string arguments and
/// defers float arguments (`f32`/`f64`) to the interpreters (dtoa is out of the
/// scalar subset).
const TO_STRING_BUILTIN: &str = "to_string";

/// Index-based string-operation builtins the WASM backend compiles. `substring`
/// and `find` are CHAR-indexed (they decode UTF-8 to map char indices to byte
/// offsets, matching the interpreters' `chars()`/char-count semantics);
/// `contains`/`starts_with`/`ends_with` are byte-exact substring/prefix/suffix
/// tests (byte equality is char-position-independent, so no decode is needed).
const SUBSTRING_BUILTIN: &str = "substring";
const FIND_BUILTIN: &str = "find";
const CONTAINS_BUILTIN: &str = "contains";
const STARTS_WITH_BUILTIN: &str = "starts_with";
const ENDS_WITH_BUILTIN: &str = "ends_with";

// -- String record layout ----------------------------------------------------
//
// A `string` is an `i32` pointer to `[char_len: i32][byte_len: i32][utf8 bytes]`:
// two `i32` headers followed by the UTF-8 encoding of the text. The FIRST header
// is the Unicode scalar (char) count — the value `len(s)` returns, matching the
// interpreters — and shares offset 0 with the array/list length header, so
// `len(s)` reuses the array length path. The SECOND header is the UTF-8 BYTE
// count, needed to slice the raw bytes without assuming one byte per char (so
// non-ASCII text concatenates and decodes correctly). String literals are laid
// out once in the Data section (their pointer is a constant static offset);
// runtime concatenation `a + b` allocates a fresh record.

/// Byte offset of a string's char-count header (Unicode scalar count). Shares
/// offset 0 with the array/list length header so `len(s)` reuses that path.
const STR_CHAR_LEN_OFF: i32 = 0;

/// Byte offset of a string's byte-count header (UTF-8 byte length of the text).
const STR_BYTE_LEN_OFF: i32 = 4;

/// Byte offset of a string's first UTF-8 data byte: past the two `i32` headers.
const STR_DATA_OFF: i32 = 8;

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

// -- Growable map layout -----------------------------------------------------
//
// A growable `map<K, V>` (scalar `K`/`V`) is an `i32` pointer to a header
// `[len: i32][cap: i32][(key, value) slot pairs...]`: the current entry count,
// the allocated capacity (in entries), then `cap` entry records. Each entry is
// two uniform 8-byte slots — the key slot then the value slot — so the offset of
// entry `i` is `MAP_DATA_OFF + i * MAP_ENTRY_SIZE`, its key at `+0` and its value
// at `+SLOT_SIZE`. Uniform 8-byte slots keep every scalar key/value naturally
// aligned for `i64`/`f64` loads and stores. `len` shares the leading `i32` offset
// with strings/arrays/lists.
//
// This mirrors the interpreters' `Value::Map` — an INSERTION-ORDERED association
// list scanned linearly with `Value` equality: `map_set` overwrites the value of
// an existing key in place (preserving its slot/order) or appends a new entry at
// the end, growing with capacity doubling like a list; `map_get`/`map_has` scan
// entries front-to-back; `map_len` reads the header. Ordering and lookup match
// the interpreters bit-for-bit.
//
// Value semantics: like lists, maps are value-semantic. Every mutating op
// (`map_set`) deep-copies the source map first and mutates the fresh copy, and a
// map crossing a call boundary is deep-copied like any other mutable aggregate,
// so mutating one binding is never observable through another. The bump allocator
// never reclaims, so a grown/copied map orphans its old block.

/// Byte offset of a map's `len` header (entry count). Shares offset 0 with the
/// string/array/list length header.
const MAP_LEN_OFF: i32 = 0;

/// Byte offset of a map's `cap` header (allocated capacity, in entries).
const MAP_CAP_OFF: i32 = 4;

/// Byte offset of a map's first entry record: past the two `i32` headers.
const MAP_DATA_OFF: i32 = 8;

/// Bytes per map entry: a key slot followed by a value slot, each `SLOT_SIZE`.
const MAP_ENTRY_SIZE: i32 = SLOT_SIZE * 2;

/// Byte offset of the value slot within a map entry (the key is at offset 0).
const MAP_VALUE_OFF: i32 = SLOT_SIZE;

/// Initial capacity a `map_new()` header (and the first growth of an empty map)
/// allocates, so a handful of inserts do not each trigger a realloc.
const MAP_INITIAL_CAP: i32 = 4;

/// Builtins that construct or read growable maps, matched by name in call
/// lowering (arity/key/value types are validated there against the IR types).
const MAP_NEW_BUILTIN: &str = "map_new";
const MAP_SET_BUILTIN: &str = "map_set";
const MAP_GET_BUILTIN: &str = "map_get";
const MAP_HAS_BUILTIN: &str = "map_has";
const MAP_LEN_BUILTIN: &str = "map_len";

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
pub(crate) struct EnumLayout {
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

/// Resolve the [`EnumLayout`] of an enum-typed `TypeRef`, or `None` if `ty` is not
/// an enum the WASM backend can lay out. The supported enums are the built-in
/// `option<T>` (variants `some(T)`, `none`) and `result<T, E>` (variants `ok(T)`,
/// `err(E)`), and any user enum, where every variant payload is layable-out by
/// [`enum_payload_slot_type`]: a scalar, a `string`, or a MUTABLE aggregate (a
/// `struct` or a supported nested growable `list`).
///
/// - A **scalar** payload occupies one 8-byte slot as its own value.
/// - A **`string`** payload occupies one `i32`-pointer slot to the immutable record;
///   because strings are immutable it is copied by SHARING its pointer on a deep
///   copy (see [`emit_deep_copy_enum`]), exactly like a scalar.
/// - A **mutable-aggregate** payload (`struct` / nested `list`) occupies one
///   `i32`-pointer slot and is DEEP-COPIED per payload on the enum's value-semantic
///   copy, so `option<struct>` — the result of `map_get` on a `map<K, struct>` —
///   and `result<struct, E>` are supported with correct value semantics. One level
///   of mutable nesting is allowed (see [`collection_slot_type`]); deeper cases
///   yield `None` so the enclosing function is skipped (still runs on the
///   interpreters).
fn enum_layout(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
) -> Option<EnumLayout> {
    // Built-in `option<T>`: variants `some(T)`, `none`, in that order. `?` bails
    // (unsupported enum) when `T` is not layable-out. `option<string>` — the result
    // of `map_get` on a `map<K, string>` — and `option<struct>` — the result of
    // `map_get` on a `map<K, struct>` — are both supported.
    if let Some(inner) = ty.option_element() {
        enum_payload_slot_type(&inner, structs, enums)?;
        return Some(build_layout(vec![
            ("some".to_string(), vec![inner]),
            ("none".to_string(), Vec::new()),
        ]));
    }
    // Built-in `result<T, E>`: variants `ok(T)`, `err(E)`, in that order. Each
    // payload type must be layable-out; `?` bails otherwise.
    if let Some((ok, err)) = ty.result_args() {
        enum_payload_slot_type(&ok, structs, enums)?;
        enum_payload_slot_type(&err, structs, enums)?;
        return Some(build_layout(vec![
            ("ok".to_string(), vec![ok]),
            ("err".to_string(), vec![err]),
        ]));
    }
    // A user enum: every variant payload must be layable-out (`?` bails otherwise).
    let def = enums.get(&ty.name)?;
    let mut variants = Vec::with_capacity(def.variants.len());
    for variant in &def.variants {
        for payload_ty in &variant.payload {
            enum_payload_slot_type(payload_ty, structs, enums)?;
        }
        variants.push((variant.name.clone(), variant.payload.clone()));
    }
    Some(build_layout(variants))
}

/// The WASM slot type of an enum variant payload: a scalar, a `string`, or a
/// mutable aggregate (a `struct` or a supported nested growable `list`) at nesting
/// depth 0. A mutable-aggregate payload is an `i32` pointer deep-copied per payload
/// on the enum's value-semantic copy (see [`emit_deep_copy_enum`]). Returns `None`
/// for a payload the backend cannot lay out (e.g. a `map` payload or nesting beyond
/// one mutable level), so the enclosing enum — and its function — is skipped.
fn enum_payload_slot_type(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
) -> Option<WasmValType> {
    if let Some(vt) = scalar_or_string_slot_type(ty) {
        return Some(vt);
    }
    collection_slot_type(ty, structs, enums, 0)
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
pub(crate) enum WasmValType {
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
    if enum_layout(ty, structs, enums).is_some() {
        return true;
    }
    // A declared enum NAME is always an i32 pointer, even when `enum_layout` cannot
    // resolve a concrete layout for it — that happens for the BASE spelling of a
    // generic enum (`Opt`, not `Opt<i64>`), which is the type a generic enum
    // CONSTRUCTION node carries (`present(n)` is typed `Opt`). The construction still
    // lowers to a pointer (see `generic_enum_construction_layout`); this lets the
    // value-type classifier treat it as one so a generic-enum-returning tail/return
    // resolves. (A base generic STRUCT name is already caught by `structs` above.)
    if enums.contains_key(&ty.name) {
        return true;
    }
    if let Some(elem) = ty.array_element() {
        return slot_val_type(&elem, structs, enums).is_some();
    }
    if supported_list_element(ty, structs, enums).is_some() {
        return true;
    }
    if supported_map_kv(ty, structs, enums).is_some() {
        return true;
    }
    false
}

/// The WASM slot type for a supported growable-collection element/value: a scalar
/// (its own value type) or a `string` (an `i32` pointer to the immutable
/// `[char_len][byte_len][utf8]` record). `None` for any other type. A `string`
/// element/value occupies a single 8-byte slot exactly like a scalar and, because
/// strings are immutable, is copied by SHARING its pointer on a value-semantic
/// deep copy (never deep-recursed) — so the flat word copy in
/// [`emit_list_copy_elems`]/[`emit_map_copy_entries`] is already an exact deep
/// copy. Other heap elements (`struct`/`array`/`list`/`map`) are DEFERRED because
/// they would need a recursive per-element deep copy.
fn scalar_or_string_slot_type(ty: &TypeRef) -> Option<WasmValType> {
    if let Some(vt) = scalar_val_type(ty) {
        return Some(vt);
    }
    if ty.name == "string" {
        return Some(WasmValType::I32);
    }
    None
}

/// The maximum depth of MUTABLE-aggregate nesting a growable collection
/// element/value may reach before the WASM backend defers it. Depth 0 is the
/// collection's own element/value slot; a struct field or a nested list element
/// consumes one level. This bounds the recursive per-element deep copy to a level
/// the emitter can verify: one level of mutable nesting (`list<struct>`,
/// `list<list<scalar>>`, `map<K, struct>`) is supported, and deeper cases
/// (`list<list<list<…>>>`, `list<map<…>>`, a struct field that is itself a list)
/// are skipped gracefully (still run on the interpreters) rather than miscompiled.
const MAX_COLLECTION_NEST_DEPTH: u32 = 1;

/// The WASM slot type of a growable-collection element/value at nesting `depth`, or
/// `None` if the WASM backend cannot lay it out (so the enclosing function is
/// skipped). Accepts, in order:
///
/// - a **scalar** — its own value type, flat-copied on a deep copy;
/// - a **`string`** — an `i32` pointer to the immutable record, SHARED on a deep
///   copy (never deep-recursed) since strings are immutable;
/// - a **mutable aggregate** (a named `struct` or a supported nested growable
///   `list<T>`) at `depth < MAX_COLLECTION_NEST_DEPTH` — an `i32` pointer that is
///   itself DEEP-COPIED per element on the collection's value-semantic copy (see
///   [`emit_copy_slot`]/[`emit_copy_element_at`] and [`emit_list_copy_elems`]/
///   [`emit_map_copy_entries`]), matching the interpreters' recursive
///   `Value::clone`. The nested aggregate's own fields/elements are classified one
///   level deeper, so `list<list<scalar>>` is accepted but `list<list<list<…>>>`
///   is deferred.
///
/// An `enum` element/value is NOT accepted here (an enum with a mutable payload is
/// unsupported anyway, and a scalar/string-payload enum crossing a collection is a
/// later increment), nor is a nested `map` element/value (`list<map<…>>`,
/// `map<K, map<…>>`) or a fixed `array` element — those are deferred.
fn collection_slot_type(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
    depth: u32,
) -> Option<WasmValType> {
    if let Some(vt) = scalar_or_string_slot_type(ty) {
        return Some(vt);
    }
    if depth >= MAX_COLLECTION_NEST_DEPTH {
        return None;
    }
    // A struct element/value: every field must itself be layable-out (a scalar, a
    // `string`, or a mutable aggregate one level deeper). This mirrors
    // `emit_deep_copy_struct`, which deep-copies each field.
    if let Some(fields) = structs.get(&ty.name) {
        for (_, field_ty) in fields {
            struct_field_slot_type(field_ty, structs, enums, depth + 1)?;
        }
        return Some(WasmValType::I32);
    }
    // A nested growable `list<T>` element/value: its own element must be layable-out
    // one level deeper (so `list<list<scalar>>` works, `list<list<list<…>>>` does
    // not). The nested list is deep-copied per element on the outer copy.
    if let Some(elem) = ty.list_element() {
        collection_slot_type(&elem, structs, enums, depth + 1)?;
        return Some(WasmValType::I32);
    }
    None
}

/// The WASM slot type of a struct FIELD nested inside a growable-collection element,
/// at nesting `depth`. A field may be a scalar, a `string`, an enum/array/struct
/// pointer the base backend already lays out, or a nested growable collection one
/// level deeper. Distinct from [`collection_slot_type`] because a struct field set
/// is broader than a list element set (a field may be an enum or a fixed array,
/// which the existing struct deep copy already handles) — but a nested
/// growable-collection field still consumes a nesting level.
fn struct_field_slot_type(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
    depth: u32,
) -> Option<WasmValType> {
    if let Some(vt) = scalar_val_type(ty) {
        return Some(vt);
    }
    // A `string`, a supported enum, or a fixed `array` field is an `i32` pointer the
    // base struct/array deep copy already handles (a string is shared; an enum/array
    // is deep-copied by the existing paths) — no extra nesting budget consumed.
    if ty.name == "string" {
        return Some(WasmValType::I32);
    }
    if enum_layout(ty, structs, enums).is_some() {
        return Some(WasmValType::I32);
    }
    if let Some(elem) = ty.array_element() {
        slot_val_type(&elem, structs, enums)?;
        return Some(WasmValType::I32);
    }
    // A nested struct or growable collection consumes a nesting level.
    collection_slot_type(ty, structs, enums, depth)
}

/// The `(K, V)` key/value types of a supported growable `map<K, V>`, or `None` if
/// `ty` is not a map, its key is neither a scalar nor a `string`, or its value is
/// unsupported. A `string` KEY is an `i32` pointer stored in the key slot; unlike a
/// scalar key it is compared by CONTENT (the decoded UTF-8 bytes) — see
/// [`emit_string_eq`] and [`emit_map_find`] — matching the interpreters' `Value`
/// content equality, so two distinct string objects with the same bytes are the
/// SAME key. The value may be a scalar, a `string` (shared on deep copy since
/// strings are immutable), or a MUTABLE aggregate — a named `struct` (or a nested
/// `list<scalar|string>`), deep-copied per value on the map's value-semantic copy
/// so `map<K, struct>` is now supported. A map KEY is restricted to a scalar or
/// `string` (a mutable-aggregate key is DEFERRED — the semantic layer already
/// restricts map keys to `i64`/`string`), and a map value nested more than one
/// mutable-aggregate level deep (`map<K, map<…>>`, `map<K, list<list<…>>>`) is
/// DEFERRED — such a map is unsupported and its enclosing function is skipped (still
/// runs on the interpreters).
fn supported_map_kv(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
) -> Option<(TypeRef, TypeRef)> {
    let (key, value) = ty.map_args()?;
    // Keys stay scalar-or-string: a mutable-aggregate key needs content equality the
    // backend does not implement, and the semantic layer forbids it anyway.
    scalar_or_string_slot_type(&key)?;
    collection_slot_type(&value, structs, enums, 0)?;
    Some((key, value))
}

/// The element type of a supported growable `list<T>`, or `None` if `ty` is not a
/// list or its element is unsupported. The element may be a scalar, a `string` (an
/// `i32` pointer shared on deep copy since strings are immutable), or a MUTABLE
/// aggregate — a named `struct` or a nested `list<scalar|string>` — which is
/// deep-copied per element on the list's value-semantic copy (see
/// [`emit_list_copy_elems`]), matching the interpreters' recursive `Value::clone`.
/// One level of mutable nesting (`list<struct>`, `list<list<scalar>>`) is supported;
/// deeper cases (`list<list<list<…>>>`, `list<map<…>>`) are DEFERRED and the
/// enclosing function is skipped (still runs on the interpreters).
fn supported_list_element(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
) -> Option<TypeRef> {
    let elem = ty.list_element()?;
    collection_slot_type(&elem, structs, enums, 0)?;
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
    if enum_layout(ty, structs, enums).is_some() {
        return true;
    }
    if let Some(elem) = ty.array_element() {
        return slot_val_type(&elem, structs, enums).is_some();
    }
    // A supported growable `list<T>` (scalar, `string`, or a mutable-aggregate
    // element) is a mutable aggregate: it is deep-copied when it crosses a call
    // boundary so a callee mutating its parameter cannot alter the caller's list,
    // exactly like the interpreters' `Value::clone`.
    if supported_list_element(ty, structs, enums).is_some() {
        return true;
    }
    // A supported growable `map<K, V>` (scalar/string key; scalar, `string`, or a
    // mutable-aggregate value) is a mutable aggregate: it is deep-copied when it
    // crosses a call boundary so a callee mutating its parameter cannot alter the
    // caller's map, exactly like the interpreters' `Value::clone`.
    if supported_map_kv(ty, structs, enums).is_some() {
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
pub(crate) struct Local {
    index: u32,
    ty: WasmValType,
}

// -- Public entry point ------------------------------------------------------

/// Emit a binary `.wasm` module for the scalar-subset functions of `module`.
///
/// Every top-level function is examined: an eligible one is lowered and exported
/// by its Lullaby name; an ineligible one is recorded in `skipped` with a reason.
/// If no function is eligible, this returns `Err(WasmError)` with code `L0338`.
///
/// Runs the inherent-method pre-pass (`expand_method_instances`) exactly ONCE, up
/// front: it rewrites each resolvable `recv.method(args)` UFCS call into a direct call
/// to a synthesized, monomorphized method-instance function appended to the module,
/// mirroring the native backend. The expansion is deliberately kept out of the
/// recursive skip fixpoint below (which reduces the function set on a lowering
/// failure) so an already-expanded module is never expanded again (that would
/// duplicate instance functions). A module with no receiver-dispatched methods is
/// left structurally unchanged, so non-method WASM output stays byte-identical.
pub fn emit_wasm_module(module: &IrModule) -> Result<WasmArtifact, WasmError> {
    let expanded = expand_method_instances(module);
    emit_wasm_module_expanded(&expanded)
}

/// Emit the module after inherent-method expansion. This is the recursive core of
/// [`emit_wasm_module`]: on a per-function lowering failure it retries over the reduced
/// function set (re-invoking itself, NOT the public entry, so methods are not
/// re-expanded).
fn emit_wasm_module_expanded(module: &IrModule) -> Result<WasmArtifact, WasmError> {
    // A struct name -> ordered `(field, type)` map, used everywhere we classify a
    // type (pointer vs scalar) or compute a struct's field layout.
    let mut structs = struct_table(&module.structs);
    // A user-enum name -> its IR definition, used to classify an enum type and to
    // resolve its variant table / payload layout (see `enum_layout`). Built-in
    // `option`/`result` are resolved structurally from the type spelling, so they
    // are not in this map.
    let mut enums = enum_table(&module.enums);

    // Monomorphize every reachable user-generic struct/enum instantiation, registering
    // each supported (scalar-only or one-level `string`) concrete layout into the two
    // tables under its full spelling (`Box<i64>`, `Opt<string>`). Every downstream
    // classification/layout path keys off the concrete type spelling, so this makes a
    // generic instantiation a first-class concrete type with no other changes —
    // exactly the native backend's per-backend monomorphization. Instantiations whose
    // monomorphized layout carries deeper-than-one-level heap are left unregistered so
    // the enclosing function skips gracefully (`L0338`), matching native's A1 boundary.
    // No-op for a module without generics (non-generic output stays byte-identical).
    expand_generic_instantiations(module, &mut structs, &mut enums);
    let structs = structs;
    let enums = enums;

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
                return match emit_wasm_module_expanded(&reduced) {
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
/// once as `[char_len: i32][byte_len: i32][utf8 bytes]` starting at
/// `RESERVED_BASE`; the value of the literal is the byte offset of its char-count
/// header. The two headers mirror the runtime record layout (see the string
/// record layout notes near `STR_DATA_OFF`) so a literal and a runtime-built
/// string are byte-for-byte interchangeable.
pub(crate) struct StringPool {
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
        let byte_count = text.len() as i32;
        // `[char_len: i32][byte_len: i32][utf8 bytes]` — matches the runtime record
        // built by string concatenation, so literals and runtime strings interop.
        self.bytes.extend_from_slice(&char_count.to_le_bytes());
        self.bytes.extend_from_slice(&byte_count.to_le_bytes());
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
pub(crate) struct LoweredFunction {
    name: String,
    params: Vec<WasmValType>,
    result: Option<WasmValType>,
    /// Locals beyond the parameters, in index order.
    extra_locals: Vec<WasmValType>,
    body: Vec<u8>,
}

/// Per-function lowering state.
pub(crate) struct LowerCtx<'a> {
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
        enum_layout(ty, self.structs, self.enums)
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
    // invariant: only functions that passed `eligibility` reach here, and it
    // rejects any parameter whose type has no `value_val_type` (see the loop over
    // `function.params` in `eligibility`), so every param maps to a value type.
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
pub(crate) struct LoopCtx {
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
            // `s += piece` is string concatenation, not a scalar op; the scalar
            // backend would `i32.add` two heap pointers, so defer the function.
            if ir_name.as_deref() == Some("string") {
                return Err(
                    "compound assignment on a string is not supported on the wasm backend"
                        .to_string(),
                );
            }
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
        lullaby_parser::AssignOp::Remainder => BinaryOp::Remainder,
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
    // A value-producing tail `if`/`else` (every reachable branch, including the
    // final `else`, ends in a value expression of the same type) must emit each
    // WASM `if` with that value's block type so the branch value is left on the
    // stack; a statement `if` (or one with no `else`) stays a void block.
    let block_type = match if_result_type(ctx, branches, else_body)? {
        Some(vt) => vt.byte(),
        None => 0x40, // void block type
    };
    lower_if_from(ctx, branches, 0, else_body, block_type, out, loops)
}

/// The WASM value type an `if`/`else` yields, or `None` if it is a statement
/// (void). Mirrors [`match_result_type`]: an `if` is value-producing only when it
/// has an `else` and a branch/`else` body ends in a non-void tail expression; the
/// type checker guarantees the remaining branches agree.
fn if_result_type(
    ctx: &LowerCtx,
    branches: &[crate::IrIfBranch],
    else_body: &[IrStmt],
) -> Result<Option<WasmValType>, String> {
    // Without an `else`, one path yields nothing, so the `if` cannot be a value.
    if else_body.is_empty() {
        return Ok(None);
    }
    for branch in branches {
        if let Some(IrStmt::Expr(tail)) = branch.body.last()
            && !tail.ty.is_void()
        {
            return expr_val_type(ctx, tail);
        }
    }
    if let Some(IrStmt::Expr(tail)) = else_body.last()
        && !tail.ty.is_void()
    {
        return expr_val_type(ctx, tail);
    }
    Ok(None)
}

fn lower_if_from(
    ctx: &mut LowerCtx,
    branches: &[crate::IrIfBranch],
    idx: usize,
    else_body: &[IrStmt],
    block_type: u8,
    out: &mut Vec<u8>,
    loops: &LoopCtx,
) -> Result<(), String> {
    let branch = &branches[idx];
    lower_expr(ctx, &branch.condition, out)?; // condition (i32)
    out.push(0x04); // if
    out.push(block_type); // void (0x40) or the yielded value's block type
    let inner = loops.nest();
    lower_stmts(ctx, &branch.body, out, &inner)?;
    out.push(0x05); // else
    if idx + 1 < branches.len() {
        lower_if_from(ctx, branches, idx + 1, else_body, block_type, out, &inner)?;
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
            if is_mutable_aggregate(payload_ty, ctx.structs, ctx.enums) {
                // Bind an INDEPENDENT deep copy of a mutable-aggregate payload so
                // mutating the binding cannot alter the matched enum's payload
                // record, matching the interpreters' clone-on-bind. A scalar or
                // immutable `string` payload needs no copy.
                emit_deep_copy(ctx, payload_ty, out)?;
            }
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
#[path = "wasm_generics.rs"]
mod generics;
pub(crate) use generics::*;
#[path = "wasm_method.rs"]
mod method;
pub(crate) use method::*;
#[path = "wasm_lowering.rs"]
mod lowering;
pub(crate) use lowering::*;
#[path = "wasm_lowering_mem.rs"]
mod lowering_mem;
pub(crate) use lowering_mem::*;

#[cfg(test)]
#[path = "wasm_tests.rs"]
mod tests;
