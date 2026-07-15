//! Native runtime memory layout and collection shape rules: heap/list/map/string/
//! struct field offsets and sizes, the runtime helper symbol names, the COFF
//! section/relocation/CodeView constants, and the collection type-shape predicates
//! that decide which aggregate shapes the native backend can represent. Split out
//! of native_object.rs; sees the parent's items via `use super::*`.

use super::*;

/// The bump-allocator helper emitted in `.text`. Signature: the requested byte
/// count is passed in `rcx`, the allocated heap pointer is returned in `rax`.
/// See `emit_heap_alloc_helper` for the body.
pub(crate) const HEAP_ALLOC_SYMBOL: &str = "__lullaby_alloc";

/// The string-length helper emitted in `.text`. Signature: a pointer to a
/// NUL-terminated byte string in `.rdata` is passed in `rcx`; the byte length of
/// a fresh heap copy of that string is returned in `rax`. This exercises the
/// full first heap step: it bump-allocates via `__lullaby_alloc`, copies the
/// `.rdata` bytes into the heap, then scans the heap copy for the terminator.
pub(crate) const HEAP_STRLEN_SYMBOL: &str = "__lullaby_strlen_copy";

/// The bump-pointer cell symbol in `.bss` (an 8-byte pointer, zero-initialized).
/// A zero value means "not yet initialized"; the allocator lazily seeds it to
/// the base of the heap region on first use.
pub(crate) const HEAP_NEXT_SYMBOL: &str = "__lullaby_heap_next";

/// The free-list head cell symbol in `.bss` (an 8-byte pointer, zero-initialized).
/// Reference-counted blocks freed by `__lullaby_rc_free` are pushed onto this LIFO
/// free list and reused by `__lullaby_alloc` (first-fit). `0` means the list is
/// empty. Until scope-based drop insertion (RC stage 2) emits `rc_dec`/`rc_free`
/// calls, nothing frees, so the list stays empty and the allocator behaves exactly
/// like the old bump allocator (plus a per-block RC header).
pub(crate) const HEAP_FREE_HEAD_SYMBOL: &str = "__lullaby_free_head";

/// The heap region base symbol in `.bss` — a fixed reserved bump region.
pub(crate) const HEAP_BASE_SYMBOL: &str = "__lullaby_heap_base";

/// The arena-mode flag cell in `.bss` (an 8-byte word, zero-initialized).
///
/// Arena-first memory (stage 1): a **function-scoped implicit region** for a
/// provably-local heap-using function. While such a function runs this flag is
/// `1`; `__lullaby_alloc` then skips its free-list reuse scan and *always* bump-
/// allocates, and `__lullaby_rc_free` becomes a no-op (it does not push the block
/// onto the free list). The function saves `__lullaby_heap_next` on entry and
/// restores it on every return edge, so the whole region it allocated is reclaimed
/// by a single bulk rewind of the bump pointer. Because arena allocations only ever
/// bump (never touch the pre-existing free list) and arena frees never push,
/// restoring the bump pointer is sound and the free list is invariant across the
/// call. Default `0` selects the normal RC / free-list path (every non-arena
/// function, and the whole program before any arena function runs). The cell lives
/// past the heap region so the RC cell offsets (`heap_next`=0, `free_head`=8,
/// `heap_base`=16) stay unchanged.
pub(crate) const ALLOC_MODE_SYMBOL: &str = "__lullaby_alloc_mode";

/// Byte offset of the arena-mode flag within `.bss`, immediately after the heap
/// region (so the pre-existing cell offsets are unchanged).
pub(crate) const ALLOC_MODE_OFFSET: u32 = 16 + HEAP_REGION_SIZE;

/// Total size of the `.bss` section backing the heap: the three RC cells (24 bytes
/// through `heap_base`... conceptually 16 for the cells preceding the region) plus
/// the 1 MiB region plus the 8-byte arena-mode flag past it. Computed as the mode
/// cell offset plus its 8 bytes.
pub(crate) const HEAP_BSS_SIZE: u32 = ALLOC_MODE_OFFSET + 8;

/// `__lullaby_rc_dec(payload ptr in rcx)`: decrement the block's refcount at
/// `[rcx - 8]` and, if it reached zero, tail-call `__lullaby_rc_free` to return the
/// block to the free list. The drop primitive scope-based drop insertion (RC stage
/// 2) emits at each scope-exit edge of a uniquely-owned heap local.
pub(crate) const RC_DEC_SYMBOL: &str = "__lullaby_rc_dec";

/// `__lullaby_rc_free(payload ptr in rcx)`: push the block (whose base is
/// `rcx - 16`) onto the LIFO free list `__lullaby_free_head`, threading the "next"
/// link through the freed block's refcount slot (`[base + 8]`). The allocator
/// first-fit-reuses it on a later allocation.
pub(crate) const RC_FREE_SYMBOL: &str = "__lullaby_rc_free";

/// `__lullaby_drop_string_array(block ptr in rcx)`: a RECURSIVE drop for a
/// `list<string>`-layout block (an `array<string>` / `list<string>`): `rc_dec` each
/// of the `len` shared string element pointers, then `rc_dec` the block itself.
/// Used to reclaim a uniquely-owned `array<string>` loop temporary (a `split`
/// result) whose string elements are owned solely by the block.
pub(crate) const DROP_STRING_ARRAY_SYMBOL: &str = "__lullaby_drop_string_array";

