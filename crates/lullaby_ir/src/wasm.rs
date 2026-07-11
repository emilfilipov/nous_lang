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
    if enum_layout(ty, structs, enums).is_some() {
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
/// once as `[char_len: i32][byte_len: i32][utf8 bytes]` starting at
/// `RESERVED_BASE`; the value of the literal is the byte offset of its char-count
/// header. The two headers mirror the runtime record layout (see the string
/// record layout notes near `STR_DATA_OFF`) so a literal and a runtime-built
/// string are byte-for-byte interchangeable.
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
            // `to_string(x)` builds a `[char_len][byte_len][utf8]` string record for
            // an integer / bool / char / byte / string argument, matching the
            // interpreters' `Value::Display` bit-for-bit. A float argument
            // (`to_string(f32|f64)`) is DEFERRED — matching Rust's `Display` dtoa in
            // WASM is out of scope — so it errors here and the function falls back to
            // the interpreters.
            if name == TO_STRING_BUILTIN {
                if args.len() != 1 {
                    return Err("`to_string` takes exactly one argument".to_string());
                }
                return lower_to_string(ctx, &args[0], out);
            }
            // Index-based string operations. Each is gated on a `string` first
            // argument so the name cannot shadow a user function of the same
            // spelling: only a genuine `string`-typed call routes here. `substring`
            // and `find` are CHAR-indexed (they decode UTF-8 to map char index to
            // byte offset), while `contains`/`starts_with`/`ends_with` are byte-exact
            // substring/prefix/suffix tests — matching the interpreters bit-for-bit
            // (`builtin_substring`/`builtin_find`/`char_find`/`builtin_contains`/…).
            if name == SUBSTRING_BUILTIN && args.len() == 3 && args[0].ty.name == "string" {
                return lower_substring(ctx, &args[0], &args[1], &args[2], out);
            }
            if name == FIND_BUILTIN && args.len() == 2 && args[0].ty.name == "string" {
                return lower_find(ctx, &args[0], &args[1], out);
            }
            if name == CONTAINS_BUILTIN && args.len() == 2 && args[0].ty.name == "string" {
                return lower_contains(ctx, &args[0], &args[1], out);
            }
            if name == STARTS_WITH_BUILTIN && args.len() == 2 && args[0].ty.name == "string" {
                return lower_starts_with(ctx, &args[0], &args[1], out);
            }
            if name == ENDS_WITH_BUILTIN && args.len() == 2 && args[0].ty.name == "string" {
                return lower_ends_with(ctx, &args[0], &args[1], out);
            }
            // Growable `list<T>` (scalar or `string` `T`) builtins. `list_new()`
            // allocates an empty header; `push`/`get`/`set`/`pop` operate on a
            // `list`-typed first argument (checked so these names cannot shadow a
            // user function or an array op). `len(l)` is NOT special-cased here — a
            // list's `len` shares offset 0 with the string/array length header, so
            // the generic `len` path below reads it. A list op whose element is a
            // MUTABLE heap type is deferred: `supported_list_element` returns
            // `None`, so lowering errors and the function is demoted to the
            // interpreters.
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
            // Growable `map<K, V>` (scalar `K`; scalar or `string` `V`) builtins.
            // `map_new()` allocates an empty `[len][cap][entries]` header;
            // `map_set`/`map_get`/`map_has`/`map_len` operate on a `map`-typed first
            // argument. These names are not shared with any array/list op, so they
            // dispatch on name directly (the arity/key/value types are validated in
            // each lowering). A map op whose key is a heap type, or whose value is a
            // MUTABLE heap type, is deferred: `supported_map_kv` returns `None`, so
            // lowering errors and the function is demoted to the interpreters.
            // `map_len(m)` shares offset 0 with the length header, but (unlike
            // lists) it is spelled `map_len`, so it routes here explicitly.
            if name == MAP_NEW_BUILTIN {
                return lower_map_new(ctx, args, out);
            }
            if name == MAP_SET_BUILTIN && args.len() == 3 && args[0].ty.map_args().is_some() {
                return lower_map_set(ctx, &args[0], &args[1], &args[2], out);
            }
            if name == MAP_GET_BUILTIN && args.len() == 2 && args[0].ty.map_args().is_some() {
                return lower_map_get(ctx, &expr.ty, &args[0], &args[1], out);
            }
            if name == MAP_HAS_BUILTIN && args.len() == 2 && args[0].ty.map_args().is_some() {
                return lower_map_has(ctx, &args[0], &args[1], out);
            }
            if name == MAP_LEN_BUILTIN && args.len() == 1 && args[0].ty.map_args().is_some() {
                return lower_map_len(ctx, &args[0], out);
            }
            // `len(s)`/`len(a)`/`len(l)` reads the leading i32 length header.
            if name == LEN_BUILTIN {
                return lower_len(ctx, args, out);
            }
            // Overflow-aware arithmetic builtins (`checked_*`/`saturating_*`/
            // `wrapping_*`). `wrapping_*` reuses the default fixed-width `+`/`-`/`*`;
            // `saturating_*` clamps to `T`'s bounds; `checked_*` builds an
            // `option<T>` record. Guarded by a fixed-width first operand so the
            // names cannot shadow a user function of the same spelling.
            if let Some((ovf_op, mode)) = overflow_builtin(name)
                && args.len() == 2
                && let Some(kind) = fixed_int_kind(args[0].ty.name.as_str())
            {
                if mode == OverflowMode::Wrapping {
                    lower_expr(ctx, &args[0], out)?;
                    lower_expr(ctx, &args[1], out)?;
                    return emit_fixed_binop(ctx, ovf_op.binary_op(), kind, out);
                }
                return lower_wasm_overflow(
                    ctx, ovf_op, mode, kind, &expr.ty, &args[0], &args[1], out,
                );
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

/// Lower a `string` argument to the two host-import operands `[ptr, len]`: push a
/// pointer to the string's first UTF-8 byte, then its UTF-8 BYTE length. The
/// record pointer is evaluated once into a scratch `i32` local so a non-trivial
/// string expression is not lowered twice; the operand pointer is
/// `record_ptr + STR_DATA_OFF` (past the two `i32` headers) so the host slices
/// `[ptr, ptr + len)` directly, and the length is the record's byte-length header
/// (`STR_BYTE_LEN_OFF`) so multi-byte UTF-8 text decodes correctly — not the char
/// count, which only equals the byte length for ASCII.
fn lower_string_ptr_len(ctx: &mut LowerCtx, arg: &IrExpr, out: &mut Vec<u8>) -> Result<(), String> {
    if value_val_type(&arg.ty, ctx.structs, ctx.enums) != Some(WasmValType::I32)
        || arg.ty.name != "string"
    {
        return Err(format!(
            "console_log/dom_set_text expect a string but got `{}`",
            arg.ty.name
        ));
    }
    lower_expr(ctx, arg, out)?; // string record pointer (i32)
    let ptr = ctx.add_local(WasmValType::I32);
    set_local(out, ptr);
    // operand: record_ptr + STR_DATA_OFF (pointer to the first UTF-8 byte).
    get_local(out, ptr);
    out.push(0x41); // i32.const STR_DATA_OFF
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    // operand: byte length (the second header).
    get_local(out, ptr); // base for the length load
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
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

    // Runtime string concatenation: `a + b` where both operands are `string`
    // allocates a fresh `[char_len][byte_len][utf8 bytes]` record whose bytes are
    // the two operands' byte ranges joined and whose char/byte headers are the
    // sums of the operands' headers. Strings are immutable, so the result is a new
    // record with no aliasing. Any other `+` operand type falls through to the
    // scalar arithmetic paths below.
    if op == BinaryOp::Add && left.ty.name == "string" && right.ty.name == "string" {
        return lower_string_concat(ctx, left, right, out);
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

/// Lower runtime string concatenation `a + b` (both `string`) into a fresh
/// `[char_len: i32][byte_len: i32][utf8 bytes]` record and leave its pointer on
/// the stack.
///
/// Strings are immutable, so concatenation always builds a NEW record (no
/// aliasing): read each operand's char-count and byte-count headers, `__alloc` a
/// record of `STR_DATA_OFF + byte_a + byte_b` bytes, write the summed headers
/// (char count = `char_a + char_b`, byte count = `byte_a + byte_b`), then
/// `memory.copy` each operand's UTF-8 byte range into place. Working in BYTE
/// ranges (not char counts) keeps multi-byte UTF-8 correct; the result's `len`
/// (its char-count header) is `len(a) + len(b)`, matching the interpreters
/// bit-for-bit. Chained `a + b + c` nests naturally: the inner `+` yields a normal
/// string record consumed by the outer `+`.
fn lower_string_concat(
    ctx: &mut LowerCtx,
    left: &IrExpr,
    right: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // Evaluate both operands once into scratch record-pointer locals (each may be
    // a non-trivial expression — a variable, a literal, or a nested concat).
    lower_expr(ctx, left, out)?;
    let a = ctx.add_local(WasmValType::I32);
    set_local(out, a);
    lower_expr(ctx, right, out)?;
    let b = ctx.add_local(WasmValType::I32);
    set_local(out, b);

    // Read the four headers into locals: char and byte counts of each operand.
    let char_a = ctx.add_local(WasmValType::I32);
    get_local(out, a);
    emit_load_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);
    set_local(out, char_a);
    let byte_a = ctx.add_local(WasmValType::I32);
    get_local(out, a);
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    set_local(out, byte_a);
    let char_b = ctx.add_local(WasmValType::I32);
    get_local(out, b);
    emit_load_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);
    set_local(out, char_b);
    let byte_b = ctx.add_local(WasmValType::I32);
    get_local(out, b);
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    set_local(out, byte_b);

    // dst = __alloc(STR_DATA_OFF + byte_a + byte_b): header + both byte ranges.
    out.push(0x41); // i32.const STR_DATA_OFF
    write_sleb(out, STR_DATA_OFF as i64);
    get_local(out, byte_a);
    out.push(0x6a); // i32.add
    get_local(out, byte_b);
    out.push(0x6a); // i32.add
    let dst = alloc_runtime(ctx, out);

    // dst[char_len] = char_a + char_b.
    get_local(out, dst);
    get_local(out, char_a);
    get_local(out, char_b);
    out.push(0x6a); // i32.add
    emit_store_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);
    // dst[byte_len] = byte_a + byte_b.
    get_local(out, dst);
    get_local(out, byte_a);
    get_local(out, byte_b);
    out.push(0x6a); // i32.add
    emit_store_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);

    // memory.copy(dst + STR_DATA_OFF, a + STR_DATA_OFF, byte_a): first operand's
    // bytes. `memory.copy` pops size, src, dest (pushed dest, src, size).
    get_local(out, dst);
    out.push(0x41); // i32.const STR_DATA_OFF
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add -> dest
    get_local(out, a);
    out.push(0x41);
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add -> src
    get_local(out, byte_a); // size
    emit_memory_copy(out);

    // memory.copy(dst + STR_DATA_OFF + byte_a, b + STR_DATA_OFF, byte_b): second
    // operand's bytes appended after the first range.
    get_local(out, dst);
    out.push(0x41);
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    get_local(out, byte_a);
    out.push(0x6a); // i32.add -> dest = dst + STR_DATA_OFF + byte_a
    get_local(out, b);
    out.push(0x41);
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add -> src
    get_local(out, byte_b); // size
    emit_memory_copy(out);

    // The concatenated record's pointer is the value of the expression.
    get_local(out, dst);
    Ok(())
}

/// Emit the `memory.copy` bulk-memory instruction, copying `size` bytes from `src`
/// to `dest` within the single linear memory (all three operands already on the
/// stack in dest, src, size order). Encoded as the `0xfc` misc prefix, sub-opcode
/// `0x0a`, then the destination and source memory indices (both `0` — the module
/// has exactly one memory).
fn emit_memory_copy(out: &mut Vec<u8>) {
    out.push(0xfc); // misc-op prefix
    write_uleb(out, 0x0a); // memory.copy
    out.push(0x00); // dest memory index
    out.push(0x00); // src memory index
}

// -- Index-based string-operation codegen ------------------------------------
//
// These lower the char-indexed `substring`/`find` and the byte-exact
// `contains`/`starts_with`/`ends_with` builtins over the `[char_len][byte_len]
// [utf8 bytes]` string record. The byte scans compare `memory[hay + i]` against
// `memory[needle + j]` with `i32.load8_u`; `find`/`substring` additionally decode
// UTF-8 lead bytes (a byte is a char start iff `(b & 0xC0) != 0x80`) to map char
// indices to byte offsets. Every scan is an inline WASM loop over the UTF-8 bytes,
// matching the interpreters' `str::find`/`str::contains`/`chars()` bit-for-bit.

/// Push a pointer to a string record's first UTF-8 byte: `record_ptr + STR_DATA_OFF`.
/// The record pointer must already be on the stack.
fn emit_add_data_off(out: &mut Vec<u8>) {
    out.push(0x41); // i32.const STR_DATA_OFF
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add
}

/// Emit `i32.load8_u` reading the byte at the address on the stack (offset 0).
fn emit_load8_u(out: &mut Vec<u8>) {
    out.push(0x2d); // i32.load8_u
    write_uleb(out, 0); // align 0 (1-byte)
    write_uleb(out, 0); // offset 0
}

/// Evaluate a `string` expression into a fresh scratch triple of `i32` locals and
/// return them as `(data_ptr, byte_len)`: `data_ptr` points at the first UTF-8
/// byte (`record + STR_DATA_OFF`) and `byte_len` is the UTF-8 byte-length header.
/// The record pointer is evaluated once so a non-trivial string expression is not
/// lowered twice.
fn lower_string_data_len(
    ctx: &mut LowerCtx,
    arg: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(u32, u32), String> {
    if arg.ty.name != "string" {
        return Err(format!(
            "index-based string op expects a string but got `{}`",
            arg.ty.name
        ));
    }
    lower_expr(ctx, arg, out)?; // record pointer (i32)
    let record = ctx.add_local(WasmValType::I32);
    set_local(out, record);
    // data = record + STR_DATA_OFF
    let data = ctx.add_local(WasmValType::I32);
    get_local(out, record);
    emit_add_data_off(out);
    set_local(out, data);
    // byte_len = i32.load [record + STR_BYTE_LEN_OFF]
    let byte_len = ctx.add_local(WasmValType::I32);
    get_local(out, record);
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    set_local(out, byte_len);
    Ok((data, byte_len))
}

/// Emit an expression that leaves an `i32` bool (`1`/`0`) on the stack: whether the
/// `needle` bytes match the haystack bytes starting at byte position `pos`. The
/// caller guarantees `pos + needle_len <= hay_len`, so no bounds check is needed
/// inside; an empty needle (`needle_len == 0`) yields `1` (the inner loop runs zero
/// times), matching Rust's `""` prefix/substring semantics. Emitted as a
/// self-contained `block (result i32)` holding a byte-compare loop.
fn emit_bytes_match_at(
    ctx: &mut LowerCtx,
    hay_data: u32,
    needle_data: u32,
    needle_len: u32,
    pos: u32,
    out: &mut Vec<u8>,
) {
    // result block: j = 0; loop { if j >= needle_len -> push 1, break;
    //   if hay[pos+j] != needle[j] -> push 0, break; j += 1; continue }
    let j = ctx.add_local(WasmValType::I32);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, j);
    out.push(0x02); // block (result i32)
    out.push(0x7f);
    out.push(0x03); // loop (result i32)
    out.push(0x7f);
    // if j >= needle_len -> matched: push 1 and break out of both.
    get_local(out, j);
    get_local(out, needle_len);
    out.push(0x4e); // i32.ge_s
    out.push(0x04); // if (no result — a branch exits the enclosing blocks)
    out.push(0x40);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x0c); // br 2 (leave block with 1 on the stack)
    write_uleb(out, 2);
    out.push(0x0b); // end if
    // if hay[pos + j] != needle[j] -> mismatch: push 0 and break.
    get_local(out, hay_data);
    get_local(out, pos);
    out.push(0x6a); // i32.add
    get_local(out, j);
    out.push(0x6a); // i32.add -> hay_data + pos + j
    emit_load8_u(out);
    get_local(out, needle_data);
    get_local(out, j);
    out.push(0x6a); // i32.add -> needle_data + j
    emit_load8_u(out);
    out.push(0x47); // i32.ne
    out.push(0x04); // if
    out.push(0x40);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    out.push(0x0c); // br 2 (leave block with 0)
    write_uleb(out, 2);
    out.push(0x0b); // end if
    // j += 1; continue the loop.
    get_local(out, j);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, j);
    out.push(0x0c); // br 0 (repeat loop)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block -> i32 bool on the stack
}