/// Size of the per-allocation reference-counting header, in bytes: `[size i64]
/// [refcount i64]` sitting immediately BEFORE the payload the allocator returns.
/// The returned pointer names the payload (offset 0 = the first payload word), so
/// every existing record offset (string/list/map/struct/enum) is unchanged; the
/// header is addressed at negative offsets (`refcount` at `[ptr - 8]`, `size` at
/// `[ptr - 16]`). Storing the block size lets `rc_free` return a block to the
/// free list and `alloc` first-fit-reuse it.
pub(crate) const RC_HEADER_SIZE: i32 = 16;

/// Size in bytes of the fixed reserved native heap region. Growable lists
/// capacity-double and orphan their old backing blocks in this no-reclaim bump
/// heap, so the region is sized generously (1 MiB) to give list-building
/// programs headroom. It lives in zero-initialized `.bss`, so it costs no object
/// file bytes.
pub(crate) const HEAP_REGION_SIZE: u32 = 1024 * 1024;

// -- Growable list layout (native) -------------------------------------------
//
// A growable `list<T>` (scalar `T`) is a heap pointer (one 8-byte word / register
// value) to a header `[len: i64][cap: i64][elem slots...]`: the current element
// count, the allocated capacity, then `cap` 8-byte element slots. Every field is
// an 8-byte word so the whole block is naturally `i64`-aligned and element `i`
// lives at `LIST_DATA_OFF + i * 8`, letting a scalar element (i64/fixed-width/
// bool/char/byte, or an f64/f32 stored bit-for-bit in its low bytes) be moved as
// a flat 8-byte word.
//
// Value semantics: Lullaby lists are value-semantic (`l = push(l, x)` returns a
// NEW list). Every mutating op (`push`/`set`/`pop`) deep-copies the source list
// first and mutates the fresh copy, so mutating one binding is never observable
// through another (`let b = a` then `set(b, ...)` leaves `a` untouched because
// `set` copies `b`). Read ops (`get`/`len`) never mutate, so sharing a list
// pointer across a binding or a call boundary is safe without an extra copy —
// exactly matching the interpreters bit-for-bit. The bump allocator never
// reclaims, so a grown or copied list orphans its old block, like the existing
// string-constant heap growth.

/// Byte offset of a list's `len` header (element count), an `i64` word.
pub(crate) const LIST_LEN_OFF: i32 = 0;

/// Byte offset of a list's `cap` header (allocated capacity in elements), an
/// `i64` word.
pub(crate) const LIST_CAP_OFF: i32 = 8;

/// Byte offset of a list's first element slot, past the two `i64` headers.
pub(crate) const LIST_DATA_OFF: i32 = 16;

/// Bytes per list element slot — one 8-byte word, like struct/array/enum slots.
pub(crate) const LIST_SLOT_SIZE: i32 = 8;

/// Initial capacity a `list_new()` header (and the first growth of an empty list)
/// allocates, so a handful of pushes do not each trigger a realloc. Mirrors the
/// WASM backend's `LIST_INITIAL_CAP`.
pub(crate) const LIST_INITIAL_CAP: i64 = 4;

/// The list-runtime helper emitted in `.text`. Signature: no arguments; returns a
/// fresh `[len=0][cap=LIST_INITIAL_CAP][slots]` heap block pointer in `rax`.
pub(crate) const LIST_NEW_SYMBOL: &str = "__lullaby_list_new";

/// The list deep-copy helper emitted in `.text`. Signature: the source list
/// pointer in `rcx`; returns a fresh independent copy's pointer in `rax`.
pub(crate) const LIST_COPY_SYMBOL: &str = "__lullaby_list_copy";

/// The list-grow helper emitted in `.text`. Signature: the list pointer in `rcx`;
/// returns a (possibly reallocated) list pointer in `rax` guaranteed to have
/// `cap > len` (room for one more push). Doubles capacity (or seeds
/// `LIST_INITIAL_CAP` from an empty list) and copies the live elements.
pub(crate) const LIST_GROW_SYMBOL: &str = "__lullaby_list_grow";

// -- Heap struct layout (native, collection-element representation) -----------
//
// A struct used as a MUTABLE-heap collection element/value/enum payload
// (`list<struct>`, `map<K, struct>`, `option<struct>`) cannot be the stack-
// flattened `NativeType::Struct` (that occupies many words); it must fit a single
// element slot. So it is laid out on the HEAP: a header word `[nwords i64]`
// followed by `nwords` field words (one 8-byte word per flattened field, in
// declared order). The value in the element slot is a pointer to FIELD 0 — i.e.
// `alloc_base + 8` — so field `k` lives at `[ptr + 8*k]` and the word count is at
// `[ptr - STRUCT_HEADER_SIZE]`. Storing the count in the block lets a single
// type-agnostic `__lullaby_struct_copy` helper deep-copy any heap struct (its
// fields are always scalars/immutable strings at the one-level nesting bound, so a
// flat word copy IS an exact deep copy). Because the header sits *below* field 0,
// heap-struct field access (`p.x`) and the heap→stack bridge address fields at
// `[ptr + 8*k]` with no header adjustment.

/// Bytes of the heap-struct header (the `[nwords]` word), stored just below the
/// field-0 pointer. The allocation is `STRUCT_HEADER_SIZE + nwords * 8` bytes and
/// the value pointer is `alloc_base + STRUCT_HEADER_SIZE`.
pub(crate) const STRUCT_HEADER_SIZE: i32 = 8;

/// The generic heap-struct deep-copy helper emitted in `.text`. Signature: the
/// source heap-struct pointer (to field 0) in `rcx`; returns in `rax` a fresh
/// independent block's field-0 pointer. Reads the `[rcx - STRUCT_HEADER_SIZE]`
/// word count, allocates `STRUCT_HEADER_SIZE + nwords * 8`, copies the header and
/// every field word (a flat copy — heap-struct fields are scalars or shared
/// immutable strings at the one-level nesting bound), and returns field-0 pointer.
pub(crate) const STRUCT_COPY_SYMBOL: &str = "__lullaby_struct_copy";

/// Builtins that construct or read growable lists, matched by name in call
/// lowering (arity / element type are validated there against the IR types).
pub(crate) const LIST_NEW_BUILTIN: &str = "list_new";
pub(crate) const LIST_PUSH_BUILTIN: &str = "push";
pub(crate) const LIST_GET_BUILTIN: &str = "get";
pub(crate) const LIST_SET_BUILTIN: &str = "set";
pub(crate) const LIST_POP_BUILTIN: &str = "pop";

/// Whether a growable-collection element/value/payload type occupies a single
/// 8-byte native slot copied by a flat word — a native scalar (`i64`/fixed-width/
/// `bool`/`char`/`byte`/`f64`/`f32`) or a `string` (an immutable heap pointer). A
/// `string` occupies one word exactly like a scalar and, because strings are
/// immutable, is copied by SHARING its pointer on a value-semantic deep copy
/// (never deep-recursed into the string record) — so the flat word copy the list/
/// map/enum copy paths already emit is an exact deep copy and needs no per-slot
/// type dispatch. This mirrors the WASM backend's `scalar_or_string_slot_type`.
/// Other (mutable) heap types (`struct`/`array`/`list`/`map`) are NOT single-slot
/// copyable — they would need a recursive per-element deep copy — so they return
/// `false` and the enclosing function skips gracefully.
pub(crate) fn is_scalar_or_string_slot(ty: &TypeRef) -> bool {
    ty.name == "i64"
        || fixed_int_kind(&ty.name).is_some()
        || matches!(ty.name.as_str(), "bool" | "char" | "byte" | "f64" | "f32")
        || ty.name == "string"
}

/// The maximum depth of MUTABLE-aggregate nesting a growable collection
/// element/value/enum payload may reach before the native backend defers it.
/// Depth 0 is the collection's own element/value slot; a struct field or a nested
/// list element consumes one level. One level of mutable nesting (`list<struct>`,
/// `list<list<scalar>>`, `map<K, struct>`, `option<struct>`) is supported; deeper
/// cases (`list<list<list<…>>>`, `list<map<…>>`, a struct field that is itself a
/// list/map/struct) are skipped gracefully (still run on the interpreters) rather
/// than miscompiled. Mirrors the WASM backend's `MAX_COLLECTION_NEST_DEPTH`.
const MAX_COLLECTION_NEST_DEPTH: u32 = 1;

/// The native slot layout of a growable-collection element/value/enum payload at
/// nesting `depth`, or `None` if the native backend cannot lay it out (so the
/// enclosing function skips gracefully). This is the native mirror of the WASM
/// backend's `collection_slot_type`, bounded to one mutable-aggregate level.
/// Accepts, in order:
///
/// - a **scalar** — its own single-word layout, flat-copied on a deep copy;
/// - a **`string`** — a single pointer word to the immutable record, SHARED on a
///   deep copy (never deep-recursed) since strings are immutable;
/// - a **mutable aggregate** (a named `struct` → [`NativeType::HeapStruct`], or a
///   supported nested growable `list<T>` → [`NativeType::List`]) at
///   `depth < MAX_COLLECTION_NEST_DEPTH` — a single pointer word that is itself
///   DEEP-COPIED per element on the collection's value-semantic copy (see
///   [`emit_heap_slot_deep_copy`]), matching the interpreters' recursive
///   `Value::clone`. The nested aggregate's own fields/elements are classified one
///   level deeper, so `list<list<scalar>>` is accepted but `list<list<list<…>>>`
///   is deferred.
///
/// A nested `map` element/value (`list<map<…>>`, `map<K, map<…>>`), a fixed
/// `array` element, and an `enum` element are DEFERRED (return `None`).
pub(crate) fn native_collection_slot(
    ty: &TypeRef,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    depth: u32,
) -> Option<NativeType> {
    if is_scalar_or_string_slot(ty) {
        // A scalar or immutable-string element occupies one flat word.
        return resolve_native_type(ty, structs, enums).ok();
    }
    if depth >= MAX_COLLECTION_NEST_DEPTH {
        return None;
    }
    // A struct element/value: every field must itself be layable-out (a scalar, a
    // `string`, or a mutable aggregate one level deeper). Laid out on the heap as a
    // single pointer word (`HeapStruct`), deep-copied per element.
    if let Some(def) = structs.iter().find(|s| s.name == ty.name) {
        let mut fields = Vec::with_capacity(def.fields.len());
        for (field_name, field_ty) in &def.fields {
            let native = native_collection_slot(field_ty, structs, enums, depth + 1)?;
            fields.push((field_name.clone(), native));
        }
        return Some(NativeType::HeapStruct {
            name: ty.name.clone(),
            fields,
        });
    }
    // A nested growable `list<T>` element/value: its own element must be layable-out
    // one level deeper (so `list<list<scalar>>` works, `list<list<list<…>>>` does
    // not). The nested list is deep-copied per element on the outer copy.
    if let Some(elem) = ty.list_element() {
        let elem_native = native_collection_slot(&elem, structs, enums, depth + 1)?;
        return Some(NativeType::List {
            elem: Box::new(elem_native),
        });
    }
    None
}