/// Emit a scan for the FIRST byte position at which `needle` matches `hay`, storing
/// that byte position into `found_pos` and a `1`/`0` flag into `found_flag`. Mirrors
/// Rust's `str::find` at the byte level: it tries every start `pos` in
/// `0..=(hay_len - needle_len)` and stops at the first full byte match. An empty
/// needle matches at `pos = 0` (the match loop runs zero iterations), matching
/// `"...".find("") == Some(0)`. When `needle_len > hay_len` the outer loop never
/// runs and `found_flag` stays `0`. Returns `(found_pos, found_flag)`: fresh
/// caller-visible `i32` locals holding the matched byte position and the found
/// flag, initialized by this function.
fn emit_byte_search(
    ctx: &mut LowerCtx,
    hay_data: u32,
    hay_len: u32,
    needle_data: u32,
    needle_len: u32,
    out: &mut Vec<u8>,
) -> (u32, u32) {
    let found_pos = ctx.add_local(WasmValType::I32);
    let found_flag = ctx.add_local(WasmValType::I32);
    // found_flag = 0; found_pos = 0.
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, found_flag);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, found_pos);
    // limit = hay_len - needle_len (last valid start position, inclusive). When
    // needle_len > hay_len this is negative, so the `pos <= limit` guard fails
    // immediately and the search reports "not found".
    let limit = ctx.add_local(WasmValType::I32);
    get_local(out, hay_len);
    get_local(out, needle_len);
    out.push(0x6b); // i32.sub
    set_local(out, limit);
    // pos = 0; loop { if pos > limit break; if match_at(pos) { found; break }; pos += 1 }
    let pos = ctx.add_local(WasmValType::I32);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, pos);
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // break when pos > limit (signed; limit may be negative).
    get_local(out, pos);
    get_local(out, limit);
    out.push(0x4a); // i32.gt_s
    out.push(0x0d); // br_if 1 (out of the block)
    write_uleb(out, 1);
    // if bytes_match_at(pos) { found_pos = pos; found_flag = 1; break }
    emit_bytes_match_at(ctx, hay_data, needle_data, needle_len, pos, out);
    out.push(0x04); // if
    out.push(0x40);
    get_local(out, pos);
    set_local(out, found_pos);
    out.push(0x41);
    write_sleb(out, 1);
    set_local(out, found_flag);
    out.push(0x0c); // br 2 (out of the block)
    write_uleb(out, 2);
    out.push(0x0b); // end if
    // pos += 1; continue.
    get_local(out, pos);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, pos);
    out.push(0x0c); // br 0 (repeat)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
    (found_pos, found_flag)
}

/// Emit a loop that counts the number of UTF-8 characters in `data[0..byte_end)`
/// and leaves that count (an `i32`) on the stack. A byte begins a character iff
/// `(b & 0xC0) != 0x80` (it is not a continuation byte), so the char count is the
/// number of non-continuation bytes in the range — exactly what
/// `text[..byte_index].chars().count()` yields in the interpreters' `char_find`.
/// `data` and `byte_end` are `i32` locals; `byte_end` is a byte offset relative to
/// `data`.
fn emit_char_count_upto(ctx: &mut LowerCtx, data: u32, byte_end: u32, out: &mut Vec<u8>) {
    let count = ctx.add_local(WasmValType::I32);
    let bi = ctx.add_local(WasmValType::I32);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, count);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, bi);
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // break when bi >= byte_end.
    get_local(out, bi);
    get_local(out, byte_end);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1
    write_uleb(out, 1);
    // if (mem[data + bi] & 0xC0) != 0x80 -> count += 1 (a char start).
    get_local(out, data);
    get_local(out, bi);
    out.push(0x6a); // i32.add
    emit_load8_u(out);
    out.push(0x41); // i32.const 0xC0
    write_sleb(out, 0xC0);
    out.push(0x71); // i32.and
    out.push(0x41); // i32.const 0x80
    write_sleb(out, 0x80);
    out.push(0x47); // i32.ne
    out.push(0x04); // if
    out.push(0x40);
    get_local(out, count);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, count);
    out.push(0x0b); // end if
    // bi += 1; continue.
    get_local(out, bi);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, bi);
    out.push(0x0c); // br 0
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
    get_local(out, count); // leave the count on the stack
}

/// Emit a loop that advances a byte offset from the start of `data` past exactly
/// `target_char` whole UTF-8 characters, storing the resulting byte offset into the
/// caller-owned `i32` local `out_byte`. Each step moves past one lead byte and then
/// over all following continuation bytes (`(b & 0xC0) == 0x80`). For
/// `target_char == char_count` this lands on `byte_len` (one past the last byte).
/// The string is well-formed UTF-8 and `target_char <= char_count` is guaranteed by
/// the caller's bounds check, so the walk terminates in range.
fn emit_char_index_to_byte(
    ctx: &mut LowerCtx,
    data: u32,
    byte_len: u32,
    target_char: u32,
    out_byte: u32,
    out: &mut Vec<u8>,
) {
    // bi = 0; c = 0; loop { if c >= target_char break; bi += 1;
    //   while bi < byte_len and (mem[data+bi] & 0xC0)==0x80 { bi += 1 }; c += 1 }
    let c = ctx.add_local(WasmValType::I32);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, out_byte); // bi
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, c);
    out.push(0x02); // outer block
    out.push(0x40);
    out.push(0x03); // outer loop (over chars)
    out.push(0x40);
    // break when c >= target_char.
    get_local(out, c);
    get_local(out, target_char);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of outer block)
    write_uleb(out, 1);
    // bi += 1 (past the lead byte).
    get_local(out, out_byte);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, out_byte);
    // inner loop: while bi < byte_len and mem[data+bi] is a continuation byte { bi += 1 }
    out.push(0x02); // inner block
    out.push(0x40);
    out.push(0x03); // inner loop
    out.push(0x40);
    // break inner when bi >= byte_len.
    get_local(out, out_byte);
    get_local(out, byte_len);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of inner block)
    write_uleb(out, 1);
    // break inner when NOT a continuation byte: (mem[data+bi] & 0xC0) != 0x80.
    get_local(out, data);
    get_local(out, out_byte);
    out.push(0x6a); // i32.add
    emit_load8_u(out);
    out.push(0x41);
    write_sleb(out, 0xC0);
    out.push(0x71); // i32.and
    out.push(0x41);
    write_sleb(out, 0x80);
    out.push(0x47); // i32.ne
    out.push(0x0d); // br_if 1 (out of inner block — reached the next char start)
    write_uleb(out, 1);
    // bi += 1; continue inner.
    get_local(out, out_byte);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, out_byte);
    out.push(0x0c); // br 0 (repeat inner)
    write_uleb(out, 0);
    out.push(0x0b); // end inner loop
    out.push(0x0b); // end inner block
    // c += 1; continue outer.
    get_local(out, c);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, c);
    out.push(0x0c); // br 0 (repeat outer)
    write_uleb(out, 0);
    out.push(0x0b); // end outer loop
    out.push(0x0b); // end outer block
}

/// Lower `substring(s, start, end) -> string`: the char-indexed half-open
/// `[start, end)` slice. Matches `builtin_substring` exactly: `start`/`end` are
/// char indices; if `start < 0 || end < 0 || start > end || end > char_count` the
/// range is out of bounds and the interpreters raise `L0413`, so the WASM path
/// traps (`unreachable`) rather than producing a wrong value. Otherwise the slice's
/// char indices are mapped to byte offsets by walking the UTF-8, a fresh
/// `[char_len][byte_len][utf8]` record is allocated, and the byte range is
/// `memory.copy`'d in.
fn lower_substring(
    ctx: &mut LowerCtx,
    s: &IrExpr,
    start: &IrExpr,
    end: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // Evaluate the source string into (data, byte_len); also read its char-count
    // header for the bounds check.
    lower_expr(ctx, s, out)?; // record pointer
    let record = ctx.add_local(WasmValType::I32);
    set_local(out, record);
    let char_count = ctx.add_local(WasmValType::I32);
    get_local(out, record);
    emit_load_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);
    set_local(out, char_count);
    let byte_len = ctx.add_local(WasmValType::I32);
    get_local(out, record);
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    set_local(out, byte_len);
    let data = ctx.add_local(WasmValType::I32);
    get_local(out, record);
    emit_add_data_off(out);
    set_local(out, data);

    // start/end are i64 char indices; narrow to i32 for offset math (a valid char
    // index fits in i32 — a string cannot hold more than 2^31 chars in a wasm32
    // linear memory). Keep the i64 values for the bounds comparisons so a huge or
    // negative index is rejected exactly like the interpreters.
    let start64 = ctx.add_local(WasmValType::I64);
    lower_expr(ctx, start, out)?;
    set_local(out, start64);
    let end64 = ctx.add_local(WasmValType::I64);
    lower_expr(ctx, end, out)?;
    set_local(out, end64);

    // Bounds check (traps on failure): start < 0 || end < 0 || start > end ||
    // end > char_count. char_count is an i32 count; extend it to i64 for the
    // comparison.
    // cond = (start < 0) | (end < 0) | (start > end) | (end > char_count)
    get_local(out, start64);
    out.push(0x42); // i64.const 0
    write_sleb(out, 0);
    out.push(0x53); // i64.lt_s -> start < 0
    get_local(out, end64);
    out.push(0x42);
    write_sleb(out, 0);
    out.push(0x53); // end < 0
    out.push(0x72); // i32.or
    get_local(out, start64);
    get_local(out, end64);
    out.push(0x55); // i64.gt_s -> start > end
    out.push(0x72); // i32.or
    get_local(out, end64);
    get_local(out, char_count);
    out.push(0xac); // i64.extend_i32_s (char_count -> i64)
    out.push(0x55); // i64.gt_s -> end > char_count
    out.push(0x72); // i32.or
    out.push(0x04); // if (out-of-bounds) { unreachable }
    out.push(0x40);
    out.push(0x00); // unreachable (trap — mirrors the interpreters' L0413)
    out.push(0x0b); // end if

    // start_char / end_char as i32 char indices.
    let start_char = ctx.add_local(WasmValType::I32);
    get_local(out, start64);
    out.push(0xa7); // i32.wrap_i64
    set_local(out, start_char);
    let end_char = ctx.add_local(WasmValType::I32);
    get_local(out, end64);
    out.push(0xa7); // i32.wrap_i64
    set_local(out, end_char);

    // Map char indices to byte offsets by walking the UTF-8.
    let start_byte = ctx.add_local(WasmValType::I32);
    emit_char_index_to_byte(ctx, data, byte_len, start_char, start_byte, out);
    let end_byte = ctx.add_local(WasmValType::I32);
    emit_char_index_to_byte(ctx, data, byte_len, end_char, end_byte, out);

    // slice_bytes = end_byte - start_byte; slice_chars = end_char - start_char.
    let slice_bytes = ctx.add_local(WasmValType::I32);
    get_local(out, end_byte);
    get_local(out, start_byte);
    out.push(0x6b); // i32.sub
    set_local(out, slice_bytes);

    // dst = __alloc(STR_DATA_OFF + slice_bytes).
    out.push(0x41); // i32.const STR_DATA_OFF
    write_sleb(out, STR_DATA_OFF as i64);
    get_local(out, slice_bytes);
    out.push(0x6a); // i32.add
    let dst = alloc_runtime(ctx, out);
    // dst.char_len = end_char - start_char.
    get_local(out, dst);
    get_local(out, end_char);
    get_local(out, start_char);
    out.push(0x6b); // i32.sub
    emit_store_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);
    // dst.byte_len = slice_bytes.
    get_local(out, dst);
    get_local(out, slice_bytes);
    emit_store_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    // memory.copy(dst + STR_DATA_OFF, data + start_byte, slice_bytes).
    get_local(out, dst);
    emit_add_data_off(out); // dest
    get_local(out, data);
    get_local(out, start_byte);
    out.push(0x6a); // i32.add -> src
    get_local(out, slice_bytes); // size
    emit_memory_copy(out);

    get_local(out, dst); // the slice record's pointer is the value
    Ok(())
}

/// Lower `find(haystack, needle) -> i64`: the CHAR index of the first byte-level
/// occurrence of `needle`, or `-1` if absent. Matches `char_find` exactly: byte
/// search for the first match, then count the UTF-8 characters preceding that byte
/// offset (`text[..byte_index].chars().count()`). An empty needle finds at byte 0,
/// whose preceding char count is 0, so `find(s, "") == 0` — matching Rust's
/// `find("") == Some(0)`.
fn lower_find(
    ctx: &mut LowerCtx,
    haystack: &IrExpr,
    needle: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (hay_data, hay_len) = lower_string_data_len(ctx, haystack, out)?;
    let (needle_data, needle_len) = lower_string_data_len(ctx, needle, out)?;
    let (found_pos, found_flag) =
        emit_byte_search(ctx, hay_data, hay_len, needle_data, needle_len, out);
    // if found_flag { char_count(hay[0..found_pos]) as i64 } else { -1 }
    get_local(out, found_flag);
    out.push(0x04); // if (result i64)
    out.push(0x7e);
    emit_char_count_upto(ctx, hay_data, found_pos, out); // i32 char index
    out.push(0xac); // i64.extend_i32_s
    out.push(0x05); // else
    out.push(0x42); // i64.const -1
    write_sleb(out, -1);
    out.push(0x0b); // end if -> i64 on the stack
    Ok(())
}

/// Lower `contains(s, sub) -> bool`: byte-exact substring test. Emits the same
/// byte search as `find` and yields its found flag (`1`/`0`). An empty `sub` is
/// contained (matches at byte 0), matching Rust's `str::contains("")`.
fn lower_contains(
    ctx: &mut LowerCtx,
    s: &IrExpr,
    sub: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (hay_data, hay_len) = lower_string_data_len(ctx, s, out)?;
    let (needle_data, needle_len) = lower_string_data_len(ctx, sub, out)?;
    let (_found_pos, found_flag) =
        emit_byte_search(ctx, hay_data, hay_len, needle_data, needle_len, out);
    get_local(out, found_flag); // i32 bool result
    Ok(())
}

/// Lower `starts_with(s, prefix) -> bool`: byte-exact prefix test. If
/// `prefix_len > s_len` the result is `0`; otherwise it is whether the prefix bytes
/// match at byte position 0. An empty prefix matches, mirroring
/// `str::starts_with("")`.
fn lower_starts_with(
    ctx: &mut LowerCtx,
    s: &IrExpr,
    prefix: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (hay_data, hay_len) = lower_string_data_len(ctx, s, out)?;
    let (needle_data, needle_len) = lower_string_data_len(ctx, prefix, out)?;
    // if needle_len > hay_len { 0 } else { bytes_match_at(pos = 0) }
    get_local(out, needle_len);
    get_local(out, hay_len);
    out.push(0x4a); // i32.gt_s
    out.push(0x04); // if (result i32)
    out.push(0x7f);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    out.push(0x05); // else
    let pos = ctx.add_local(WasmValType::I32);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, pos);
    emit_bytes_match_at(ctx, hay_data, needle_data, needle_len, pos, out);
    out.push(0x0b); // end if -> i32 bool
    Ok(())
}

/// Lower `ends_with(s, suffix) -> bool`: byte-exact suffix test. If
/// `suffix_len > s_len` the result is `0`; otherwise it is whether the suffix bytes
/// match at byte position `s_len - suffix_len`. An empty suffix matches, mirroring
/// `str::ends_with("")`.
fn lower_ends_with(
    ctx: &mut LowerCtx,
    s: &IrExpr,
    suffix: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (hay_data, hay_len) = lower_string_data_len(ctx, s, out)?;
    let (needle_data, needle_len) = lower_string_data_len(ctx, suffix, out)?;
    // if needle_len > hay_len { 0 } else { bytes_match_at(pos = hay_len - needle_len) }
    get_local(out, needle_len);
    get_local(out, hay_len);
    out.push(0x4a); // i32.gt_s
    out.push(0x04); // if (result i32)
    out.push(0x7f);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    out.push(0x05); // else
    let pos = ctx.add_local(WasmValType::I32);
    get_local(out, hay_len);
    get_local(out, needle_len);
    out.push(0x6b); // i32.sub
    set_local(out, pos);
    emit_bytes_match_at(ctx, hay_data, needle_data, needle_len, pos, out);
    out.push(0x0b); // end if -> i32 bool
    Ok(())
}

// -- to_string codegen -------------------------------------------------------
//
// `to_string(x)` produces a fresh `[char_len: i32][byte_len: i32][utf8 bytes]`
// string record (see the string record layout notes near `STR_DATA_OFF`),
// interchangeable with string literals and concatenation results. The output
// matches the interpreters' `Value::Display`:
//   - `i64`/signed fixed-width/`isize`: decimal, leading `-` for negatives.
//   - `u64`/unsigned fixed-width/`usize`/`byte`: unsigned decimal magnitude.
//   - `bool`: `"true"` / `"false"` (interned literals).
//   - `char`: the 1–4 byte UTF-8 encoding of the scalar (char_len = 1).
//   - `string`: identity — strings are immutable, so the same pointer is returned.
// A float argument is deferred (see the caller).