/// Whether a resolved `NativeType`, when it occupies a collection element / map
/// value / enum payload slot, must be DEEP-COPIED (recursively) on a value-semantic
/// copy rather than flat-word-copied. A scalar or immutable `string` is flat-copied
/// (a `string`'s shared pointer IS its value-semantic copy); a `HeapStruct` or a
/// nested `List`/`Map` element must be recursively deep-copied so mutating one copy
/// is never observable through another (the interpreters' recursive `Value::clone`).
/// Mirrors the WASM backend's `is_mutable_aggregate` for the element context.
pub(crate) fn native_slot_needs_deep_copy(ty: &NativeType) -> bool {
    matches!(
        ty,
        NativeType::HeapStruct { .. } | NativeType::List { .. } | NativeType::Map { .. }
    )
}

/// The element type of a supported growable `list<T>`, or `None` if `ty` is not a
/// list or its element is neither a native scalar nor a `string`. A `string`
/// element is an immutable heap pointer stored in one slot and shared (not
/// deep-recursed) on the value-semantic deep copy, so `list<string>` is supported.
/// Lists of MUTABLE heap elements (`list<struct>`/`list<list<…>>`/`list<map<…>>`)
/// are DEFERRED — the native backend does not yet recursively deep-copy mutable
/// heap elements — so such a list is unsupported and its enclosing function skips
/// (still runs on the interpreters).
///
/// A **one-level MUTABLE-aggregate element** — a named `struct` or a nested
/// `list<scalar|string>` — is now also accepted (`list<struct>`,
/// `list<list<i64>>`): such an element occupies one pointer word and is
/// DEEP-COPIED per element on the value-semantic copy (see [`native_collection_slot`]
/// and [`emit_heap_slot_deep_copy`]), matching the WASM backend and the
/// interpreters' recursive `Value::clone`. Deeper nesting, a `map`/`array` element,
/// or an `enum` element stays DEFERRED. This function is a **structural** accept
/// (it does not re-validate a named element against the struct/enum tables — the
/// eligibility gate's `resolve_native_type` already rejected an unresolvable element
/// before any lowering guard consults this), so it takes only the `TypeRef`.
pub(crate) fn supported_list_element(ty: &TypeRef) -> Option<TypeRef> {
    let elem = ty.list_element()?;
    if is_native_collection_element_shape(&elem) {
        Some(elem)
    } else {
        None
    }
}

/// A heap-backed `array<string>` — the `split`/`words` result and `array<string>`
/// literals. It is represented natively exactly like a `list<string>`: a pointer
/// to a `[len][cap][slot…]` block of shared immutable string pointers. Returns the
/// element type (`string`) when `ty` is such an array, else `None`. Only `string`
/// elements are heap-backed here; `array<i64>`/`array<f64>` stay stack-flattened
/// with a statically-inferred length, so they are excluded.
pub(crate) fn heap_string_array_element(ty: &TypeRef) -> Option<TypeRef> {
    let elem = ty.name.strip_prefix("array<")?.strip_suffix('>')?;
    (elem == "string").then(|| TypeRef::new("string"))
}

/// Structural shape test for a collection element / map value / enum payload: a
/// scalar, a `string`, a named type (a struct — validated for real by
/// `native_collection_slot`/`resolve_native_type`), or a nested `list<…>` whose own
/// element is again a plausible shape. Bounded to one nested `list` level here (a
/// `list<list<list<…>>>` element is a `list` whose element is a `list`, which this
/// rejects) so the structural guard already excludes over-deep nesting; the full
/// depth-and-table validation is `native_collection_slot`. A `map`/`array` element
/// is rejected (deferred).
fn is_native_collection_element_shape(ty: &TypeRef) -> bool {
    if is_scalar_or_string_slot(ty) {
        return true;
    }
    if let Some(inner) = ty.list_element() {
        // A nested list: accept when its own element is a scalar or string (one
        // mutable level). `list<list<list<…>>>` (inner is itself a list) is rejected.
        return is_scalar_or_string_slot(&inner);
    }
    if ty.name.starts_with("map<") || ty.name.starts_with("array<") {
        return false;
    }
    // A bare named type is a candidate struct; `native_collection_slot` validates it
    // against the struct table and bounds its field nesting.
    ty.list_element().is_none() && ty.option_element().is_none() && ty.result_args().is_none()
}

// -- Growable map layout (native) --------------------------------------------
//
// A growable `map<K, V>` (scalar `K`/`V`) is a heap pointer (one 8-byte word,
// like a list) to a header `[len: i64][cap: i64][entries...]`: the current entry
// count, the allocated capacity in entries, then `cap` two-word entries. Each
// entry is a `(key, value)` pair of 8-byte words — the key at `+0`, the value at
// `+MAP_VALUE_OFF` — so entry `i` lives at `MAP_DATA_OFF + i * MAP_ENTRY_SIZE`.
// Every field is an 8-byte word (uniform with native list/struct/enum slots).
//
// This mirrors the interpreters' `Value::Map`: an INSERTION-ORDERED association
// list scanned linearly with `Value` equality. `map_set` overwrites the value of
// an existing key in place (preserving its position) or appends a new entry at
// the end, growing with capacity doubling like a list; `map_get`/`map_has` scan
// entries front-to-back so the FIRST matching key wins; `map_len` reads the
// header. Ordering and lookup therefore match the interpreters bit-for-bit.
//
// Value semantics: like native lists, maps are value-semantic. Every mutating op
// (`map_set`) deep-copies the source map first and mutates the fresh copy, and a
// map crossing a call boundary shares its pointer safely because the only
// mutator copies. Read ops (`map_get`/`map_has`/`map_len`) never mutate. The bump
// allocator never reclaims, so a grown/copied map orphans its old block.
//
// Key equality is a raw 8-byte word compare (`cmp`), exact for the integer-cell
// key types (`i64`/fixed-width/`bool`/`char`/`byte`, all stored as normalized
// `i64` cells). FLOAT keys are DEFERRED here: their word compare would treat
// `+0.0`/`-0.0` and NaNs differently from the interpreters' value equality (and
// from the WASM backend's ordered `f*.eq`), so a `map<f64, V>`/`map<f32, V>` is
// unsupported and its function skips to the interpreters. Float VALUES are fine:
// they are stored/loaded bit-for-bit and never compared.

/// Byte offset of a map's `len` header (entry count), an `i64` word.
pub(crate) const MAP_LEN_OFF: i32 = 0;

/// Byte offset of a map's `cap` header (allocated capacity in entries), an `i64`
/// word.
pub(crate) const MAP_CAP_OFF: i32 = 8;

/// Byte offset of a map's first entry, past the two `i64` headers.
pub(crate) const MAP_DATA_OFF: i32 = 16;

/// Byte offset of the value word within an entry (past the key word).
pub(crate) const MAP_VALUE_OFF: i32 = 8;

/// Bytes per map entry — a `(key, value)` pair of two 8-byte words.
pub(crate) const MAP_ENTRY_SIZE: i32 = 16;

/// Initial capacity a `map_new()` header (and the first growth of an empty map)
/// allocates. Mirrors the list `LIST_INITIAL_CAP` and the WASM `MAP_INITIAL_CAP`.
pub(crate) const MAP_INITIAL_CAP: i64 = 4;

/// The map-runtime helper emitted in `.text`. Signature: no arguments; returns a
/// fresh `[len=0][cap=MAP_INITIAL_CAP][entries]` heap block pointer in `rax`.
pub(crate) const MAP_NEW_SYMBOL: &str = "__lullaby_map_new";

/// The map deep-copy helper emitted in `.text`. Signature: the source map pointer
/// in `rcx`; returns a fresh independent copy's pointer in `rax`.
pub(crate) const MAP_COPY_SYMBOL: &str = "__lullaby_map_copy";

/// The map-grow helper emitted in `.text`. Signature: the map pointer in `rcx`;
/// returns a (possibly reallocated) map pointer in `rax` guaranteed to have
/// `cap > len` (room for one more entry). Doubles capacity (or seeds
/// `MAP_INITIAL_CAP` from an empty map) and copies the live entries.
pub(crate) const MAP_GROW_SYMBOL: &str = "__lullaby_map_grow";

/// The map linear-scan helper emitted in `.text`. Signature: the map pointer in
/// `rcx`, the key word in `rdx`; returns in `rax` the index of the first entry
/// whose key equals `rdx`, or the map's `len` if no key matched (the "found index
/// else len" convention shared by `map_set`/`map_get`/`map_has`).
pub(crate) const MAP_FIND_SYMBOL: &str = "__lullaby_map_find";

/// Builtins that construct or read growable maps, matched by name in call
/// lowering (arity / key / value types are validated there).
pub(crate) const MAP_NEW_BUILTIN: &str = "map_new";
pub(crate) const MAP_SET_BUILTIN: &str = "map_set";
pub(crate) const MAP_GET_BUILTIN: &str = "map_get";
pub(crate) const MAP_HAS_BUILTIN: &str = "map_has";
pub(crate) const MAP_LEN_BUILTIN: &str = "map_len";

/// The `(key, value)` element types of a supported growable `map<K, V>`, or
/// `None` if `ty` is not a map, its key is not a supported native scalar, or its
/// value is neither a native scalar nor a `string`. Keys are restricted to the
/// integer-cell scalar types (`i64`, fixed-width integers, `bool`/`char`/`byte`)
/// so that key equality is an exact 8-byte word compare. Values may be any native
/// scalar including a float (`f64`/`f32`, stored bit-for-bit, never compared) or a
/// `string` (an immutable heap pointer stored in one slot, shared on the flat
/// two-word entry copy since strings are immutable — `map<K, string>` is
/// supported). Heap KEYS (`map<string, V>`, …), MUTABLE heap values
/// (`map<K, list<…>>`, `map<K, struct>`), and float keys are DEFERRED — such a map
/// is unsupported and its enclosing function skips (still runs on the
/// interpreters), matching the WASM map's increment. A string key stays deferred
/// because the interpreters compare keys by content equality (decoded bytes), not
/// by the interned pointer — separate work.
pub(crate) fn supported_map_kv(ty: &TypeRef) -> Option<(TypeRef, TypeRef)> {
    let (key, value) = ty.map_args()?;
    // Key must be an integer-cell scalar (word-compare equality).
    if !(key.name == "i64"
        || fixed_int_kind(&key.name).is_some()
        || matches!(key.name.as_str(), "bool" | "char" | "byte"))
    {
        return None;
    }
    // Value may be any native scalar (including a float), a `string` pointer, or a
    // one-level MUTABLE aggregate (a `struct` or a nested `list<scalar|string>`),
    // which is deep-copied per value on the map's value-semantic copy
    // (`map<K, struct>` is now supported). A `map`/`array` value is DEFERRED.
    if !is_native_collection_element_shape(&value) {
        return None;
    }
    Some((key, value))
}