/// Lower `to_string(x)` for the supported argument types, leaving the resulting
/// string record's `i32` pointer on the stack. Dispatches on the argument's IR
/// type. A float argument errors so the enclosing function falls back to the
/// interpreters.
fn lower_to_string(ctx: &mut LowerCtx, arg: &IrExpr, out: &mut Vec<u8>) -> Result<(), String> {
    match arg.ty.name.as_str() {
        // A `string` is already a record; strings are immutable, so returning the
        // same pointer is value-equivalent to the interpreters' clone.
        "string" => lower_expr(ctx, arg, out),
        // `bool` prints `true`/`false`: select the interned literal pointer.
        "bool" => lower_bool_to_string(ctx, arg, out),
        // `char` prints its UTF-8 encoding (1–4 bytes, char_len = 1).
        "char" => lower_char_to_string(ctx, arg, out),
        // `byte` is a 0–255 magnitude held in an `i32` cell: unsigned itoa.
        "byte" => {
            lower_expr(ctx, arg, out)?;
            // Widen the i32 byte cell to an i64 magnitude (unsigned: 0..255).
            out.push(0xad); // i64.extend_i32_u
            emit_itoa_unsigned(ctx, out);
            Ok(())
        }
        // `i64` (plain signed) and the fixed-width integer kinds. Unsigned kinds
        // print the u64 reinterpretation of their normalized cell; signed kinds
        // print the signed value with a leading `-` for negatives.
        "i64" => {
            lower_expr(ctx, arg, out)?;
            emit_itoa_signed(ctx, out);
            Ok(())
        }
        name => match fixed_int_kind(name) {
            Some(kind) if kind.is_unsigned() => {
                lower_expr(ctx, arg, out)?;
                emit_itoa_unsigned(ctx, out);
                Ok(())
            }
            Some(_) => {
                lower_expr(ctx, arg, out)?;
                emit_itoa_signed(ctx, out);
                Ok(())
            }
            // Floats and everything else are deferred to the interpreters.
            None => Err(format!(
                "to_string of `{name}` is not supported by the WASM backend"
            )),
        },
    }
}

/// Lower `to_string(b)` for a `bool`: push the pointer of the interned `"true"`
/// literal when `b` is nonzero, else the interned `"false"` literal, via a typed
/// `if`/`else` yielding an `i32`.
fn lower_bool_to_string(ctx: &mut LowerCtx, arg: &IrExpr, out: &mut Vec<u8>) -> Result<(), String> {
    let true_ptr = ctx.pool.intern("true");
    let false_ptr = ctx.pool.intern("false");
    lower_expr(ctx, arg, out)?; // bool condition (i32 0/1)
    out.push(0x04); // if
    out.push(WasmValType::I32.byte()); // block type: yields i32
    out.push(0x41); // i32.const true_ptr
    write_sleb(out, true_ptr as i64);
    out.push(0x05); // else
    out.push(0x41); // i32.const false_ptr
    write_sleb(out, false_ptr as i64);
    out.push(0x0b); // end
    Ok(())
}

/// Lower `to_string(c)` for a `char`: encode the Unicode scalar (an `i32` code
/// point) to its 1–4 byte UTF-8 sequence in a fresh record with `char_len == 1`
/// and `byte_len` the encoded length. The scalar is guaranteed valid (the type
/// checker only admits real `char` values), so the four ranges below are
/// exhaustive over Unicode scalars.
fn lower_char_to_string(ctx: &mut LowerCtx, arg: &IrExpr, out: &mut Vec<u8>) -> Result<(), String> {
    lower_expr(ctx, arg, out)?;
    let code = ctx.add_local(WasmValType::I32);
    set_local(out, code);

    // Allocate the maximum record (header + 4 UTF-8 bytes). Only `byte_len` bytes
    // are meaningful; the bump allocator never reclaims, so an over-allocation of
    // a few bytes is harmless and keeps the size a compile-time constant.
    let dst = alloc_bytes(ctx, STR_DATA_OFF + 4, out);
    // char_len is always 1 for a single scalar.
    get_local(out, dst);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    emit_store_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);

    // byte_len local, computed alongside the byte writes.
    let byte_len = ctx.add_local(WasmValType::I32);

    // if code < 0x80 { 1-byte } else if < 0x800 { 2-byte } else if < 0x10000
    // { 3-byte } else { 4-byte }. Each arm writes its bytes at dst+STR_DATA_OFF..
    // and sets byte_len.
    // --- code < 0x80 ---
    get_local(out, code);
    out.push(0x41);
    write_sleb(out, 0x80);
    out.push(0x48); // i32.lt_s
    out.push(0x04); // if
    out.push(0x40); // block type: void
    // dst[data+0] = code
    emit_store_byte_at(dst, STR_DATA_OFF, |o| get_local(o, code), out);
    set_byte_len(byte_len, 1, out);
    out.push(0x05); // else
    // --- code < 0x800 ---
    get_local(out, code);
    out.push(0x41);
    write_sleb(out, 0x800);
    out.push(0x48); // i32.lt_s
    out.push(0x04); // if
    out.push(0x40);
    // b0 = 0xC0 | (code >> 6)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF,
        |o| {
            push_or(o, 0xC0, |o| push_shr_u(o, code, 6));
        },
        out,
    );
    // b1 = 0x80 | (code & 0x3F)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF + 1,
        |o| {
            push_or(o, 0x80, |o| push_and(o, code, 0x3F));
        },
        out,
    );
    set_byte_len(byte_len, 2, out);
    out.push(0x05); // else
    // --- code < 0x10000 ---
    get_local(out, code);
    out.push(0x41);
    write_sleb(out, 0x10000);
    out.push(0x48); // i32.lt_s
    out.push(0x04); // if
    out.push(0x40);
    // b0 = 0xE0 | (code >> 12)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF,
        |o| {
            push_or(o, 0xE0, |o| push_shr_u(o, code, 12));
        },
        out,
    );
    // b1 = 0x80 | ((code >> 6) & 0x3F)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF + 1,
        |o| {
            push_or(o, 0x80, |o| push_and_of_shr(o, code, 6, 0x3F));
        },
        out,
    );
    // b2 = 0x80 | (code & 0x3F)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF + 2,
        |o| {
            push_or(o, 0x80, |o| push_and(o, code, 0x3F));
        },
        out,
    );
    set_byte_len(byte_len, 3, out);
    out.push(0x05); // else
    // --- 4-byte: code >= 0x10000 ---
    // b0 = 0xF0 | (code >> 18)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF,
        |o| {
            push_or(o, 0xF0, |o| push_shr_u(o, code, 18));
        },
        out,
    );
    // b1 = 0x80 | ((code >> 12) & 0x3F)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF + 1,
        |o| {
            push_or(o, 0x80, |o| push_and_of_shr(o, code, 12, 0x3F));
        },
        out,
    );
    // b2 = 0x80 | ((code >> 6) & 0x3F)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF + 2,
        |o| {
            push_or(o, 0x80, |o| push_and_of_shr(o, code, 6, 0x3F));
        },
        out,
    );
    // b3 = 0x80 | (code & 0x3F)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF + 3,
        |o| {
            push_or(o, 0x80, |o| push_and(o, code, 0x3F));
        },
        out,
    );
    set_byte_len(byte_len, 4, out);
    // Close the three nested `if`s (`< 0x10000`, `< 0x800`, `< 0x80`); the 4-byte
    // case is the innermost `else`, so it needs no `end` of its own.
    out.push(0x0b); // end (`< 0x10000` if)
    out.push(0x0b); // end (`< 0x800` if)
    out.push(0x0b); // end (`< 0x80` if)

    // dst[byte_len] = byte_len local.
    get_local(out, dst);
    get_local(out, byte_len);
    emit_store_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);

    // The record pointer is the result.
    get_local(out, dst);
    Ok(())
}

/// Store one byte at `dst + offset`: push `dst + offset`, then `value_fn` pushes
/// the byte value, then `i32.store8`.
fn emit_store_byte_at(
    dst: u32,
    offset: i32,
    value_fn: impl FnOnce(&mut Vec<u8>),
    out: &mut Vec<u8>,
) {
    get_local(out, dst);
    value_fn(out);
    out.push(0x3a); // i32.store8
    write_uleb(out, 0); // align 0 (1-byte)
    write_uleb(out, offset as u64);
}

/// Push `constant | inner(...)` as an `i32`: push `constant`, run `inner` (which
/// leaves an i32), `i32.or`.
fn push_or(out: &mut Vec<u8>, constant: i64, inner: impl FnOnce(&mut Vec<u8>)) {
    out.push(0x41); // i32.const constant
    write_sleb(out, constant);
    inner(out);
    out.push(0x72); // i32.or
}

/// Push `local >> shift` (logical) as an `i32`.
fn push_shr_u(out: &mut Vec<u8>, local: u32, shift: i64) {
    get_local(out, local);
    out.push(0x41); // i32.const shift
    write_sleb(out, shift);
    out.push(0x76); // i32.shr_u
}

/// Push `local & mask` as an `i32`.
fn push_and(out: &mut Vec<u8>, local: u32, mask: i64) {
    get_local(out, local);
    out.push(0x41); // i32.const mask
    write_sleb(out, mask);
    out.push(0x71); // i32.and
}

/// Push `(local >> shift) & mask` as an `i32`.
fn push_and_of_shr(out: &mut Vec<u8>, local: u32, shift: i64, mask: i64) {
    push_shr_u(out, local, shift);
    out.push(0x41); // i32.const mask
    write_sleb(out, mask);
    out.push(0x71); // i32.and
}

/// Store the constant `value` into the `byte_len` local (an i32).
fn set_byte_len(byte_len: u32, value: i64, out: &mut Vec<u8>) {
    out.push(0x41); // i32.const value
    write_sleb(out, value);
    set_local(out, byte_len);
}

/// Emit signed integer-to-decimal: consume the `i64` value on the stack and leave
/// a fresh string record pointer. A negative value writes a leading `-` and
/// formats its magnitude; `i64::MIN` is handled by computing the magnitude in
/// unsigned space (`0 - value` wraps to the correct unsigned magnitude), so the
/// unformattable positive `-i64::MIN` is never needed.
fn emit_itoa_signed(ctx: &mut LowerCtx, out: &mut Vec<u8>) {
    let value = ctx.add_local(WasmValType::I64);
    set_local(out, value);
    // sign = (value < 0) as i32.
    let sign = ctx.add_local(WasmValType::I32);
    get_local(out, value);
    out.push(0x42); // i64.const 0
    write_sleb(out, 0);
    out.push(0x53); // i64.lt_s
    set_local(out, sign);
    // magnitude = value < 0 ? (0 - value) : value, computed via unsigned wrap so
    // `i64::MIN` yields its correct u64 magnitude (0x8000000000000000).
    let mag = ctx.add_local(WasmValType::I64);
    get_local(out, sign);
    out.push(0x04); // if
    out.push(WasmValType::I64.byte()); // yields i64
    out.push(0x42); // i64.const 0
    write_sleb(out, 0);
    get_local(out, value);
    out.push(0x7d); // i64.sub  -> 0 - value (wrapping)
    out.push(0x05); // else
    get_local(out, value);
    out.push(0x0b); // end
    set_local(out, mag);
    emit_itoa_core(ctx, mag, sign, out);
}

/// Emit unsigned integer-to-decimal: consume the `i64` magnitude on the stack
/// (interpreted as `u64`) and leave a fresh string record pointer. No sign is
/// written.
fn emit_itoa_unsigned(ctx: &mut LowerCtx, out: &mut Vec<u8>) {
    let mag = ctx.add_local(WasmValType::I64);
    set_local(out, mag);
    // sign = 0 (no leading `-`).
    let sign = ctx.add_local(WasmValType::I32);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, sign);
    emit_itoa_core(ctx, mag, sign, out);
}

/// The shared itoa core: format the unsigned `u64` magnitude in `mag` with an
/// optional leading `-` when `sign` is nonzero, leaving a fresh
/// `[char_len][byte_len][utf8]` record pointer on the stack. All output is ASCII,
/// so `char_len == byte_len == sign + digit_count`.
///
/// Two passes over the magnitude: pass one counts decimal digits (`0` is one
/// digit), pass two writes them least-significant-first into the tail of the data
/// region, moving a write cursor backward from the last byte so the digits land
/// in print order. The record is allocated once the digit count is known.
fn emit_itoa_core(ctx: &mut LowerCtx, mag: u32, sign: u32, out: &mut Vec<u8>) {
    // --- Pass 1: ndigits = number of decimal digits in `mag` (>= 1). ---
    // A do-while counting loop (`block { loop { body; br_if 1 exit; br 0 } }`, the
    // same idiom the list/map loops use): each iteration counts one digit and
    // divides `scratch` down, so `mag == 0` still counts a single digit.
    let ndigits = ctx.add_local(WasmValType::I32);
    let scratch = ctx.add_local(WasmValType::I64);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, ndigits);
    get_local(out, mag);
    set_local(out, scratch);
    out.push(0x02); // block
    out.push(0x40); // void
    out.push(0x03); // loop
    out.push(0x40); // void
    // ndigits += 1
    get_local(out, ndigits);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, ndigits);
    // scratch /= 10
    get_local(out, scratch);
    out.push(0x42);
    write_sleb(out, 10);
    out.push(0x80); // i64.div_u
    set_local(out, scratch);
    // exit the block when scratch == 0.
    get_local(out, scratch);
    out.push(0x42);
    write_sleb(out, 0);
    out.push(0x51); // i64.eq
    out.push(0x0d); // br_if 1 (exit block)
    write_uleb(out, 1);
    out.push(0x0c); // br 0 (repeat loop)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block

    // total_len = sign + ndigits.
    let total = ctx.add_local(WasmValType::I32);
    get_local(out, sign);
    get_local(out, ndigits);
    out.push(0x6a); // i32.add
    set_local(out, total);

    // dst = __alloc(STR_DATA_OFF + total).
    out.push(0x41);
    write_sleb(out, STR_DATA_OFF as i64);
    get_local(out, total);
    out.push(0x6a); // i32.add
    let dst = alloc_runtime(ctx, out);

    // Headers: char_len = byte_len = total (all ASCII).
    get_local(out, dst);
    get_local(out, total);
    emit_store_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);
    get_local(out, dst);
    get_local(out, total);
    emit_store_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);

    // Optional leading '-' at dst + STR_DATA_OFF (only when sign != 0).
    get_local(out, sign);
    out.push(0x04); // if
    out.push(0x40);
    get_local(out, dst);
    out.push(0x41);
    write_sleb(out, b'-' as i64);
    out.push(0x3a); // i32.store8
    write_uleb(out, 0);
    write_uleb(out, STR_DATA_OFF as u64);
    out.push(0x0b); // end if

    // --- Pass 2: write digits from the tail backward. ---
    // cursor = dst + STR_DATA_OFF + total - 1  (address of the last byte).
    let cursor = ctx.add_local(WasmValType::I32);
    get_local(out, dst);
    out.push(0x41);
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    get_local(out, total);
    out.push(0x6a); // i32.add
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6b); // i32.sub  -> last byte address
    set_local(out, cursor);

    // scratch = mag; then a do-while writing one digit per iteration (so `0`
    // writes a single '0').
    get_local(out, mag);
    set_local(out, scratch);
    out.push(0x03); // loop
    out.push(0x40);
    // *cursor = '0' + (scratch % 10).
    get_local(out, cursor);
    out.push(0x41);
    write_sleb(out, b'0' as i64);
    // (scratch % 10) as i32
    get_local(out, scratch);
    out.push(0x42);
    write_sleb(out, 10);
    out.push(0x82); // i64.rem_u
    out.push(0xa7); // i32.wrap_i64
    out.push(0x6a); // i32.add -> '0' + digit
    out.push(0x3a); // i32.store8
    write_uleb(out, 0);
    write_uleb(out, 0);
    // cursor -= 1.
    get_local(out, cursor);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6b); // i32.sub
    set_local(out, cursor);
    // scratch /= 10.
    get_local(out, scratch);
    out.push(0x42);
    write_sleb(out, 10);
    out.push(0x80); // i64.div_u
    set_local(out, scratch);
    // continue while scratch != 0.
    get_local(out, scratch);
    out.push(0x42);
    write_sleb(out, 0);
    out.push(0x52); // i64.ne
    out.push(0x0d); // br_if 0 -> repeat while nonzero
    write_uleb(out, 0);
    out.push(0x0b); // end loop

    // The record pointer is the result.
    get_local(out, dst);
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
        BinaryOp::Remainder => {
            // WASM `rem_s`/`rem_u` need no overflow guard: `rem_s` returns 0 for
            // `MIN % -1` (matching `wrapping_rem`) and traps only on a zero
            // divisor, exactly like the interpreters' remainder path.
            if kind.is_unsigned() {
                out.push(0x82); // i64.rem_u
            } else {
                out.push(0x81); // i64.rem_s
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

// -- Overflow-aware arithmetic (checked/saturating/wrapping) -----------------
//
// The overflow-aware builtins operate on two operands of the same fixed-width
// kind `T` (`i8`…`u64`/`isize`/`usize`; `i64` is excluded by the type checker).
// `wrapping_*` reuses the default fixed-width `+`/`-`/`*` (wrap then normalize).
// `saturating_*` and `checked_*` detect overflow with comparison-only formulas
// on the normalized operands (no host carry flags exist in WASM), producing the
// same clamp/`none`/`some` result as the interpreters' `overflow_arith` for every
// width and sign. Division appears only in the 64-bit `mul` overflow tests and is
// always guarded (by a structured `if` on a zero divisor, plus the signed
// `MIN / -1` guard) so no case can trap.

/// The arithmetic operation of an overflow-aware builtin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverflowOp {
    Add,
    Sub,
    Mul,
}

impl OverflowOp {
    /// The wrapping [`BinaryOp`] this operation shares with the default `+`/`-`/`*`
    /// (used to route `wrapping_*` through the fixed-width binary-op emitter).
    fn binary_op(self) -> BinaryOp {
        match self {
            OverflowOp::Add => BinaryOp::Add,
            OverflowOp::Sub => BinaryOp::Subtract,
            OverflowOp::Mul => BinaryOp::Multiply,
        }
    }

    /// The bare `i64.add`/`i64.sub`/`i64.mul` opcode.
    fn wasm_opcode(self) -> u8 {
        match self {
            OverflowOp::Add => 0x7c,
            OverflowOp::Sub => 0x7d,
            OverflowOp::Mul => 0x7e,
        }
    }
}

/// The overflow behaviour of an overflow-aware builtin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverflowMode {
    Wrapping,
    Saturating,
    Checked,
}