// -- Heap string layout (native) ---------------------------------------------
//
// A first-class `string` value is a heap pointer (one 8-byte word / register
// value) to a record `[char_len: i64][byte_len: i64][utf8 bytes]`: the Unicode
// scalar (char) count, the UTF-8 byte length, then the encoded bytes. This
// mirrors the WASM backend's string record (which uses i32 headers); native uses
// i64 headers so every field is a uniform 8-byte word, matching the native
// list/map/struct slot discipline.
//
// Strings are IMMUTABLE, so — unlike lists and maps — a string value needs no
// deep-copy when bound (`let b = a`), passed as an argument, or returned: sharing
// the pointer is already value-equivalent (exactly the interpreters' behavior and
// the WASM backend's, which also never copies a string argument). A string
// therefore crosses a function boundary as a plain pointer word in an integer
// register, never as a by-pointer aggregate.
//
// `len(s)` reads the `char_len` header for ANY string value. Runtime `+`
// concatenation allocates a fresh record, sums the headers, and byte-copies both
// operands' UTF-8 ranges. `to_string` builds a fresh record from an integer/
// bool/char/byte (identity on a string). All records are bump-allocated (no
// reclamation), like every other native heap value.

/// Byte offset of a string record's `char_len` header (Unicode scalar count).
pub(crate) const STR_CHAR_LEN_OFF: i32 = 0;

/// Byte offset of a string record's `byte_len` header (UTF-8 byte length).
pub(crate) const STR_BYTE_LEN_OFF: i32 = 8;

/// Byte offset of a string record's first UTF-8 byte, past the two i64 headers.
pub(crate) const STR_DATA_OFF: i32 = 16;

/// The string-literal materialization helper emitted in `.text`. Signature: a
/// pointer to a NUL-terminated `.rdata` byte string in `rcx`; returns in `rax` a
/// fresh heap string record `[char_len][byte_len][utf8 bytes]` copied from those
/// bytes (the byte length scanned to the terminator, the char count computed by
/// decoding UTF-8 lead bytes). The `.rdata` layout is unchanged (raw
/// NUL-terminated bytes, shared with the `len("literal")` path), so a string
/// literal used as a value materializes through this helper at runtime.
pub(crate) const STR_LIT_SYMBOL: &str = "__lullaby_str_lit";

/// The `string`→`cstr` FFI materialization helper emitted in `.text`. Signature: a
/// heap string record pointer (`[char_len][byte_len][utf8]`) in `rcx`; returns in
/// `rax` a freshly bump-allocated `byte_len + 1` buffer holding the record's UTF-8
/// bytes followed by a NUL terminator — a `const char*` a C function borrows for
/// the duration of the call. Used to pass a Lullaby `string` to an extern's `cstr`
/// parameter. An interior NUL is copied verbatim, so a C reader sees the truncated
/// C-string prefix (standard `char*` semantics), matching the ffi_design contract.
pub(crate) const TO_CSTR_SYMBOL: &str = "__lullaby_to_cstr";

/// The string-concatenation helper emitted in `.text`. Signature: the left
/// string record pointer in `rcx`, the right in `rdx`; returns in `rax` a fresh
/// record whose char/byte headers are the summed operands' headers and whose
/// bytes are the two operands' UTF-8 ranges concatenated.
pub(crate) const STR_CONCAT_SYMBOL: &str = "__lullaby_str_concat";

/// The ownership-aware concat helper: left in `rcx`, right in `rdx`, and a
/// compile-time ownership mask in `r8` (bit 0 = the left operand is a
/// uniquely-owned fresh temporary, bit 1 = the right is). It concatenates (via
/// `__lullaby_str_concat`), then `rc_dec`s each operand the mask marks — reclaiming
/// intermediate string temporaries (e.g. the `to_string(i)` and the literal inside
/// `to_string(i) + "…"`) that would otherwise leak. Emitted only when at least one
/// operand is a fresh temp; a plain `var + var` concat still lowers to the bare
/// `__lullaby_str_concat`.
pub(crate) const STR_CONCAT_OWN_SYMBOL: &str = "__lullaby_str_concat_own";

/// The ownership-aware `len` helper: a string record pointer in `rcx` that is a
/// uniquely-owned fresh temporary. Reads the `char_len` header, then `rc_dec`s the
/// record (reclaiming it), and returns the length in `rax`. Lets `len(<fresh
/// temp>)` — e.g. `len(to_string(i))`, `len(a + b)`, `len(substring(…))` — reclaim
/// the temporary that `len` would otherwise read and leak. A `len` on a borrowed
/// string value keeps the plain header read.
pub(crate) const STR_LEN_OWN_SYMBOL: &str = "__lullaby_str_len_own";

/// The ownership-aware two-string-op helper: left in `rcx`, right in `rdx`, a
/// compile-time ownership mask in `r8` (bit 0 = left is a fresh temp, bit 1 =
/// right), and the target op's address in `r9`. It calls the op (an indirect
/// `call r9`, forwarding `left`/`right`), then `rc_dec`s each operand the mask
/// marks, and returns the op's result — reclaiming fresh-temp arguments to the
/// borrow-only two-string builtins (`find`/`count`/`contains`/`starts_with`/
/// `ends_with`) and to `split`/`join`. Emitted only when an operand is a fresh
/// temp; a `var`/`var` call keeps the bare op with zero overhead.
pub(crate) const STR_BINOP_OWN_SYMBOL: &str = "__lullaby_str_binop_own";

/// The ownership-aware string-read helper: a fresh-temp source string in `rcx`,
/// the op's other (scalar) arguments already in `rdx`/`r8`, and the op's address in
/// `r9`. It calls the op (`call r9`, forwarding `rcx`/`rdx`/`r8`), then `rc_dec`s
/// the source, and returns the op's single result in `rax` — reclaiming a fresh
/// temporary passed to `substring`/`char_at`/`repeat`/`trim` (each reads the source
/// and produces an independent new value, so the source is dead afterwards).
pub(crate) const STR_READ_OWN_SYMBOL: &str = "__lullaby_str_read_own";

/// The integer-to-string helper emitted in `.text`. Signature: a signed 64-bit
/// value in `rcx` and a signedness flag in `rdx` (0 = format as unsigned `u64`,
/// nonzero = format as signed `i64`); returns in `rax` a fresh string record of
/// the decimal digits (a leading `-` for a negative signed value). Matches the
/// interpreters' `Display` for `i64`/fixed-width integers (`byte` uses the
/// unsigned path).
pub(crate) const STR_FROM_INT_SYMBOL: &str = "__lullaby_str_from_int";

/// The bool-to-string helper emitted in `.text`. Signature: a 0/1 flag in `rcx`;
/// returns in `rax` a fresh string record holding `"true"` or `"false"`.
pub(crate) const STR_FROM_BOOL_SYMBOL: &str = "__lullaby_str_from_bool";

/// The char-to-string helper emitted in `.text`. Signature: a Unicode scalar
/// value (code point) in `rcx`; returns in `rax` a fresh single-character string
/// record holding that code point's UTF-8 encoding (1–4 bytes, `char_len = 1`).
pub(crate) const STR_FROM_CHAR_SYMBOL: &str = "__lullaby_str_from_char";

/// The char-indexed `substring` helper emitted in `.text`. Signature: the source
/// string record pointer in `rcx`, the `start` char index (i64) in `rdx`, the
/// `end` char index (i64) in `r8`; returns in `rax` a fresh `[char_len][byte_len]
/// [utf8]` record holding the half-open `[start, end)` char slice. On an
/// out-of-bounds range (`start < 0 || end < 0 || start > end || end > char_count`)
/// it traps with `ud2`, mirroring the interpreters' `L0413`.
pub(crate) const STR_SUBSTRING_SYMBOL: &str = "__lullaby_str_substring";

/// The char-index helper emitted in `.text`. Signature: the source string record
/// pointer in `rcx`, the char index (i64) in `rdx`; returns in `rax` the Unicode
/// code point of the `i`-th character (an `i64` `char` cell). On an out-of-range
/// index (`i < 0 || i >= char_count`) it traps with `ud2`, mirroring `L0413`.
/// Implements `s[i]`.
pub(crate) const STR_CHAR_AT_SYMBOL: &str = "__lullaby_str_char_at";

/// The substring-count helper emitted in `.text`. Signature: the haystack record
/// pointer in `rcx`, the needle record pointer in `rdx`; returns in `rax` the count
/// of NON-overlapping byte-level occurrences of the needle (matching the
/// interpreters' `text.matches(sub).count()`). An empty needle yields `0`.
pub(crate) const STR_COUNT_SYMBOL: &str = "__lullaby_str_count";

/// The string-repeat helper emitted in `.text`. Signature: the source record
/// pointer in `rcx`, the repeat count (i64) in `rdx`; returns in `rax` a fresh
/// record that is the source concatenated `count` times (`count <= 0` yields the
/// empty string), matching the interpreters' `text.repeat(count)`.
pub(crate) const STR_REPEAT_SYMBOL: &str = "__lullaby_str_repeat";

/// The string-trim helper emitted in `.text`. Signature: the source record pointer
/// in `rcx`; returns in `rax` a fresh record with leading/trailing ASCII
/// whitespace removed (matching `trim_matches(is_ascii_whitespace)`). Computes the
/// trimmed byte bounds and delegates to `__lullaby_str_substring` (byte offsets ==
/// char indices for the ASCII strings the native subset builds).
pub(crate) const STR_TRIM_SYMBOL: &str = "__lullaby_str_trim";

/// The `upper`/`lower` helpers emitted in `.text`. Signature: the source record
/// pointer in `rcx`; returns in `rax` a fresh record with each ASCII letter
/// upper-/lower-cased (non-letters copied verbatim). The native string subset is
/// ASCII, so a byte-wise ASCII case fold matches the interpreters' `to_uppercase`/
/// `to_lowercase` (which are length-preserving on ASCII) exactly.
pub(crate) const STR_UPPER_SYMBOL: &str = "__lullaby_str_upper";
pub(crate) const STR_LOWER_SYMBOL: &str = "__lullaby_str_lower";

/// The `find` helper emitted in `.text`. Signature: the haystack record pointer
/// in `rcx`, the needle record pointer in `rdx`; returns in `rax` the CHAR index
/// (i64) of the first byte-level occurrence of the needle, or `-1` if absent. An
/// empty needle yields `0`. Matches the interpreters' `char_find`.
pub(crate) const STR_FIND_SYMBOL: &str = "__lullaby_str_find";

/// The `contains` helper emitted in `.text`. Signature: the string record pointer
/// in `rcx`, the substring record pointer in `rdx`; returns `0`/`1` (bool) in
/// `rax`. An empty substring is contained. Byte-exact, matching the interpreters.
pub(crate) const STR_CONTAINS_SYMBOL: &str = "__lullaby_str_contains";