/// Recognize an overflow-aware arithmetic builtin name (`checked_add`,
/// `saturating_mul`, `wrapping_sub`, …), returning its `(op, mode)`.
fn overflow_builtin(name: &str) -> Option<(OverflowOp, OverflowMode)> {
    let (mode, op) = name.split_once('_')?;
    let mode = match mode {
        "checked" => OverflowMode::Checked,
        "saturating" => OverflowMode::Saturating,
        "wrapping" => OverflowMode::Wrapping,
        _ => return None,
    };
    let op = match op {
        "add" => OverflowOp::Add,
        "sub" => OverflowOp::Sub,
        "mul" => OverflowOp::Mul,
        _ => return None,
    };
    Some((op, mode))
}

/// `i64.const v`.
fn push_i64_const(out: &mut Vec<u8>, v: i64) {
    out.push(0x42);
    write_sleb(out, v);
}

/// `i32.const v`.
fn push_i32_const(out: &mut Vec<u8>, v: i32) {
    out.push(0x41);
    write_sleb(out, i64::from(v));
}

/// Push an `i32` boolean (`1` iff `a <op> b` overflows `kind`), leaving it on the
/// stack. `a`/`b` are the normalized operands; `wrapped` is `normalize(a op b)`
/// (used by the 64-bit signed `mul` division test). Comparison-only, matching
/// [`lullaby_runtime`]'s `overflow_arith` exactly.
fn push_wasm_overflow_flag(
    ctx: &mut LowerCtx,
    op: OverflowOp,
    kind: IntKind,
    a: u32,
    b: u32,
    wrapped: u32,
    out: &mut Vec<u8>,
) {
    let (min_i128, max_i128) = kind.range_i128();
    let min = min_i128 as i64;
    let max = max_i128 as i64;
    let w64 = matches!(kind, IntKind::U64 | IntKind::Usize | IntKind::Isize);
    let unsigned = kind.is_unsigned();
    match op {
        OverflowOp::Add if unsigned => {
            // a >u (MAX - b)
            get_local(out, a);
            push_i64_const(out, max);
            get_local(out, b);
            out.push(0x7d); // i64.sub
            out.push(0x56); // i64.gt_u
        }
        OverflowOp::Add => {
            // pos = (b > 0) & (a > MAX - b)
            get_local(out, b);
            push_i64_const(out, 0);
            out.push(0x55); // i64.gt_s
            get_local(out, a);
            push_i64_const(out, max);
            get_local(out, b);
            out.push(0x7d); // i64.sub
            out.push(0x55); // i64.gt_s
            out.push(0x71); // i32.and
            // neg = (b < 0) & (a < MIN - b)
            get_local(out, b);
            push_i64_const(out, 0);
            out.push(0x53); // i64.lt_s
            get_local(out, a);
            push_i64_const(out, min);
            get_local(out, b);
            out.push(0x7d); // i64.sub
            out.push(0x53); // i64.lt_s
            out.push(0x71); // i32.and
            out.push(0x72); // i32.or
        }
        OverflowOp::Sub if unsigned => {
            // a <u b
            get_local(out, a);
            get_local(out, b);
            out.push(0x54); // i64.lt_u
        }
        OverflowOp::Sub => {
            // pos = (b < 0) & (a > MAX + b)
            get_local(out, b);
            push_i64_const(out, 0);
            out.push(0x53); // i64.lt_s
            get_local(out, a);
            push_i64_const(out, max);
            get_local(out, b);
            out.push(0x7c); // i64.add
            out.push(0x55); // i64.gt_s
            out.push(0x71); // i32.and
            // neg = (b > 0) & (a < MIN + b)
            get_local(out, b);
            push_i64_const(out, 0);
            out.push(0x55); // i64.gt_s
            get_local(out, a);
            push_i64_const(out, min);
            get_local(out, b);
            out.push(0x7c); // i64.add
            out.push(0x53); // i64.lt_s
            out.push(0x71); // i32.and
            out.push(0x72); // i32.or
        }
        OverflowOp::Mul if !w64 => {
            // Narrow: the exact product fits i64; range-check it against [min, max].
            if unsigned {
                get_local(out, a);
                get_local(out, b);
                out.push(0x7e); // i64.mul
                push_i64_const(out, max);
                out.push(0x56); // i64.gt_u
            } else {
                let prod = ctx.add_local(WasmValType::I64);
                get_local(out, a);
                get_local(out, b);
                out.push(0x7e); // i64.mul
                set_local(out, prod);
                get_local(out, prod);
                push_i64_const(out, max);
                out.push(0x55); // i64.gt_s
                get_local(out, prod);
                push_i64_const(out, min);
                out.push(0x53); // i64.lt_s
                out.push(0x72); // i32.or
            }
        }
        OverflowOp::Mul if unsigned => {
            // 64-bit unsigned: overflow iff a*b > MAX iff (b != 0) & (a > MAX/u b).
            // Guard the divide-by-zero with a structured `if` (WASM `i32.and` does
            // not short-circuit).
            get_local(out, b);
            out.push(0x50); // i64.eqz
            out.push(0x04); // if
            out.push(0x7f); // result i32
            push_i32_const(out, 0);
            out.push(0x05); // else
            get_local(out, a);
            push_i64_const(out, max);
            get_local(out, b);
            out.push(0x80); // i64.div_u
            out.push(0x56); // i64.gt_u
            out.push(0x0b); // end
        }
        OverflowOp::Mul => {
            // 64-bit signed (isize): if a == 0 no overflow, else overflow iff the
            // wrapped product divided by `a` does not recover `b` — plus the
            // `-1 * MIN` case the wrapping division cannot distinguish. The guarded
            // signed division avoids the `MIN / -1` trap; `a != 0` avoids div-by-0.
            get_local(out, a);
            out.push(0x50); // i64.eqz
            out.push(0x04); // if
            out.push(0x7f); // result i32
            push_i32_const(out, 0);
            out.push(0x05); // else
            // (a == -1) & (b == MIN)
            get_local(out, a);
            push_i64_const(out, -1);
            out.push(0x51); // i64.eq
            get_local(out, b);
            push_i64_const(out, min);
            out.push(0x51); // i64.eq
            out.push(0x71); // i32.and
            // (guarded_div_s(wrapped, a) != b)
            get_local(out, wrapped);
            get_local(out, a);
            emit_i64_signed_div_guarded(ctx, out);
            get_local(out, b);
            out.push(0x52); // i64.ne
            out.push(0x72); // i32.or
            out.push(0x0b); // end
        }
    }
}

/// Push the `i64` saturation target for `a <op> b` (the bound the true result
/// crosses on overflow). Read only when the overflow flag is set.
fn push_wasm_saturation_target(
    op: OverflowOp,
    kind: IntKind,
    a: u32,
    b: u32,
    wrapped: u32,
    out: &mut Vec<u8>,
) {
    let (min_i128, max_i128) = kind.range_i128();
    let min = min_i128 as i64;
    let max = max_i128 as i64;
    let unsigned = kind.is_unsigned();
    match (op, unsigned) {
        // Unsigned subtraction underflows to the minimum (0); unsigned add/mul
        // saturate up to the maximum.
        (OverflowOp::Sub, true) => push_i64_const(out, min),
        (_, true) => push_i64_const(out, max),
        // Signed multiply: the true product's sign is sign(a) ^ sign(b); a negative
        // product saturates to MIN, else MAX. `select(MIN, MAX, (a ^ b) < 0)`.
        (OverflowOp::Mul, false) => {
            push_i64_const(out, min);
            push_i64_const(out, max);
            get_local(out, a);
            get_local(out, b);
            out.push(0x85); // i64.xor
            push_i64_const(out, 0);
            out.push(0x53); // i64.lt_s
            out.push(0x1b); // select
        }
        // Signed add/sub: a signed overflow flips the wrapped result's sign, so a
        // negative wrapped value means positive overflow (target MAX), else MIN.
        // `select(MAX, MIN, wrapped < 0)`.
        (_, false) => {
            push_i64_const(out, max);
            push_i64_const(out, min);
            get_local(out, wrapped);
            push_i64_const(out, 0);
            out.push(0x53); // i64.lt_s
            out.push(0x1b); // select
        }
    }
}