/// The `starts_with` helper emitted in `.text`. Signature: the string record
/// pointer in `rcx`, the prefix record pointer in `rdx`; returns `0`/`1` (bool) in
/// `rax`. An empty prefix matches; a longer-than-haystack prefix does not.
pub(crate) const STR_STARTS_WITH_SYMBOL: &str = "__lullaby_str_starts_with";

/// The `ends_with` helper emitted in `.text`. Signature: the string record
/// pointer in `rcx`, the suffix record pointer in `rdx`; returns `0`/`1` (bool) in
/// `rax`. An empty suffix matches; a longer-than-haystack suffix does not.
pub(crate) const STR_ENDS_WITH_SYMBOL: &str = "__lullaby_str_ends_with";

/// The `parse_i64` helper emitted in `.text`. Signature: the source string record
/// pointer in `rcx`; returns the `result<i64, string>` variant tag in `rax`
/// (`0` = `ok`, `1` = `err`) and the payload in `rdx` (the parsed `i64` on `ok`,
/// or a freshly-allocated error-message string record on `err`). The parse matches
/// Rust's `str::parse::<i64>()` exactly: an optional single leading `+`/`-`, then
/// one or more ASCII digits, no surrounding whitespace, and a checked base-10
/// accumulation so an out-of-range value is an `err`. The error message is the same
/// fixed `` cannot parse `{text}` as i64 `` the interpreters produce.
pub(crate) const PARSE_I64_SYMBOL: &str = "__lullaby_parse_i64";

/// The `split` helper emitted in `.text`. Signature: the text record pointer in
/// `rcx`, the separator record pointer in `rdx`; returns in `rax` a fresh
/// `list<string>`-layout block (`[len][cap][slot…]`) of the fields, matching the
/// interpreters' `text.split(sep)` (leading/trailing/consecutive separators yield
/// empty fields; an empty input yields one empty field). Composed from the tested
/// `__lullaby_str_count`/`_find`/`_substring` helpers. An empty separator traps
/// with `ud2` (the interpreters' `L0417`).
pub(crate) const STR_SPLIT_SYMBOL: &str = "__lullaby_str_split";

/// The `join` helper emitted in `.text`. Signature: an `array<string>`
/// (`list<string>`-layout) block pointer in `rcx`, the separator record pointer in
/// `rdx`; returns in `rax` a fresh record joining the fields with the separator
/// between them, matching the interpreters' `parts.join(sep)`. An empty array
/// yields the empty string.
pub(crate) const STR_JOIN_SYMBOL: &str = "__lullaby_str_join";

/// Whether `ty` is the native heap `string` type. A string value is a single
/// pointer word (like a list/map) but immutable, so it needs no deep copy.
pub(crate) fn is_string_type(ty: &TypeRef) -> bool {
    ty.name == "string"
}

/// `.rdata` section characteristics: initialized, read-only data.
/// `IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ`.
pub(crate) const RDATA_CHARACTERISTICS: u32 = 0x4000_0040;

/// `.bss` section characteristics: uninitialized data, read + write.
/// `IMAGE_SCN_CNT_UNINITIALIZED_DATA | IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE`.
pub(crate) const BSS_CHARACTERISTICS: u32 = 0xC000_0080;

/// COFF relocation type for a 32-bit PC-relative reference to a symbol, used for
/// `call rel32` and `jmp rel32` targeting another symbol (`IMAGE_REL_AMD64_REL32`).
pub(crate) const IMAGE_REL_AMD64_REL32: u16 = 0x0004;

/// COFF relocation type for a 32-bit offset of a symbol from the start of its own
/// section (`IMAGE_REL_AMD64_SECREL`). Used by CodeView `DEBUG_S_LINES`
/// subsections to point at a function's `.text` offset.
pub(crate) const IMAGE_REL_AMD64_SECREL: u16 = 0x000B;

/// COFF relocation type for the 16-bit section index of a symbol
/// (`IMAGE_REL_AMD64_SECTION`). Paired with `SECREL` in CodeView line subsections.
pub(crate) const IMAGE_REL_AMD64_SECTION: u16 = 0x000A;

/// `.debug$S` section characteristics: initialized data, read-only, discardable,
/// 1-byte aligned. `IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ |
/// IMAGE_SCN_MEM_DISCARDABLE | IMAGE_SCN_ALIGN_1BYTES`.
pub(crate) const DEBUG_S_CHARACTERISTICS: u32 = 0x4210_0040;

/// CodeView `.debug$S` section signature (`CV_SIGNATURE_C13`).
pub(crate) const CV_SIGNATURE_C13: u32 = 4;

/// CodeView subsection kind: a symbol subsection (`DEBUG_S_SYMBOLS`).
pub(crate) const DEBUG_S_SYMBOLS: u32 = 0xF1;
/// CodeView subsection kind: a line-number subsection (`DEBUG_S_LINES`).
pub(crate) const DEBUG_S_LINES: u32 = 0xF2;
/// CodeView subsection kind: the source-file checksum table (`DEBUG_S_FILECHKSMS`).
pub(crate) const DEBUG_S_FILECHKSMS: u32 = 0xF4;
/// CodeView subsection kind: the string table (`DEBUG_S_STRINGTABLE`).
pub(crate) const DEBUG_S_STRINGTABLE: u32 = 0xF3;

/// CodeView symbol record kind `S_COMPILE3` (compiler/environment record). A
/// minimal one is emitted so the `.debug$S` stream is a well-formed CodeView
/// symbol subsection.
pub(crate) const S_COMPILE3: u16 = 0x113C;