/// Lower an overflow-aware arithmetic builtin. `wrapping_*` leaves the wrapped
/// `T` value on the stack; `saturating_*` the clamped `T`; `checked_*` a fresh
/// `option<T>` record pointer (`some(result)`/`none`).
#[allow(clippy::too_many_arguments)]
fn lower_wasm_overflow(
    ctx: &mut LowerCtx,
    op: OverflowOp,
    mode: OverflowMode,
    kind: IntKind,
    result_ty: &TypeRef,
    left: &IrExpr,
    right: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // Evaluate both operands into `i64` locals so the overflow tests can read them
    // several times.
    lower_expr(ctx, left, out)?;
    let a = ctx.add_local(WasmValType::I64);
    set_local(out, a);
    lower_expr(ctx, right, out)?;
    let b = ctx.add_local(WasmValType::I64);
    set_local(out, b);
    // wrapped = normalize(a op b) — the wrapping result and the `some` payload.
    let wrapped = ctx.add_local(WasmValType::I64);
    get_local(out, a);
    get_local(out, b);
    out.push(op.wasm_opcode());
    emit_normalize_i64(kind, out);
    set_local(out, wrapped);

    match mode {
        OverflowMode::Wrapping => {
            get_local(out, wrapped);
            Ok(())
        }
        OverflowMode::Saturating => {
            let ovf = ctx.add_local(WasmValType::I32);
            push_wasm_overflow_flag(ctx, op, kind, a, b, wrapped, out);
            set_local(out, ovf);
            // result = ovf ? target : wrapped.
            push_wasm_saturation_target(op, kind, a, b, wrapped, out);
            get_local(out, wrapped);
            get_local(out, ovf);
            out.push(0x1b); // select
            Ok(())
        }
        OverflowMode::Checked => {
            let ovf = ctx.add_local(WasmValType::I32);
            push_wasm_overflow_flag(ctx, op, kind, a, b, wrapped, out);
            set_local(out, ovf);
            // Build the `option<T>` record: tag = ovf ? none : some, payload = wrapped.
            let inner = result_ty.option_element().ok_or_else(|| {
                format!(
                    "checked_* result type `{}` is not an `option<T>` enum",
                    result_ty.name
                )
            })?;
            let slot_ty = slot_val_type(&inner, ctx.structs, ctx.enums).ok_or_else(|| {
                format!("checked_* option payload `{}` is unsupported", inner.name)
            })?;
            let layout = build_layout(vec![
                ("some".to_string(), vec![inner]),
                ("none".to_string(), Vec::new()),
            ]);
            let some_tag = layout
                .tag_of("some")
                .ok_or_else(|| "checked_* option layout missing `some` variant".to_string())?;
            let none_tag = layout
                .tag_of("none")
                .ok_or_else(|| "checked_* option layout missing `none` variant".to_string())?;
            let opt = alloc_bytes(ctx, layout.size_bytes(), out);
            // tag = select(none, some, ovf).
            get_local(out, opt);
            push_i32_const(out, none_tag as i32);
            push_i32_const(out, some_tag as i32);
            get_local(out, ovf);
            out.push(0x1b); // select
            emit_store_at(WasmValType::I32, 0, out);
            // payload slot = wrapped.
            get_local(out, opt);
            get_local(out, wrapped);
            emit_store_at(slot_ty, ENUM_PAYLOAD_BASE, out);
            get_local(out, opt);
            Ok(())
        }
    }
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
    if let Some(elem) = supported_list_element(ty, ctx.structs, ctx.enums) {
        return emit_list_deep_copy(ctx, &elem, out);
    }
    if let Some((_, value)) = supported_map_kv(ty, ctx.structs, ctx.enums) {
        return emit_map_deep_copy(ctx, &value, out);
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

/// Deep-copy an enum: the source pointer is on the stack. The `[tag][payload slots]`
/// record is `size_bytes()` bytes (one padded tag slot plus `slot_count` payload
/// slots, every one 8-byte aligned).
///
/// A scalar or `string` payload is copied word-for-word (a `string` is immutable, so
/// sharing its pointer IS its value-semantic copy). When EVERY variant payload is a
/// scalar or `string`, the whole record is a flat `i64` word copy — an exact deep
/// copy with no tag dispatch.
///
/// When some variant has a MUTABLE-aggregate payload (`struct` / nested `list`, e.g.
/// `option<struct>`), the flat copy is done first (so the tag and every scalar/
/// string slot land), then the record's tag is loaded and, for the matching
/// variant, each mutable-aggregate payload slot is re-copied as an independent
/// [`emit_deep_copy`] — so mutating the copy's payload is never observable through
/// the original. This mirrors the interpreters' recursive `Value::clone` on an enum.
fn emit_deep_copy_enum(
    ctx: &mut LowerCtx,
    layout: &EnumLayout,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let src = ctx.add_local(WasmValType::I32);
    set_local(out, src);
    let dst = alloc_bytes(ctx, layout.size_bytes(), out);
    // Flat word copy of the whole record (tag + every payload slot). For a scalar/
    // string-only enum this is already the exact deep copy.
    let words = layout.size_bytes() / SLOT_SIZE;
    for word in 0..words {
        let offset = word * SLOT_SIZE;
        get_local(out, dst);
        get_local(out, src);
        emit_load_at(WasmValType::I64, offset, out);
        emit_store_at(WasmValType::I64, offset, out);
    }
    // If any variant carries a mutable-aggregate payload, re-deep-copy those slots
    // for the record's actual variant (branch on the loaded tag).
    let has_mutable_payload = layout.variants.iter().any(|(_, payload)| {
        payload
            .iter()
            .any(|p| is_mutable_aggregate(p, ctx.structs, ctx.enums))
    });
    if has_mutable_payload {
        let tag = ctx.add_local(WasmValType::I32);
        get_local(out, dst);
        emit_load_at(WasmValType::I32, 0, out);
        set_local(out, tag);
        let variants = layout.variants.clone();
        for (variant_tag, (_, payload)) in variants.iter().enumerate() {
            let mutable_slots: Vec<(usize, TypeRef)> = payload
                .iter()
                .enumerate()
                .filter(|(_, p)| is_mutable_aggregate(p, ctx.structs, ctx.enums))
                .map(|(i, p)| (i, p.clone()))
                .collect();
            if mutable_slots.is_empty() {
                continue;
            }
            // if tag == variant_tag { deep-copy each mutable payload slot }
            get_local(out, tag);
            out.push(0x41); // i32.const variant_tag
            write_sleb(out, variant_tag as i64);
            out.push(0x46); // i32.eq
            out.push(0x04); // if -> void
            out.push(0x40);
            for (slot, payload_ty) in &mutable_slots {
                let offset = ENUM_PAYLOAD_BASE + *slot as i32 * SLOT_SIZE;
                // dst_addr = dst + offset; load the payload pointer, deep-copy it,
                // store the fresh independent pointer back into dst's slot.
                get_local(out, dst);
                out.push(0x41); // i32.const offset
                write_sleb(out, offset as i64);
                out.push(0x6a); // i32.add -> dst slot addr
                get_local(out, dst);
                emit_load_at(WasmValType::I32, offset, out);
                emit_deep_copy(ctx, payload_ty, out)?;
                emit_store(WasmValType::I32, out);
            }
            out.push(0x0b); // end if
        }
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
/// and `dst` are `i32` locals holding list base pointers. `elem_ty` is the list's
/// element IR type, which decides the per-element copy:
///
/// - a **scalar or `string`** element is copied word-for-word by a `SLOT_SIZE`-
///   aligned `i64` load/store — a `string` is immutable, so sharing its pointer IS
///   its value-semantic copy;
/// - a **mutable-aggregate** element (a `struct` or a nested `list`) is
///   DEEP-COPIED: the element pointer is loaded from `src`, `emit_deep_copy`
///   produces a fresh independent record, and that fresh pointer is stored into
///   `dst`. This mirrors [`emit_copy_element_at`] (the array path) and the
///   interpreters' recursive `Value::clone`, so mutating an element of one list
///   copy is never observable through another.
fn emit_list_copy_elems(
    ctx: &mut LowerCtx,
    elem_ty: &TypeRef,
    src: u32,
    dst: u32,
    count: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let deep = is_mutable_aggregate(elem_ty, ctx.structs, ctx.enums);
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
    if deep {
        // dst_addr = dst + off; load the element pointer from src + off, deep-copy
        // it into a fresh record, store that fresh pointer into dst.
        get_local(out, dst);
        get_local(out, off);
        out.push(0x6a); // i32.add -> dst addr
        get_local(out, src);
        get_local(out, off);
        out.push(0x6a); // i32.add -> src addr
        emit_load(WasmValType::I32, out);
        emit_deep_copy(ctx, elem_ty, out)?;
        emit_store(WasmValType::I32, out);
    } else {
        // memory[dst + off] = memory[src + off] (one 8-byte word: scalar or a
        // shared immutable `string` pointer).
        get_local(out, dst);
        get_local(out, off);
        out.push(0x6a); // i32.add -> dst addr
        get_local(out, src);
        get_local(out, off);
        out.push(0x6a); // i32.add -> src addr
        emit_load(WasmValType::I64, out);
        emit_store(WasmValType::I64, out);
    }
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
    Ok(())
}

/// Deep-copy the growable `list<T>` whose pointer is on the stack, leaving a fresh
/// independent `[len][cap][slots]` block's pointer on the stack. The copy keeps the
/// source's `len` and `cap` and duplicates its `len` element slots (deep-copying a
/// mutable-aggregate element per [`emit_list_copy_elems`]). This is the WASM
/// realization of the interpreters' recursive `Value::clone` on a list: mutating
/// the copy (or the original), or an element of either, is never observable through
/// the other pointer. `elem_ty` is the element IR type.
fn emit_list_deep_copy(
    ctx: &mut LowerCtx,
    elem_ty: &TypeRef,
    out: &mut Vec<u8>,
) -> Result<(), String> {
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
    emit_list_copy_elems(ctx, elem_ty, src, dst, len, out)?;
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
    let elem_ty = supported_list_element(&list.ty, ctx.structs, ctx.enums).ok_or_else(|| {
        format!(
            "push expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let slot_ty = collection_slot_type(&elem_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element type `{}` is unsupported", elem_ty.name))?;
    let deep_elem = is_mutable_aggregate(&elem_ty, ctx.structs, ctx.enums);
    // Deep-copy the source list into a fresh, independent block (value semantics).
    lower_expr(ctx, list, out)?;
    emit_list_deep_copy(ctx, &elem_ty, out)?;
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
    emit_list_grow(ctx, &elem_ty, lst, len, cap, out)?;
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
    if deep_elem {
        // A mutable-aggregate element is stored as an INDEPENDENT deep copy so a
        // later mutation of the source value never leaks into the list, matching the
        // interpreters (an argument `Value` is cloned before it is pushed).
        emit_deep_copy(ctx, &elem_ty, out)?;
    }
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
/// a fresh block, copy the `len` live elements (deep-copying a mutable-aggregate
/// element per [`emit_list_copy_elems`], with `elem_ty` the element type), write the
/// new `cap` and preserved `len`, and update `lst` to the new pointer. `len` and
/// `cap` locals are refreshed so the caller sees the post-grow capacity; the old
/// block is orphaned. The elements copied here already came from a fresh deep copy
/// of the source list, so the grow reuses their independent records without a second
/// deep recursion — but the copy still routes through `emit_list_copy_elems`, whose
/// per-element deep-copy keeps the grown block's elements independent of the
/// pre-grow block that is about to be orphaned.
fn emit_list_grow(
    ctx: &mut LowerCtx,
    elem_ty: &TypeRef,
    lst: u32,
    len: u32,
    cap: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
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
    emit_list_copy_elems(ctx, elem_ty, lst, dst, len, out)?;
    // lst = dst; cap = new_cap (len is unchanged)
    get_local(out, dst);
    set_local(out, lst);
    get_local(out, new_cap);
    set_local(out, cap);
    Ok(())
}

/// Lower `get(l, i) -> T`: load element `i` from `l + LIST_DATA_OFF + i*SLOT_SIZE`.
/// The interpreters bounds-check and raise `L0413`; the WASM backend relies on
/// linear-memory trapping for a truly out-of-range index, so in-bounds reads match
/// the interpreters exactly and an OOB read traps (a consistent, documented
/// behavior) instead of returning a poisoned value.
///
/// The interpreters return `values[i].clone()` — a DEEP CLONE of the element — so a
/// mutable-aggregate element (`struct`/nested `list`) is loaded and then
/// `emit_deep_copy`'d into a fresh independent record before it leaves `get`.
/// Mutating the returned copy (or pushing it into another list and mutating that)
/// therefore never affects the original list's element, exactly like the
/// interpreters. A scalar or immutable `string` element needs no copy (a `string`
/// is shared, which IS its value-semantic clone).
fn lower_list_get(
    ctx: &mut LowerCtx,
    list: &IrExpr,
    index: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = supported_list_element(&list.ty, ctx.structs, ctx.enums).ok_or_else(|| {
        format!(
            "get expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let slot_ty = collection_slot_type(&elem_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element type `{}` is unsupported", elem_ty.name))?;
    lower_expr(ctx, list, out)?; // base pointer
    emit_list_elem_offset(ctx, index, out)?; // += LIST_DATA_OFF + index * SLOT_SIZE
    emit_load(slot_ty, out);
    if is_mutable_aggregate(&elem_ty, ctx.structs, ctx.enums) {
        // Return an independent deep copy (the interpreters' `values[i].clone()`).
        emit_deep_copy(ctx, &elem_ty, out)?;
    }
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
    let elem_ty = supported_list_element(&list.ty, ctx.structs, ctx.enums).ok_or_else(|| {
        format!(
            "set expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let slot_ty = collection_slot_type(&elem_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element type `{}` is unsupported", elem_ty.name))?;
    let deep_elem = is_mutable_aggregate(&elem_ty, ctx.structs, ctx.enums);
    lower_expr(ctx, list, out)?;
    emit_list_deep_copy(ctx, &elem_ty, out)?;
    let lst = ctx.add_local(WasmValType::I32);
    set_local(out, lst);
    // element slot address in the copy
    get_local(out, lst);
    emit_list_elem_offset(ctx, index, out)?;
    lower_expr(ctx, value, out)?;
    if deep_elem {
        // Store an independent deep copy of the replacement so a later mutation of
        // the source value never leaks into the list (interpreter clone semantics).
        emit_deep_copy(ctx, &elem_ty, out)?;
    }
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
    let elem_ty = supported_list_element(&list.ty, ctx.structs, ctx.enums).ok_or_else(|| {
        format!(
            "pop expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    lower_expr(ctx, list, out)?;
    emit_list_deep_copy(ctx, &elem_ty, out)?;
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

// -- Growable map codegen ----------------------------------------------------

/// Push `MAP_DATA_OFF + cap * MAP_ENTRY_SIZE` (the byte size of a map backing
/// block with `cap` entry records) onto the stack, given an `i32` local holding
/// `cap`.
fn emit_map_block_size(cap_local: u32, out: &mut Vec<u8>) {
    get_local(out, cap_local);
    out.push(0x41); // i32.const MAP_ENTRY_SIZE
    write_sleb(out, MAP_ENTRY_SIZE as i64);
    out.push(0x6c); // i32.mul  -> cap * MAP_ENTRY_SIZE
    out.push(0x41); // i32.const MAP_DATA_OFF
    write_sleb(out, MAP_DATA_OFF as i64);
    out.push(0x6a); // i32.add  -> MAP_DATA_OFF + cap * MAP_ENTRY_SIZE
}

/// Push `base + MAP_DATA_OFF + entry * MAP_ENTRY_SIZE` (the address of entry
/// record `entry`) onto the stack. `base` and `entry` are `i32` locals.
fn emit_map_entry_addr(base: u32, entry: u32, out: &mut Vec<u8>) {
    get_local(out, base);
    get_local(out, entry);
    out.push(0x41); // i32.const MAP_ENTRY_SIZE
    write_sleb(out, MAP_ENTRY_SIZE as i64);
    out.push(0x6c); // i32.mul -> entry * MAP_ENTRY_SIZE
    out.push(0x41); // i32.const MAP_DATA_OFF
    write_sleb(out, MAP_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    out.push(0x6a); // i32.add -> base + MAP_DATA_OFF + entry * MAP_ENTRY_SIZE
}

/// Copy the first `count` entry records (each `MAP_ENTRY_SIZE` = two `SLOT_SIZE`
/// words) from `src` to `dst` map backing blocks in a runtime loop. `count`, `src`,
/// and `dst` are `i32` locals; `value_ty` is the map's value IR type. A map KEY is
/// always a scalar or an immutable `string` pointer (a `string` is compared by
/// content but shared on copy), so the key word is copied flat. The VALUE word is:
///
/// - copied FLAT for a scalar or immutable `string` value — sharing the pointer IS
///   the value-semantic copy;
/// - DEEP-COPIED for a mutable-aggregate value (a `struct`, i.e. `map<K, struct>`):
///   the value pointer is loaded from `src`, `emit_deep_copy` produces a fresh
///   independent record, and that fresh pointer is stored into `dst` — matching the
///   interpreters' recursive `Value::clone` on a map, so mutating a value of one map
///   copy is never observable through another.
fn emit_map_copy_entries(
    ctx: &mut LowerCtx,
    value_ty: &TypeRef,
    src: u32,
    dst: u32,
    count: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let deep_value = is_mutable_aggregate(value_ty, ctx.structs, ctx.enums);
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
    // off = MAP_DATA_OFF + i * MAP_ENTRY_SIZE
    let off = ctx.add_local(WasmValType::I32);
    get_local(out, i);
    out.push(0x41); // i32.const MAP_ENTRY_SIZE
    write_sleb(out, MAP_ENTRY_SIZE as i64);
    out.push(0x6c); // i32.mul
    out.push(0x41); // i32.const MAP_DATA_OFF
    write_sleb(out, MAP_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    set_local(out, off);
    // Copy the key word flat (a scalar or a shared immutable `string` pointer).
    get_local(out, dst);
    get_local(out, off);
    out.push(0x6a); // i32.add -> dst key addr
    get_local(out, src);
    get_local(out, off);
    out.push(0x6a); // i32.add -> src key addr
    emit_load_at(WasmValType::I64, 0, out);
    emit_store_at(WasmValType::I64, 0, out);
    // Copy the value word at MAP_VALUE_OFF: flat for a scalar/string, deep for a
    // mutable aggregate.
    if deep_value {
        get_local(out, dst);
        get_local(out, off);
        out.push(0x6a); // i32.add -> dst entry addr
        get_local(out, src);
        get_local(out, off);
        out.push(0x6a); // i32.add -> src entry addr
        emit_load_at(WasmValType::I32, MAP_VALUE_OFF, out);
        emit_deep_copy(ctx, value_ty, out)?;
        emit_store_at(WasmValType::I32, MAP_VALUE_OFF, out);
    } else {
        get_local(out, dst);
        get_local(out, off);
        out.push(0x6a); // i32.add -> dst entry addr
        get_local(out, src);
        get_local(out, off);
        out.push(0x6a); // i32.add -> src entry addr
        emit_load_at(WasmValType::I64, MAP_VALUE_OFF, out);
        emit_store_at(WasmValType::I64, MAP_VALUE_OFF, out);
    }
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
    Ok(())
}

/// Deep-copy the growable `map<K, V>` whose pointer is on the stack, leaving a
/// fresh independent `[len][cap][entries]` block's pointer on the stack. The copy
/// keeps the source's `len` and `cap` and duplicates its `len` live entry records
/// (each two words; a mutable-aggregate value is deep-copied per
/// [`emit_map_copy_entries`], with `value_ty` the value type). This is the WASM
/// realization of the interpreters' recursive clone on a map: mutating the copy (or
/// the original), or a value of either, is never observable through the other.
fn emit_map_deep_copy(
    ctx: &mut LowerCtx,
    value_ty: &TypeRef,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let src = ctx.add_local(WasmValType::I32);
    set_local(out, src);
    // len = load [src + MAP_LEN_OFF], cap = load [src + MAP_CAP_OFF]
    let len = ctx.add_local(WasmValType::I32);
    get_local(out, src);
    emit_load_at(WasmValType::I32, MAP_LEN_OFF, out);
    set_local(out, len);
    let cap = ctx.add_local(WasmValType::I32);
    get_local(out, src);
    emit_load_at(WasmValType::I32, MAP_CAP_OFF, out);
    set_local(out, cap);
    // dst = __alloc(MAP_DATA_OFF + cap * MAP_ENTRY_SIZE)
    emit_map_block_size(cap, out);
    let dst = alloc_runtime(ctx, out);
    // dst.len = len; dst.cap = cap
    get_local(out, dst);
    get_local(out, len);
    emit_store_at(WasmValType::I32, MAP_LEN_OFF, out);
    get_local(out, dst);
    get_local(out, cap);
    emit_store_at(WasmValType::I32, MAP_CAP_OFF, out);
    // copy the `len` live entry records
    emit_map_copy_entries(ctx, value_ty, src, dst, len, out)?;
    get_local(out, dst);
    Ok(())
}

/// Lower `map_new() -> map<K, V>`: `__alloc` an empty
/// `[len=0][cap=MAP_INITIAL_CAP][entries...]` block and leave its pointer on the
/// stack. A small initial capacity means the first few `map_set`s do not realloc.
fn lower_map_new(ctx: &mut LowerCtx, args: &[IrExpr], out: &mut Vec<u8>) -> Result<(), String> {
    if !args.is_empty() {
        return Err(format!("map_new expects 0 arguments, got {}", args.len()));
    }
    let ptr = alloc_bytes(ctx, MAP_DATA_OFF + MAP_INITIAL_CAP * MAP_ENTRY_SIZE, out);
    get_local(out, ptr);
    out.push(0x41); // i32.const 0 (len)
    write_sleb(out, 0);
    emit_store_at(WasmValType::I32, MAP_LEN_OFF, out);
    get_local(out, ptr);
    out.push(0x41); // i32.const MAP_INITIAL_CAP (cap)
    write_sleb(out, MAP_INITIAL_CAP as i64);
    emit_store_at(WasmValType::I32, MAP_CAP_OFF, out);
    get_local(out, ptr);
    Ok(())
}

/// Emit the equality opcode comparing two SCALAR key values (already pushed) of
/// WASM slot type `key_ty`. Scalar keys use `i64.eq` (`i64`/fixed-width), `i32.eq`
/// (`bool`/`char`/`byte`), or the ordered `f*.eq` (floats), matching how the
/// interpreters compare `Value` keys by content. A `string` key is NOT a scalar
/// and is compared by content via [`emit_string_eq`], never this opcode.
fn emit_key_eq(key_ty: WasmValType, out: &mut Vec<u8>) {
    match key_ty {
        WasmValType::I32 => out.push(0x46), // i32.eq
        WasmValType::I64 => out.push(0x51), // i64.eq
        WasmValType::F32 => out.push(0x5b), // f32.eq
        WasmValType::F64 => out.push(0x61), // f64.eq
    }
}

/// Emit a CONTENT equality test for two `string` pointers `a` and `b` (both `i32`
/// locals pointing at `[char_len: i32][byte_len: i32][utf8 bytes]` records),
/// leaving an `i32` boolean (1 if the strings hold identical bytes, else 0) on the
/// stack. This matches the interpreters' `Value::String` equality: two DISTINCT
/// string objects with the same bytes compare equal (content, not pointer
/// identity). The test first compares the `byte_len` headers; if they differ the
/// result is 0 without touching the bytes. If they match, it walks the UTF-8 bytes
/// front-to-back, returning 0 on the first differing byte and 1 once every byte
/// matched (a zero-length string trivially matches). A pointer-identity fast path
/// (`a == b`) short-circuits when the same record is passed for both sides.
fn emit_string_eq(ctx: &mut LowerCtx, a: u32, b: u32, out: &mut Vec<u8>) -> u32 {
    let result = ctx.add_local(WasmValType::I32);
    // Fast path: identical pointers are the same string, so equal.
    get_local(out, a);
    get_local(out, b);
    out.push(0x46); // i32.eq
    out.push(0x04); // if -> void
    out.push(0x40);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    set_local(out, result);
    out.push(0x05); // else
    // Compare byte-length headers.
    let byte_len = ctx.add_local(WasmValType::I32);
    get_local(out, a);
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    set_local(out, byte_len);
    get_local(out, byte_len);
    get_local(out, b);
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    out.push(0x47); // i32.ne
    out.push(0x04); // if -> void  (byte lengths differ => not equal)
    out.push(0x40);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, result);
    out.push(0x05); // else  (equal lengths: walk the bytes)
    // Assume equal until a byte mismatches; scan i in [0, byte_len).
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    set_local(out, result);
    let i = ctx.add_local(WasmValType::I32);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, i);
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // break when i >= byte_len
    get_local(out, i);
    get_local(out, byte_len);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of the block)
    write_uleb(out, 1);
    // if load8_u(a + STR_DATA_OFF + i) != load8_u(b + STR_DATA_OFF + i) { result = 0; break }
    get_local(out, a);
    get_local(out, i);
    out.push(0x6a); // i32.add
    out.push(0x2d); // i32.load8_u
    write_uleb(out, 0); // align 0
    write_uleb(out, STR_DATA_OFF as u64);
    get_local(out, b);
    get_local(out, i);
    out.push(0x6a); // i32.add
    out.push(0x2d); // i32.load8_u
    write_uleb(out, 0); // align 0
    write_uleb(out, STR_DATA_OFF as u64);
    out.push(0x47); // i32.ne
    out.push(0x04); // if -> void
    out.push(0x40);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, result);
    out.push(0x0c); // br 2 (out of the block)
    write_uleb(out, 2);
    out.push(0x0b); // end if
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
    out.push(0x0b); // end if (byte-length mismatch)
    out.push(0x0b); // end if (pointer fast path)
    result
}

/// Linear-scan the map in `map_local` for the entry whose key equals the value in
/// `key_local` (slot type `key_ty`), returning a fresh `i32` local holding the
/// matching entry index, or the map's `len` if no key matched (the "found index
/// else len" convention: `map_set` appends at `len`, `map_get`/`map_has` treat
/// `index == len` as absent). The scan visits entries front-to-back so the FIRST
/// matching key wins, mirroring the interpreters' insertion-ordered association
/// list. When `key_is_string`, the key slot holds an `i32` string POINTER and keys
/// are compared by CONTENT via [`emit_string_eq`] (decoded bytes, so two distinct
/// string objects with equal bytes match) instead of an integer slot compare;
/// otherwise the scalar `key_ty` compare of [`emit_key_eq`] is used. `len` is
/// loaded into a caller-visible local so callers can reuse it.
fn emit_map_find(
    ctx: &mut LowerCtx,
    map_local: u32,
    key_local: u32,
    key_ty: WasmValType,
    key_is_string: bool,
    len_local: u32,
    out: &mut Vec<u8>,
) -> u32 {
    // len = load [map + MAP_LEN_OFF]
    get_local(out, map_local);
    emit_load_at(WasmValType::I32, MAP_LEN_OFF, out);
    set_local(out, len_local);
    // found = len (sentinel: "not found")
    let found = ctx.add_local(WasmValType::I32);
    get_local(out, len_local);
    set_local(out, found);
    // i = 0
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
    get_local(out, len_local);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of the block)
    write_uleb(out, 1);
    // if key(entry i) == key_local { found = i; break }
    if key_is_string {
        // Load the entry's stored string pointer, then compare by content.
        let entry_key = ctx.add_local(WasmValType::I32);
        emit_map_entry_addr(map_local, i, out); // entry addr
        emit_load(WasmValType::I32, out); // load key slot (string pointer at offset 0)
        set_local(out, entry_key);
        let eq = emit_string_eq(ctx, entry_key, key_local, out);
        get_local(out, eq);
    } else {
        emit_map_entry_addr(map_local, i, out); // entry addr
        emit_load(key_ty, out); // load key slot (offset 0 within the entry)
        get_local(out, key_local);
        emit_key_eq(key_ty, out);
    }
    out.push(0x04); // if
    out.push(0x40); // void
    get_local(out, i);
    set_local(out, found);
    out.push(0x0c); // br 2 (out of the block, skipping the increment)
    write_uleb(out, 2);
    out.push(0x0b); // end if
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
    found
}

/// Lower `map_set(m, k, v) -> map<K, V>` (value-semantic insert/update): deep-copy
/// `m`, scan the copy for `k`; if found, overwrite that entry's value slot in
/// place (preserving the entry's position/order); otherwise grow when full (double
/// the capacity, or seed `MAP_INITIAL_CAP` from an empty map) and append a new
/// `(k, v)` entry, bumping `len`. Leaves the fresh map pointer on the stack.
/// Because `map_set` always returns a NEW map, `m = map_set(m, k, v)` matches the
/// interpreters' clone-then-mutate on the insertion-ordered association list.
fn lower_map_set(
    ctx: &mut LowerCtx,
    map: &IrExpr,
    key: &IrExpr,
    value: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (key_ty, value_ty) =
        supported_map_kv(&map.ty, ctx.structs, ctx.enums).ok_or_else(|| {
            format!(
                "map_set expects a scalar/string key and a supported value but got `{}`",
                map.ty.name
            )
        })?;
    let key_slot = scalar_or_string_slot_type(&key_ty)
        .ok_or_else(|| format!("map key type `{}` is unsupported", key_ty.name))?;
    let key_is_string = key_ty.name == "string";
    let value_slot = collection_slot_type(&value_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("map value type `{}` is unsupported", value_ty.name))?;
    let deep_value = is_mutable_aggregate(&value_ty, ctx.structs, ctx.enums);

    // Deep-copy the source map into a fresh, independent block (value semantics).
    lower_expr(ctx, map, out)?;
    emit_map_deep_copy(ctx, &value_ty, out)?;
    let mp = ctx.add_local(WasmValType::I32);
    set_local(out, mp);
    // Evaluate the key once into a local for the scan and (on append) the store.
    lower_expr(ctx, key, out)?;
    let key_local = ctx.add_local(key_slot);
    set_local(out, key_local);

    // Scan for the key; `found == len` means "not present".
    let len = ctx.add_local(WasmValType::I32);
    let found = emit_map_find(ctx, mp, key_local, key_slot, key_is_string, len, out);

    // if found == len { append } else { overwrite value in place }
    get_local(out, found);
    get_local(out, len);
    out.push(0x46); // i32.eq
    out.push(0x04); // if
    out.push(0x40); // void
    // --- append branch ---
    // Grow if len == cap.
    let cap = ctx.add_local(WasmValType::I32);
    get_local(out, mp);
    emit_load_at(WasmValType::I32, MAP_CAP_OFF, out);
    set_local(out, cap);
    get_local(out, len);
    get_local(out, cap);
    out.push(0x4e); // i32.ge_s
    out.push(0x04); // if
    out.push(0x40); // void
    emit_map_grow(ctx, &value_ty, mp, len, cap, out)?;
    out.push(0x0b); // end if (grow)
    // entry addr of index `len`
    emit_map_entry_addr(mp, len, out);
    let entry_addr = ctx.add_local(WasmValType::I32);
    set_local(out, entry_addr);
    // store key at entry + 0
    get_local(out, entry_addr);
    get_local(out, key_local);
    emit_store_at(key_slot, 0, out);
    // store value at entry + MAP_VALUE_OFF (an independent deep copy for a
    // mutable-aggregate value, matching the interpreters' clone-before-insert).
    get_local(out, entry_addr);
    lower_expr(ctx, value, out)?;
    if deep_value {
        emit_deep_copy(ctx, &value_ty, out)?;
    }
    emit_store_at(value_slot, MAP_VALUE_OFF, out);
    // len += 1
    get_local(out, mp);
    get_local(out, len);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    emit_store_at(WasmValType::I32, MAP_LEN_OFF, out);
    out.push(0x05); // else
    // --- overwrite branch: store value into entry `found`'s value slot ---
    emit_map_entry_addr(mp, found, out);
    lower_expr(ctx, value, out)?;
    if deep_value {
        emit_deep_copy(ctx, &value_ty, out)?;
    }
    emit_store_at(value_slot, MAP_VALUE_OFF, out);
    out.push(0x0b); // end if (append/overwrite)

    get_local(out, mp);
    Ok(())
}

/// Grow the map in `mp` (an `i32` local) so it has room past `len` (== `cap`):
/// compute `new_cap = if cap == 0 { MAP_INITIAL_CAP } else { cap * 2 }`, `__alloc`
/// a fresh block, copy the `len` live entries (deep-copying a mutable-aggregate
/// value per [`emit_map_copy_entries`], with `value_ty` the value type), write the
/// new `cap` and preserved `len`, and update `mp` to the new pointer. `len` and
/// `cap` locals are refreshed so the caller sees the post-grow capacity; the old
/// block is orphaned.
fn emit_map_grow(
    ctx: &mut LowerCtx,
    value_ty: &TypeRef,
    mp: u32,
    len: u32,
    cap: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // new_cap = cap == 0 ? MAP_INITIAL_CAP : cap * 2
    let new_cap = ctx.add_local(WasmValType::I32);
    get_local(out, cap);
    out.push(0x45); // i32.eqz
    out.push(0x04); // if -> i32
    out.push(0x7f); // result i32
    out.push(0x41); // i32.const MAP_INITIAL_CAP
    write_sleb(out, MAP_INITIAL_CAP as i64);
    out.push(0x05); // else
    get_local(out, cap);
    out.push(0x41); // i32.const 2
    write_sleb(out, 2);
    out.push(0x6c); // i32.mul
    out.push(0x0b); // end if
    set_local(out, new_cap);
    // dst = __alloc(MAP_DATA_OFF + new_cap * MAP_ENTRY_SIZE)
    emit_map_block_size(new_cap, out);
    let dst = alloc_runtime(ctx, out);
    // dst.len = len; dst.cap = new_cap
    get_local(out, dst);
    get_local(out, len);
    emit_store_at(WasmValType::I32, MAP_LEN_OFF, out);
    get_local(out, dst);
    get_local(out, new_cap);
    emit_store_at(WasmValType::I32, MAP_CAP_OFF, out);
    // copy the `len` live entries from the old block (mp) to dst
    emit_map_copy_entries(ctx, value_ty, mp, dst, len, out)?;
    // mp = dst; cap = new_cap (len unchanged)
    get_local(out, dst);
    set_local(out, mp);
    get_local(out, new_cap);
    set_local(out, cap);
    Ok(())
}

/// Lower `map_get(m, k) -> option<V>`: deep-copy `m` is NOT needed (read-only),
/// scan for `k`, and construct `some(v)` (loading the found entry's value slot)
/// or `none`, reusing the enum/option linear-memory layout. `result_ty` is the
/// call's `option<V>` type, from which the `some`/`none` [`EnumLayout`] is built.
fn lower_map_get(
    ctx: &mut LowerCtx,
    result_ty: &TypeRef,
    map: &IrExpr,
    key: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (key_ty, value_ty) =
        supported_map_kv(&map.ty, ctx.structs, ctx.enums).ok_or_else(|| {
            format!(
                "map_get expects a scalar/string key and a supported value but got `{}`",
                map.ty.name
            )
        })?;
    let key_slot = scalar_or_string_slot_type(&key_ty)
        .ok_or_else(|| format!("map key type `{}` is unsupported", key_ty.name))?;
    let key_is_string = key_ty.name == "string";
    let value_slot = collection_slot_type(&value_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("map value type `{}` is unsupported", value_ty.name))?;
    let deep_value = is_mutable_aggregate(&value_ty, ctx.structs, ctx.enums);
    // The result `option<V>` enum layout: variant `some(V)` is tag 0, `none` tag 1.
    // Built directly (not via `enum_layout`) so a mutable-aggregate value type
    // (`map<K, struct>` -> `option<struct>`) is laid out — the `some` payload is one
    // `i32` pointer slot, deep-copied above so the option owns an independent value.
    // The produced option is consumed locally (matched to extract the value), so its
    // enum-level deep copy across a boundary is not exercised by this construction.
    let inner = result_ty.option_element().ok_or_else(|| {
        format!(
            "map_get result type `{}` is not an `option<V>` enum",
            result_ty.name
        )
    })?;
    if inner.name != value_ty.name {
        return Err(format!(
            "map_get result `option<{}>` does not match value type `{}`",
            inner.name, value_ty.name
        ));
    }
    let layout = build_layout(vec![
        ("some".to_string(), vec![inner]),
        ("none".to_string(), Vec::new()),
    ]);

    // Evaluate the map to a pointer local and the key to a slot local.
    lower_expr(ctx, map, out)?;
    let mp = ctx.add_local(WasmValType::I32);
    set_local(out, mp);
    lower_expr(ctx, key, out)?;
    let key_local = ctx.add_local(key_slot);
    set_local(out, key_local);

    let len = ctx.add_local(WasmValType::I32);
    let found = emit_map_find(ctx, mp, key_local, key_slot, key_is_string, len, out);

    // Allocate the option record once; fill it in either branch. Result pointer is
    // stashed so both branches leave the same pointer on the stack at the `end`.
    let opt = alloc_bytes(ctx, layout.size_bytes(), out);
    // if found == len { none } else { some(value) }
    get_local(out, found);
    get_local(out, len);
    out.push(0x46); // i32.eq
    out.push(0x04); // if
    out.push(0x40); // void
    // --- none branch: tag = 1 ---
    let none_tag = layout
        .tag_of("none")
        .ok_or_else(|| "option layout missing `none` variant".to_string())?;
    get_local(out, opt);
    out.push(0x41); // i32.const none_tag
    write_sleb(out, none_tag as i64);
    emit_store_at(WasmValType::I32, 0, out);
    out.push(0x05); // else
    // --- some branch: tag = 0, payload = entry(found).value ---
    let some_tag = layout
        .tag_of("some")
        .ok_or_else(|| "option layout missing `some` variant".to_string())?;
    get_local(out, opt);
    out.push(0x41); // i32.const some_tag
    write_sleb(out, some_tag as i64);
    emit_store_at(WasmValType::I32, 0, out);
    // opt payload slot = load entry(found).value. A mutable-aggregate value is
    // deep-copied so the option's payload is an INDEPENDENT record — mutating the
    // retrieved value never affects the map's stored value, matching the
    // interpreters' `map_get` returning a clone of the value.
    get_local(out, opt);
    emit_map_entry_addr(mp, found, out);
    emit_load_at(value_slot, MAP_VALUE_OFF, out);
    if deep_value {
        emit_deep_copy(ctx, &value_ty, out)?;
    }
    emit_store_at(value_slot, ENUM_PAYLOAD_BASE, out);
    out.push(0x0b); // end if

    get_local(out, opt);
    Ok(())
}

/// Lower `map_has(m, k) -> bool`: scan for `k`, leaving `found != len` (an `i32`
/// boolean: 1 if present, 0 if absent) on the stack.
fn lower_map_has(
    ctx: &mut LowerCtx,
    map: &IrExpr,
    key: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (key_ty, _value_ty) =
        supported_map_kv(&map.ty, ctx.structs, ctx.enums).ok_or_else(|| {
            format!(
                "map_has expects a scalar/string key and a supported value but got `{}`",
                map.ty.name
            )
        })?;
    let key_slot = scalar_or_string_slot_type(&key_ty)
        .ok_or_else(|| format!("map key type `{}` is unsupported", key_ty.name))?;
    let key_is_string = key_ty.name == "string";

    lower_expr(ctx, map, out)?;
    let mp = ctx.add_local(WasmValType::I32);
    set_local(out, mp);
    lower_expr(ctx, key, out)?;
    let key_local = ctx.add_local(key_slot);
    set_local(out, key_local);

    let len = ctx.add_local(WasmValType::I32);
    let found = emit_map_find(ctx, mp, key_local, key_slot, key_is_string, len, out);
    // result = found != len
    get_local(out, found);
    get_local(out, len);
    out.push(0x47); // i32.ne
    Ok(())
}

/// Lower `map_len(m) -> i64`: load the leading `i32` `len` header and extend to
/// `i64` (the builtin's interpreter result type). Reads offset 0, shared with the
/// string/array/list length header.
fn lower_map_len(ctx: &mut LowerCtx, map: &IrExpr, out: &mut Vec<u8>) -> Result<(), String> {
    supported_map_kv(&map.ty, ctx.structs, ctx.enums).ok_or_else(|| {
        format!(
            "map_len expects a scalar/string key and a supported value but got `{}`",
            map.ty.name
        )
    })?;
    lower_expr(ctx, map, out)?; // map pointer
    emit_load_at(WasmValType::I32, MAP_LEN_OFF, out);
    out.push(0xac); // i64.extend_i32_s
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
        BinaryOp::Divide => 0x7f,    // i64.div_s (traps on 0)
        BinaryOp::Remainder => 0x81, // i64.rem_s (traps on 0; returns 0 for MIN%-1)
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
        // `and`/`or` short-circuit, `%` is integer-only, and the integer bitwise
        // ops are deferred; all are routed away before reaching this opcode table.
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::Remainder
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
        // `and`/`or` short-circuit, `%` is integer-only, and the integer bitwise
        // ops are deferred; all are routed away before reaching this opcode table.
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::Remainder
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
        BinaryOp::Divide => 0x6d,    // i32.div_s
        BinaryOp::Remainder => 0x6f, // i32.rem_s
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
        // `add` is scalar; `tally` returns `map<i64, array<i64>>` (a MUTABLE
        // heap-value map), still outside the WASM value set (strings/structs/
        // arrays/enums, scalar- or string-element `list`s, and scalar-key maps with
        // a scalar or `string` value are supported; a map with a mutable heap value
        // is not), so it is skipped.
        let source = concat!(
            "fn add a i64 b i64 -> i64\n    a + b\n\n",
            "fn tally -> map<i64, array<i64>>\n    map_new()\n",
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
    fn string_literal_record_has_char_and_byte_headers() {
        // A string literal is interned as `[char_len i32][byte_len i32][utf8]`.
        // For a multi-byte literal the two headers differ (char count != byte
        // count), proving the byte length is stored, not derived by assuming ASCII.
        let mut pool = StringPool::new();
        let offset = pool.intern("café"); // 4 chars, 5 UTF-8 bytes (é = 2 bytes)
        assert_eq!(offset, RESERVED_BASE);
        let base = (offset - RESERVED_BASE) as usize;
        let char_len = i32::from_le_bytes(pool.bytes[base..base + 4].try_into().unwrap());
        let byte_len = i32::from_le_bytes(pool.bytes[base + 4..base + 8].try_into().unwrap());
        assert_eq!(char_len, 4, "char-count header is the Unicode scalar count");
        assert_eq!(byte_len, 5, "byte-count header is the UTF-8 byte length");
        assert_eq!(
            &pool.bytes[base + 8..base + 8 + 5],
            "café".as_bytes(),
            "the UTF-8 bytes follow the two headers at STR_DATA_OFF"
        );
    }

    #[test]
    fn string_concat_function_compiles_with_alloc_and_copy_codegen() {
        // A function doing runtime `+` on two `string` values must COMPILE to WASM
        // (not skip to the interpreters). The operands are parameters so the IR
        // constant-folder cannot collapse the concat to a literal, exercising the
        // real runtime alloc-and-copy path.
        let source = "fn join a string b string -> string\n    a + b\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(
            artifact.compiled,
            vec!["join".to_string()],
            "string concat should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());

        let code = section_body(&artifact.bytes, 10).expect("code section");
        // The concat allocates a fresh record via `call __alloc`. `__alloc` is the
        // last internal function: join(0) then __alloc(1), so its shifted WASM index
        // is IMPORT_FUNC_COUNT + 1.
        let alloc_index = IMPORT_FUNC_COUNT as u8 + 1;
        assert!(
            find_subslice(&code, &[0x10, alloc_index]).is_some(),
            "string concat emits a `call __alloc` for the fresh record"
        );
        // The two byte ranges are copied with the bulk-memory `memory.copy`
        // instruction: 0xfc prefix, sub-opcode 0x0a, dest/src memory indices 0, 0.
        assert!(
            find_subslice(&code, &[0xfc, 0x0a, 0x00, 0x00]).is_some(),
            "string concat emits `memory.copy` to join the operand byte ranges"
        );
    }

    #[test]
    fn string_concat_result_len_matches_sum_of_char_counts() {
        // `len(a + b)` on a runtime concat returns the SUM of the operands' char
        // counts, matching the interpreters. The concat's char-count header is
        // written as `char_a + char_b` and `len` reads offset 0, so the whole
        // module compiles and the `len` read (an `i32.load` at offset 0 followed by
        // `i64.extend_i32_s`) appears in the emitted code.
        let source = concat!(
            "fn cat a string b string -> string\n    a + b\n\n",
            "fn probe -> i64\n",
            "    let a string = \"ab\"\n",
            "    let b string = \"cde\"\n",
            "    len(cat(a, b))\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"cat".to_string())
                && artifact.compiled.contains(&"probe".to_string()),
            "cat/probe should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());
        let code = section_body(&artifact.bytes, 10).expect("code section");
        // `len(...)` reads the char-count header at offset 0 (`i32.load align=2
        // offset=0`) then extends to i64 (`i64.extend_i32_s` = 0xac).
        assert!(
            find_subslice(&code, &[0x28, 0x02, 0x00, 0xac]).is_some(),
            "len of the concat reads the char-count header and extends to i64"
        );
    }

    #[test]
    fn substring_function_compiles_with_alloc_and_copy_codegen() {
        // `substring(s, start, end)` on `string`/`i64`/`i64` parameters must COMPILE
        // to WASM. The operands are parameters so the IR constant-folder cannot
        // collapse the call, exercising the real char->byte mapping and the fresh
        // record's alloc-and-copy.
        let source = "fn slice s string a i64 b i64 -> string\n    substring(s, a, b)\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(
            artifact.compiled,
            vec!["slice".to_string()],
            "substring should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());

        let code = section_body(&artifact.bytes, 10).expect("code section");
        // Allocates a fresh record via `call __alloc` (slice(0) then __alloc(1), so
        // its shifted WASM index is IMPORT_FUNC_COUNT + 1).
        let alloc_index = IMPORT_FUNC_COUNT as u8 + 1;
        assert!(
            find_subslice(&code, &[0x10, alloc_index]).is_some(),
            "substring emits a `call __alloc` for the slice record"
        );
        // Copies the slice's byte range with the bulk-memory `memory.copy`.
        assert!(
            find_subslice(&code, &[0xfc, 0x0a, 0x00, 0x00]).is_some(),
            "substring emits `memory.copy` for the slice byte range"
        );
        // The char->byte walk decodes UTF-8 lead bytes: it loads a byte
        // (`i32.load8_u` = 0x2d) and tests `(b & 0xC0) != 0x80`, encoded as
        // `i32.const 0xC0; i32.and; i32.const 0x80; i32.ne`.
        assert!(
            find_subslice(&code, &[0x2d]).is_some(),
            "substring emits an `i32.load8_u` byte read for the UTF-8 walk"
        );
        assert!(
            find_subslice(&code, &[0x41, 0xc0, 0x01, 0x71, 0x41, 0x80, 0x01, 0x47]).is_some(),
            "substring emits the `(b & 0xC0) != 0x80` char-start test"
        );
    }

    #[test]
    fn find_function_compiles_with_char_decode_loop() {
        // `find(haystack, needle)` on two `string` parameters must COMPILE to WASM.
        // It returns a CHAR index, so it must decode UTF-8 to count the characters
        // before the matched byte offset — the `(b & 0xC0) != 0x80` char-start test
        // (`i32.load8_u`; `i32.const 0xC0`; `i32.and`; `i32.const 0x80`; `i32.ne`)
        // must appear, and it extends the count to `i64` (`i64.extend_i32_s` = 0xac).
        let source = "fn locate h string n string -> i64\n    find(h, n)\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(
            artifact.compiled,
            vec!["locate".to_string()],
            "find should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());

        let code = section_body(&artifact.bytes, 10).expect("code section");
        // The byte search compares haystack/needle bytes with `i32.load8_u` (0x2d).
        assert!(
            find_subslice(&code, &[0x2d]).is_some(),
            "find emits `i32.load8_u` byte comparisons"
        );
        // The char-index decode loop: `(b & 0xC0) != 0x80`.
        assert!(
            find_subslice(&code, &[0x41, 0xc0, 0x01, 0x71, 0x41, 0x80, 0x01, 0x47]).is_some(),
            "find emits the `(b & 0xC0) != 0x80` char-start decode test"
        );
        // The char count is extended to i64 (the builtin's result type).
        assert!(
            find_subslice(&code, &[0xac]).is_some(),
            "find extends the char-count result to i64"
        );
    }

    #[test]
    fn contains_and_prefix_ops_compile_without_char_decode() {
        // `contains`/`starts_with`/`ends_with` are byte-exact: they scan bytes with
        // `i32.load8_u` but need NO UTF-8 char decode (byte equality is
        // char-position-independent). Each returns a `bool`.
        let source = concat!(
            "fn has s string sub string -> bool\n    contains(s, sub)\n\n",
            "fn pre s string p string -> bool\n    starts_with(s, p)\n\n",
            "fn suf s string x string -> bool\n    ends_with(s, x)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.skipped.is_empty(),
            "byte-exact predicates should all compile, skipped: {:?}",
            artifact.skipped
        );
        assert_eq!(
            artifact.compiled,
            vec!["has".to_string(), "pre".to_string(), "suf".to_string()],
        );
        let code = section_body(&artifact.bytes, 10).expect("code section");
        // Byte comparisons appear (`i32.load8_u`).
        assert!(
            find_subslice(&code, &[0x2d]).is_some(),
            "byte-exact predicates emit `i32.load8_u` comparisons"
        );
    }

    #[test]
    fn to_string_of_integer_compiles_with_itoa_codegen() {
        // `to_string(x)` on an integer argument compiles to WASM: it builds a fresh
        // string record via `call __alloc` and formats the digits with `i64.div_u`
        // (the itoa passes). The argument is a parameter so the IR constant-folder
        // cannot collapse the call to a literal, exercising the real runtime path.
        let source = "fn show n i64 -> string\n    to_string(n)\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(
            artifact.compiled,
            vec!["show".to_string()],
            "to_string of an integer should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());

        let code = section_body(&artifact.bytes, 10).expect("code section");
        // The itoa allocates the record via `call __alloc` (the last internal
        // function: show(0) then __alloc(1), shifted WASM index IMPORT_FUNC_COUNT+1).
        let alloc_index = IMPORT_FUNC_COUNT as u8 + 1;
        assert!(
            find_subslice(&code, &[0x10, alloc_index]).is_some(),
            "to_string emits a `call __alloc` for the fresh string record"
        );
        // The digit extraction uses unsigned 64-bit division (`i64.const 10`,
        // `i64.div_u` = 0x80) — the itoa core divides the magnitude down by 10.
        assert!(
            find_subslice(&code, &[0x42, 0x0a, 0x80]).is_some(),
            "to_string emits `i64.div_u` by 10 to extract decimal digits"
        );
    }

    #[test]
    fn to_string_of_float_skips_to_interpreters() {
        // `to_string(f64)` / `to_string(f32)` is DEFERRED: matching Rust's `Display`
        // dtoa bit-for-bit in WASM is out of scope, so a function calling it must be
        // demoted to the interpreters rather than miscompiled. The float `to_string`
        // is isolated in its own function so the sibling integer `to_string` still
        // compiles, proving only the float case falls back.
        let source = concat!(
            "fn float_text x f64 -> string\n    to_string(x)\n\n",
            "fn int_text n i64 -> string\n    to_string(n)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"int_text".to_string()),
            "integer to_string still compiles: {:?}",
            artifact.compiled
        );
        let skipped = artifact
            .skipped
            .iter()
            .find(|s| s.name == "float_text")
            .expect("float to_string is skipped");
        assert!(
            skipped.reason.contains("to_string"),
            "skip reason names the unsupported to_string: {}",
            skipped.reason
        );
        assert!(
            !artifact.compiled.contains(&"float_text".to_string()),
            "float to_string must not compile to WASM"
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
    fn list_of_string_element_compiles() {
        // A `list<string>` COMPILES: a `string` element is an `i32` pointer stored
        // in one slot exactly like a scalar, and — because strings are immutable —
        // is shared (not deep-recursed) on the flat word-copy deep copy. `push`
        // appends the pointer; `get` loads it back with an `i32.load`.
        let source = concat!(
            "fn names -> list<string>\n",
            "    push(list_new(), \"a\")\n\n",
            "fn head l list<string> -> string\n",
            "    get(l, 0)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"names".to_string())
                && artifact.compiled.contains(&"head".to_string()),
            "list<string> functions should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());

        let code = section_body(&artifact.bytes, 10).expect("code section");
        // The deep-copy element loop still copies each slot as one 8-byte word
        // (`i64.load` 0x29 then `i64.store` 0x37) — the string pointer is copied by
        // value (shared), NOT deep-recursed into the string record.
        assert!(
            find_subslice(&code, &[0x29, 0x03, 0x00, 0x37, 0x03, 0x00]).is_some(),
            "list<string> copy shares the element pointer via an 8-byte word copy"
        );
        // `get` loads the element slot as an `i32` pointer (`i32.load` at offset 0,
        // the slot address is fully computed on the stack), not an `i64`, because a
        // `string` element occupies the low word of the slot.
        assert!(
            find_subslice(&code, &[0x28, 0x02, 0x00]).is_some(),
            "get on a list<string> loads the element slot as an i32 pointer"
        );
    }

    #[test]
    fn list_of_mutable_heap_element_is_skipped() {
        // A `list<array<i64>>` (a fixed-`array` element) is still DEFERRED:
        // `supported_list_element`/`collection_slot_type` accept a `struct` or a
        // nested growable `list` element but NOT a fixed `array` element this
        // increment, so the function's signature is ineligible and it is skipped
        // (still runs on the interpreters), never miscompiled. (A `list<struct>` and
        // `list<list<scalar>>` DO compile now — see
        // `list_of_struct_element_compiles_with_recursive_deep_copy`.)
        let source = concat!(
            "fn grid -> list<array<i64>>\n",
            "    list_new()\n\n",
            "fn ok n i64 -> i64\n    n + 1\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["ok".to_string()]);
        assert_eq!(artifact.skipped.len(), 1);
        assert_eq!(artifact.skipped[0].name, "grid");
    }

    #[test]
    fn list_of_struct_element_compiles_with_recursive_deep_copy() {
        // A `list<struct>` COMPILES now: the element is an `i32` pointer to the
        // struct record, and the list's value-semantic deep copy RECURSES per element
        // (loads the element pointer, deep-copies the struct, stores the fresh
        // pointer) rather than sharing it — matching the interpreters' recursive
        // `Value::clone`. `get` likewise returns an independent deep copy of the
        // element (the interpreters' `values[i].clone()`), so mutating the retrieved
        // struct cannot affect the list's stored element.
        let source = concat!(
            "struct Point\n    x i64\n    y i64\n\n",
            "fn build -> list<Point>\n",
            "    push(list_new(), Point(1, 2))\n\n",
            "fn head l list<Point> -> i64\n",
            "    let p Point = get(l, 0)\n",
            "    p.x + p.y\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"build".to_string())
                && artifact.compiled.contains(&"head".to_string()),
            "list<struct> functions should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());

        let code = section_body(&artifact.bytes, 10).expect("code section");
        // A recursive per-element deep copy loads the element pointer as an `i32`
        // (`i32.load` 0x28) and, after allocating a fresh struct record, stores the
        // fresh pointer as an `i32` (`i32.store` 0x36) — the element slot is NOT
        // copied only by a flat `i64` word (which would share the pointer). The
        // struct deep copy calls `__alloc` (the last internal function: `build`(0),
        // `head`(1), `__alloc`(2), all offset past the imports), so a `call` (0x10)
        // of that index appears where a scalar-only list would not deep-copy.
        let alloc_index = IMPORT_FUNC_COUNT as u8 + 2;
        assert!(
            find_subslice(&code, &[0x28, 0x02, 0x00]).is_some(),
            "the element deep-copy loads the element pointer as an i32"
        );
        assert!(
            find_subslice(&code, &[0x10, alloc_index]).is_some(),
            "the recursive element deep-copy allocates a fresh struct record via __alloc"
        );
    }

    #[test]
    fn nested_list_element_compiles() {
        // A `list<list<i64>>` COMPILES (one level of mutable nesting): the inner list
        // element is deep-copied per outer element, and `nested_sum`-style nested
        // `get`s read the scalar leaves. A `list<list<list<i64>>>` (two levels) is
        // DEFERRED — see `nested_list_beyond_one_level_is_skipped`.
        let source = concat!(
            "fn build -> list<list<i64>>\n",
            "    push(list_new(), push(list_new(), 7))\n\n",
            "fn first l list<list<i64>> -> i64\n",
            "    let row list<i64> = get(l, 0)\n",
            "    get(row, 0)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"build".to_string())
                && artifact.compiled.contains(&"first".to_string()),
            "list<list<i64>> functions should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());
    }

    #[test]
    fn nested_list_beyond_one_level_is_skipped() {
        // `list<list<list<i64>>>` nests mutable aggregates past the one level the
        // backend can verify, so it is DEFERRED (skipped, runs on the interpreters),
        // never miscompiled.
        let source = concat!(
            "fn build -> list<list<list<i64>>>\n",
            "    list_new()\n\n",
            "fn ok n i64 -> i64\n    n + 1\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["ok".to_string()]);
        assert_eq!(artifact.skipped.len(), 1);
        assert_eq!(artifact.skipped[0].name, "build");
    }

    // -- Growable `map<K, V>` (scalar key/value) -------------------------------

    #[test]
    fn scalar_map_function_compiles_with_insert_and_lookup_codegen() {
        // A function that builds a scalar-key, scalar-value `map<i64, i64>` via
        // `map_new`/`map_set` (insert plus in-place update), reads it with
        // `map_get`/`map_has`/`map_len`, must COMPILE to WASM (not skip). The map is
        // an `i32` pointer to a `[len][cap][(k,v) pairs]` block in linear memory, so
        // both the signature (returning `map<i64, i64>`) and body are eligible.
        let source = concat!(
            "fn build n i64 -> map<i64, i64>\n",
            "    let m map<i64, i64> = map_new()\n",
            "    m = map_set(m, 1, n)\n",
            "    m = map_set(m, 2, n + 1)\n",
            "    m = map_set(m, 1, n + 2)\n",
            "    m\n\n",
            "fn probe n i64 -> i64\n",
            "    let m map<i64, i64> = build(n)\n",
            "    let seen i64 = 0\n",
            "    if map_has(m, 2)\n",
            "        seen = 1\n",
            "    map_len(m) + seen\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"build".to_string())
                && artifact.compiled.contains(&"probe".to_string()),
            "map build/probe functions should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());

        let code = section_body(&artifact.bytes, 10).expect("code section");
        // The `map_set` grow decision `new_cap = cap == 0 ? MAP_INITIAL_CAP : cap*2`
        // lowers to `i32.eqz` (0x45) then `if` producing an `i32` (0x04 0x7f).
        assert!(
            find_subslice(&code, &[0x45, 0x04, 0x7f]).is_some(),
            "map `map_set` emits the capacity-doubling grow decision"
        );
        // The linear-scan lookup compares each entry's key with `i64.eq` (0x51):
        // the key-equality opcode of `emit_map_find` for an `i64` key.
        assert!(
            find_subslice(&code, &[0x51]).is_some(),
            "map lookup emits an `i64.eq` key comparison in the scan"
        );
    }

    #[test]
    fn map_new_allocates_len_cap_header() {
        // `map_new()` allocates a `[len=0][cap=MAP_INITIAL_CAP][entries]` header:
        // the body stores 0 at the len offset and MAP_INITIAL_CAP at the cap offset.
        let source = "fn empty -> map<i64, i64>\n    map_new()\n";
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["empty".to_string()]);
        let code = section_body(&artifact.bytes, 10).expect("code section");
        // i32.const 4 (MAP_INITIAL_CAP) then i32.store at the cap offset (4).
        assert!(
            find_subslice(&code, &[0x41, MAP_INITIAL_CAP as u8, 0x36, 0x02, 0x04]).is_some(),
            "map_new stores the initial capacity into the cap header slot"
        );
    }

    #[test]
    fn map_get_lowers_to_option_construction() {
        // `map_get(m, k)` returns `option<V>`, constructed with the enum/option
        // linear-memory layout: `none` stores tag 1, `some(v)` stores tag 0 and the
        // looked-up value into the payload slot. The `i64.extend_i32_s` (0xac) of
        // `map_len` and the option tag stores must both appear.
        let source = concat!(
            "fn get_or m map<i64, i64> k i64 -> i64\n",
            "    match map_get(m, k)\n",
            "        some(v) -> v\n",
            "        none -> 0 - 1\n\n",
            "fn size m map<i64, i64> -> i64\n",
            "    map_len(m)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"get_or".to_string())
                && artifact.compiled.contains(&"size".to_string()),
            "map get/size functions should compile, skipped: {:?}",
            artifact.skipped
        );
        let code = section_body(&artifact.bytes, 10).expect("code section");
        // `map_len` extends the i32 len header to i64 with `i64.extend_i32_s` (0xac).
        assert!(
            find_subslice(&code, &[0xac]).is_some(),
            "map_len emits `i64.extend_i32_s` on the length header"
        );
    }

    #[test]
    fn map_of_string_value_function_compiles() {
        // A `map<i64, string>` (scalar key, `string` value) COMPILES: the value
        // slot holds an `i32` string pointer, shared on the flat two-word entry
        // copy since strings are immutable. `map_set` inserts/updates the pointer,
        // `map_get` returns `option<string>` (the `some` payload slot is the string
        // pointer), and `map_has`/`map_len` work unchanged.
        let source = concat!(
            "fn build n i64 -> map<i64, string>\n",
            "    let m map<i64, string> = map_new()\n",
            "    m = map_set(m, 1, \"a\")\n",
            "    m = map_set(m, 2, to_string(n))\n",
            "    m = map_set(m, 1, \"z\")\n",
            "    m\n\n",
            "fn probe n i64 -> i64\n",
            "    let m map<i64, string> = build(n)\n",
            "    let seen i64 = 0\n",
            "    if map_has(m, 2)\n",
            "        seen = 1\n",
            "    match map_get(m, 1)\n",
            "        some(s) -> len(s) + seen + map_len(m)\n",
            "        none -> 0 - 1\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"build".to_string())
                && artifact.compiled.contains(&"probe".to_string()),
            "map<i64, string> build/probe functions should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());

        let code = section_body(&artifact.bytes, 10).expect("code section");
        // The entry copy still copies each entry as two 8-byte words (`i64.load`
        // 0x29 then `i64.store` 0x37) — the string value pointer is copied by value
        // (shared), NOT deep-recursed into the string record.
        assert!(
            find_subslice(&code, &[0x29, 0x03, 0x00, 0x37, 0x03, 0x00]).is_some(),
            "map<i64, string> copy shares the value pointer via an 8-byte word copy"
        );
        // The lookup still compares the scalar `i64` key with `i64.eq` (0x51).
        assert!(
            find_subslice(&code, &[0x51]).is_some(),
            "map<i64, string> lookup emits an `i64.eq` key comparison in the scan"
        );
    }

    #[test]
    fn map_string_key_function_compiles() {
        // A `map<string, i64>` (string KEY, scalar value) and a `map<string, string>`
        // (string key AND value) COMPILE: the key slot holds an `i32` string pointer,
        // and the lookup compares keys by CONTENT — not by pointer identity — so two
        // distinct string objects with equal bytes are the same key. `map_set`
        // inserts/updates by content, `map_get` returns `option<V>`, and
        // `map_has`/`map_len` work through the content-equality scan.
        let source = concat!(
            "fn build n i64 -> map<string, i64>\n",
            "    let m map<string, i64> = map_new()\n",
            "    m = map_set(m, \"a\" + \"b\", n)\n",
            "    m = map_set(m, \"c\", n + 1)\n",
            "    m = map_set(m, \"ab\", n + 2)\n",
            "    m\n\n",
            "fn probe n i64 -> i64\n",
            "    let m map<string, i64> = build(n)\n",
            "    let seen i64 = 0\n",
            "    if map_has(m, \"c\")\n",
            "        seen = 1\n",
            "    match map_get(m, \"ab\")\n",
            "        some(v) -> v + seen + map_len(m)\n",
            "        none -> 0 - 1\n\n",
            "fn labels -> map<string, string>\n",
            "    let m map<string, string> = map_new()\n",
            "    m = map_set(m, \"k\", \"v\")\n",
            "    m\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"build".to_string())
                && artifact.compiled.contains(&"probe".to_string())
                && artifact.compiled.contains(&"labels".to_string()),
            "map<string, _> functions should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());

        let code = section_body(&artifact.bytes, 10).expect("code section");
        // The string-key lookup compares keys by CONTENT: the byte loop emits an
        // `i32.load8_u` (0x2d) over the UTF-8 bytes — a marker of the content
        // comparison that a scalar-key map (integer `i64.eq`/`i32.eq` only) never
        // emits inside its find scan.
        assert!(
            find_subslice(&code, &[0x2d]).is_some(),
            "map<string, _> lookup emits a byte-compare (`i32.load8_u`) key equality"
        );
    }

    #[test]
    fn map_with_mutable_value_is_skipped() {
        // A `map<i64, array<i64>>` (a fixed-`array` value) is DEFERRED:
        // `supported_map_kv`/`collection_slot_type` accept a `struct` value but NOT a
        // fixed `array` value this increment, so the signature is ineligible and the
        // function is skipped (still runs on the interpreters), never miscompiled.
        // (The semantic layer already restricts `map` KEYS to `i64` or `string` —
        // L0388 — so a non-string heap key never reaches this backend.) A `string`
        // value now compiles (`map_string_key_function_compiles`), and a `struct`
        // value now compiles too — see `map_of_struct_value_compiles`.
        let source = concat!(
            "fn rows -> map<i64, array<i64>>\n",
            "    map_new()\n\n",
            "fn ok n i64 -> i64\n    n + 1\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["ok".to_string()]);
        let skipped: Vec<&str> = artifact.skipped.iter().map(|s| s.name.as_str()).collect();
        assert!(skipped.contains(&"rows"));
    }

    #[test]
    fn map_of_struct_value_compiles() {
        // A `map<i64, struct>` COMPILES now: the value slot is an `i32` pointer to
        // the struct record, and the map's value-semantic deep copy RECURSES per
        // entry (loads the value pointer, deep-copies the struct, stores the fresh
        // pointer). `map_get` returns `option<struct>` (built directly so the option
        // lays out a struct payload) with an independent deep-copied value, matching
        // the interpreters' recursive `Value::clone`.
        let source = concat!(
            "struct Point\n    x i64\n    y i64\n\n",
            "fn build -> map<i64, Point>\n",
            "    map_set(map_new(), 1, Point(3, 4))\n\n",
            "fn value m map<i64, Point> k i64 -> i64\n",
            "    match map_get(m, k)\n",
            "        some(p) -> p.x + p.y\n",
            "        none -> 0 - 1\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert!(
            artifact.compiled.contains(&"build".to_string())
                && artifact.compiled.contains(&"value".to_string()),
            "map<i64, struct> functions should compile, skipped: {:?}",
            artifact.skipped
        );
        assert!(artifact.skipped.is_empty());
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
        // `map<i64, map<i64, i64>>` is a map whose VALUE is itself a map — a nested
        // collection the backend does not lay out (a map element/value is deferred),
        // so nothing is eligible and the backend reports L0338. (Scalar/`string`
        // element `list`s, `list<struct>`/`list<list<scalar>>`, `map<K, struct>`,
        // and enum payloads like `result<i64, string>`/`result<i64, list<i64>>` ARE
        // supported now — see the growable-list/map, struct-element, and enum tests.)
        let source = "fn tally n i64 -> map<i64, map<i64, i64>>\n    map_new()\n";
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
    fn overflow_builtins_compile() {
        // The overflow-aware builtins now compile on the WASM backend (matching
        // native): `saturating_*`/`wrapping_*` yield a fixed-width scalar and
        // `checked_*` an `option<T>`. Every function is compiled, none skipped.
        let source = concat!(
            "fn sat a u8 b u8 -> u8\n",
            "    saturating_add(a, b)\n\n",
            "fn wrap a u8 b u8 -> u8\n",
            "    wrapping_mul(a, b)\n\n",
            "fn chk a i8 b i8 -> i64\n",
            "    match checked_add(a, b)\n",
            "        some(v) -> to_i64(v)\n",
            "        none -> 0 - 1\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(
            artifact.compiled,
            vec!["sat".to_string(), "wrap".to_string(), "chk".to_string()]
        );
        assert!(artifact.skipped.is_empty(), "nothing should skip");
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
    fn enum_with_string_payload_compiles() {
        // `result<i64, string>` has a `string` payload, which the WASM backend now
        // supports: the payload slot holds the immutable string pointer, matched
        // and read back with an `i32` slot load. The function COMPILES (it is not
        // skipped and does not fall back to the interpreters).
        let source = concat!(
            "fn describe r result<i64, string> -> i64\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> len(m)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["describe".to_string()]);
        assert!(artifact.skipped.is_empty());
    }

    #[test]
    fn enum_with_one_level_mutable_payload_compiles() {
        // `result<i64, list<i64>>` has a one-level MUTABLE-aggregate (`list<i64>`)
        // payload, which the WASM backend now supports: the payload slot is an `i32`
        // pointer deep-copied per variant on the enum's value-semantic copy, matching
        // the interpreters' recursive `Value::clone`. The function COMPILES.
        let source = concat!(
            "fn describe r result<i64, list<i64>> -> i64\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> len(m)\n",
        );
        let artifact = emit_wasm_module(&module_for(source)).expect("emit");
        assert_eq!(artifact.compiled, vec!["describe".to_string()]);
        assert!(artifact.skipped.is_empty());
    }

    #[test]
    fn enum_with_deeply_nested_mutable_payload_is_skipped() {
        // `result<i64, list<list<list<i64>>>>` nests mutable aggregates past the one
        // level the backend can verify, so it is DEFERRED (skipped, runs on the
        // interpreters), never miscompiled. With no other eligible function, emission
        // reports L0338.
        let source = concat!(
            "fn describe r result<i64, list<list<list<i64>>>> -> i64\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> len(m)\n",
        );
        let error = emit_wasm_module(&module_for(source)).expect_err("no eligible functions");
        assert_eq!(error.code, "L0338");
        assert!(
            error.skipped.iter().any(|s| s.name == "describe"),
            "the deeply-nested-mutable-payload enum function is recorded as skipped: {:?}",
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

        let structs = struct_table(&[]);
        let option =
            enum_layout(&TypeRef::new("option<i64>"), &structs, &enums).expect("option layout");
        assert_eq!(option.tag_of("some"), Some(0));
        assert_eq!(option.tag_of("none"), Some(1));
        assert_eq!(option.slot_count, 1);

        let result = enum_layout(&TypeRef::new("result<i64, i64>"), &structs, &enums)
            .expect("result layout");
        assert_eq!(result.tag_of("ok"), Some(0));
        assert_eq!(result.tag_of("err"), Some(1));

        let shape = enum_layout(&TypeRef::new("Shape"), &structs, &enums).expect("user layout");
        assert_eq!(shape.tag_of("Circle"), Some(0));
        assert_eq!(shape.tag_of("Rect"), Some(1));
        assert_eq!(shape.tag_of("Empty"), Some(2));
        assert_eq!(shape.slot_count, 2, "widest variant `Rect` has two slots");

        // A `string` payload IS supported (shared immutable pointer in one slot).
        assert!(
            enum_layout(&TypeRef::new("result<i64, string>"), &structs, &enums).is_some(),
            "string-payload result is a supported WASM enum"
        );
        // A one-level MUTABLE-aggregate payload (`list<i64>`) is NOW supported — the
        // payload slot is deep-copied per variant on the enum's value-semantic copy.
        assert!(
            enum_layout(&TypeRef::new("result<i64, list<i64>>"), &structs, &enums).is_some(),
            "a one-level list payload is a supported WASM enum (recursive deep copy)"
        );
        // A `map` payload is still DEFERRED (the backend does not lay out a map
        // element/value inside an enum this increment).
        assert!(
            enum_layout(
                &TypeRef::new("result<i64, map<i64, i64>>"),
                &structs,
                &enums
            )
            .is_none(),
            "a map payload is not yet a supported WASM enum"
        );
        // Nesting beyond one mutable level (`list<list<list<i64>>>`) is DEFERRED.
        assert!(
            enum_layout(
                &TypeRef::new("option<list<list<list<i64>>>>"),
                &structs,
                &enums
            )
            .is_none(),
            "a payload nested past one mutable level is deferred"
        );
    }
}
