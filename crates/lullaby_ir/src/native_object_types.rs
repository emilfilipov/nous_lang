//! Native type model and resolution: `NativeType`/`NativeEnumVariant`/`FloatWidth`
//! and their impls, resolution of source `TypeRef`s and enum definitions into
//! native layout types, and the array-length bookkeeping types. Split out of
//! native_object.rs; sees the parent's items via `use super::*`.

use super::*;

// Reused from semantics to substitute a generic type's type parameters with the
// concrete type arguments of an instantiation (`{T: i64}` applied to `list<T>` ->
// `list<i64>`). Recurses through nested generic spellings, so a monomorphized
// field/payload type is fully concrete before its native layout is computed.
use lullaby_semantics::substitute_type;

// -- Stack aggregate layout (all-i64 structs and fixed i64 arrays) -----------
//
// Locals in the extended subset may be an `i64` scalar, an all-i64 (possibly
// nested) struct, or a fixed-length array of any supported element type. Each
// such value is laid out contiguously in the function's stack frame as a run of
// 8-byte words: scalars occupy one word, a struct the concatenation of its
// (recursively flattened) field words, and an array `len` copies of its element
// layout. Aggregates never live in a register; instead operations resolve the
// `[rbp - slot]` displacement of an individual scalar word and load/store it.

/// The stack layout of a native local value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NativeType {
    /// **No value at all** — the return layout of a function that declares no
    /// return type (`fn poke p ptr<i64> v i64`). This is the ABSENCE of a return
    /// value, not an unsupported one: the body runs for its effects and the
    /// epilogue returns without writing a result, so `rax` is UNDEFINED on return
    /// and the caller must not read it.
    ///
    /// `Void` is admitted in **return position only** — via
    /// [`resolve_return_native_type`], never [`resolve_signature_native_type`] or
    /// [`resolve_native_type`] — so a `void` parameter, local, field, element, or
    /// enum payload still fails to resolve and its function skips cleanly
    /// (`L0339`). It is deliberately NOT an aggregate ([`NativeType::is_aggregate`]
    /// returns false), so it reserves no hidden result pointer and no return
    /// scratch, and it occupies zero words.
    Void,
    /// A single 8-byte integer word.
    I64,
    /// A single 8-byte word holding an IEEE-754 `f64` (double). Lives in an XMM
    /// register as a `double` while live; spilled to its stack word as 8 bytes.
    F64,
    /// A single 8-byte word holding an IEEE-754 `f32` (single). Only the low four
    /// bytes are meaningful; the value is kept rounded to single precision after
    /// every operation, matching the interpreter's real `f32` storage.
    F32,
    /// A heap `string` value: a single 8-byte word holding a pointer to a
    /// `[char_len i64][byte_len i64][utf8 bytes]` record. It passes/returns in an
    /// integer register (by value, like a pointer). Unlike a list/map it is
    /// IMMUTABLE, so it needs no deep copy on a binding, argument, or return —
    /// sharing the pointer is value-equivalent, matching the interpreters and the
    /// WASM backend.
    String,
    /// A named struct whose fields are all supported native types, in order.
    Struct {
        name: String,
        fields: Vec<(String, NativeType)>,
    },
    /// A named struct laid out **on the heap**: a single 8-byte word holding a
    /// pointer to a `[field words...]` block (one 8-byte word per flattened field,
    /// in declared order). This is the representation a struct takes when it is a
    /// MUTABLE-heap collection element/value/enum payload (`list<struct>`,
    /// `map<K, struct>`, `option<struct>`) — the element slot is one word, so the
    /// struct value must be a pointer. Distinct from [`NativeType::Struct`], which
    /// is the stack-flattened representation used for struct locals/params/returns.
    /// The collection copy paths DEEP-COPY a `HeapStruct` element per value-semantic
    /// copy (mirroring the interpreters' recursive `Value::clone`), and `get`/`match`
    /// bridge a `HeapStruct` back into the stack-flattened `Struct` layout that field
    /// access and the by-pointer call ABI expect. `fields` are the flattened field
    /// (name, scalar/string/heap-aggregate layout) pairs, one word each.
    HeapStruct {
        name: String,
        fields: Vec<(String, NativeType)>,
    },
    /// A **packed narrow integer** — 1, 2, or 4 bytes of storage holding an
    /// `i8`/`i16`/`i32`/`u8`/`u16`/`u32`/`byte` value at its C-natural width.
    ///
    /// This is the ONLY place the native backend departs from the normalized
    /// 8-byte cell, and it appears in exactly one position: the ELEMENT type of a
    /// fixed [`NativeType::Array`]. A narrow *scalar* (a local, parameter, field,
    /// list element, or enum payload) is still a full sign-extended `I64` cell —
    /// see [`resolve_native_type`], which keeps mapping `i32` and friends to
    /// [`NativeType::I64`]. Only [`narrow_array_element`] produces a `Narrow`.
    ///
    /// # Why it exists
    ///
    /// The interpreters DEFINE a narrow-element buffer walk: for an `array<i32>`,
    /// `addr_of(a[0])` + `ptr_offset(p, 1)` names element 1 and the size law
    /// answers `size_of(i32) == 4` (`RawPointerMemory`'s region stride is
    /// `size_of(element)`; see `raw_pointer.rs` and the `raw_ptr_addressing.lby`
    /// fixture, which exits 18 on all three interpreters). With 8-byte element
    /// cells the native answer would be 8 — a *different answer to a defined
    /// program*, which is exactly what the backend must never do. Packing the
    /// element to its C width makes the native stride and the interpreters' stride
    /// the same number, and makes the buffer C-compatible for the kernel tier.
    ///
    /// # Why a distinct variant rather than a width field on `Array`
    ///
    /// Every `match` on `NativeType` in this backend is exhaustive (there is not a
    /// single `_ =>` arm), and every fast path that assumes an 8-byte lane gates on
    /// `NativeType::I64` / `NativeType::F64` specifically — the four SIMD bases in
    /// `native_object_simd.rs`, the strict `resolve_place_steps` i64 resolver, the
    /// reduction detectors. Giving a narrow element its own variant makes all of
    /// them **fail closed by construction**: they stop matching, the shape is not
    /// recognized, and the function falls back to the scalar path or skips cleanly.
    /// A `bytes` field on `Array` would leave `elem` reading `I64` and let
    /// `paddq`-based vectorization run over 4-byte lanes — silent corruption. The
    /// exhaustiveness also means the compiler enumerates every site that must make
    /// a decision about narrow elements, rather than leaving it to a grep.
    Narrow { bytes: usize, signed: bool },
    /// A fixed-length array of a supported element type.
    ///
    /// Elements are laid out **contiguously at their element type's own width**:
    /// element `k` begins at byte `k * elem.byte_size()` from the array's element 0,
    /// ascending. For an 8-byte element that is the historical word layout,
    /// unchanged. For a [`NativeType::Narrow`] element the array is PACKED, exactly
    /// like the C array it mirrors.
    ///
    /// The array as a whole still occupies a whole number of 8-byte words and is
    /// word-aligned (see [`NativeType::words`]): packing is a property of the
    /// element stride *within* the array's own byte span, not of the surrounding
    /// frame or struct layout. That keeps every word-granular copy path (aggregate
    /// copy-in, the by-hidden-pointer return ABI, `alloc_scratch`) correct and
    /// untouched — a copy moves `words()` whole words, which covers the packed
    /// bytes plus at most 7 bytes of tail padding that no element access ever
    /// reads.
    Array { elem: Box<NativeType>, len: usize },
    /// A **fat-pointer** `array<T>` parameter: a `(data_ptr, length)` descriptor
    /// occupying two frame words — word 0 is a pointer to the caller's element 0
    /// (the array's LOWEST stack address; elements ASCEND from there like a stack
    /// array, so the data pointer is C-shaped), word 1 is the runtime element
    /// count (at the descriptor's own `slot - 8`). Used for a **read-only**
    /// scalar-element array parameter whose length is not known at compile time, so
    /// the callee reads the caller's storage in place (no array-body copy) instead
    /// of demoting because the length could not be inferred from a call site. The
    /// descriptor crosses the call boundary by pointer, exactly like an aggregate
    /// (one register/stack argument slot), but the pointer it carries is the shared
    /// data pointer rather than a fresh copy. Value semantics hold ONLY because the
    /// parameter is read-only (the body never assigns `a[i]`); a mutating array
    /// parameter still uses the copy-in [`NativeType::Array`] path. `elem` is the
    /// (scalar) element layout, so `a[i]` and the runtime length drive the element
    /// stride and bounds check.
    FatArray { elem: Box<NativeType> },
    /// A growable `list<T>` with a scalar element type. Represented as a single
    /// 8-byte word holding a heap pointer to a `[len i64][cap i64][slots]` block;
    /// it passes/returns in an integer register (by value, like a pointer) and is
    /// value-semantic because its mutators deep-copy their source (see the
    /// "Growable list layout (native)" comment). `elem` is the (scalar) element
    /// layout, used to keep the element word count exact and mirror the WASM
    /// backend.
    List { elem: Box<NativeType> },
    /// A growable `map<K, V>` with scalar key/value types. Represented as a single
    /// 8-byte word holding a heap pointer to a `[len i64][cap i64][entries]`
    /// block (each entry a `(key, value)` word pair); it passes/returns in an
    /// integer register (by value, like a pointer) and is value-semantic because
    /// its only mutator (`map_set`) deep-copies its source. `key`/`value` are the
    /// scalar element layouts, used to keep the value slot exact and mirror the
    /// WASM backend.
    Map {
        key: Box<NativeType>,
        value: Box<NativeType>,
    },
    /// A tagged enum whose variants all carry scalar payloads. Laid out as one
    /// tag word (the variant's discriminant index) followed by
    /// `payload_words` payload words (the maximum payload width across the
    /// variants). Each variant records its ordered scalar payload words for
    /// construction and `match` binding. The discriminant of a variant is its
    /// index in `variants`, matching the order the IR/interpreters use for that
    /// enum (declared order for a user enum; `some,none` / `ok,err` for the
    /// built-in generics).
    Enum {
        name: String,
        variants: Vec<NativeEnumVariant>,
        /// Max payload words across all variants (the payload region size).
        payload_words: usize,
    },
}

/// One variant of a native enum layout: its name, its discriminant index (the
/// tag value), and its ordered scalar payload word layouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeEnumVariant {
    pub(crate) name: String,
    pub(crate) tag: i64,
    pub(crate) payload: Vec<NativeType>,
}

impl NativeType {
    /// Whether this is an aggregate (struct / fixed array / enum) — a value that
    /// crosses a native function boundary **by pointer** — rather than a scalar
    /// (`i64`/fixed-width/`bool`/`char`/`byte`/`f64`/`f32`) that passes in a
    /// register.
    pub(crate) fn is_aggregate(&self) -> bool {
        matches!(
            self,
            NativeType::Struct { .. } | NativeType::Array { .. } | NativeType::Enum { .. }
        )
    }

    /// The size of this value **in bytes** when it sits as an array element.
    ///
    /// This is the element stride: `size_of(T)` for a [`NativeType::Narrow`], and
    /// `8 * words()` for everything else (an 8-byte cell, or an aggregate element
    /// occupying whole words). It is always a power of two for the element types
    /// that can actually appear in a fixed array's element position, which
    /// `emit_dynamic_addr_into_rcx` relies on to keep the index scaling a `shl`.
    pub(crate) fn byte_size(&self) -> usize {
        match self {
            NativeType::Narrow { bytes, .. } => *bytes,
            other => other.words() * 8,
        }
    }

    /// The number of 8-byte words this value occupies on the stack.
    pub(crate) fn words(&self) -> usize {
        match self {
            // No value: no storage. `Void` only ever appears as a RETURN layout
            // (never a local/param/field), so this arm is a sizing identity rather
            // than a live stack reservation — a void return reserves no return
            // scratch (it is not an aggregate) and no hidden result pointer.
            NativeType::Void => 0,
            // A string, list, map, or heap struct is a single pointer word, like a
            // scalar.
            NativeType::I64
            | NativeType::F64
            | NativeType::F32
            | NativeType::String
            | NativeType::List { .. }
            | NativeType::Map { .. }
            | NativeType::HeapStruct { .. } => 1,
            // A packed narrow element is a sub-word quantity, so it has no
            // standalone word count. It only ever appears as an array element, and
            // the `Array` arm below sizes the packed span directly rather than
            // multiplying a per-element word count — so this arm is never consulted
            // for a live stack reservation. Answering 1 (rather than panicking)
            // keeps it a conservative sizing identity: a narrow value never needs
            // MORE than one word.
            NativeType::Narrow { .. } => 1,
            NativeType::Struct { fields, .. } => fields.iter().map(|(_, t)| t.words()).sum(),
            // The packed span rounded UP to whole words. The array is word-aligned
            // and owns its tail padding, so every word-granular copy path stays
            // correct: `array<u8>` of length 3 is 3 bytes of live elements inside a
            // 1-word (8-byte) reservation, and copying that 1 word moves the 3 live
            // bytes plus 5 bytes of padding no element access reads. An 8-byte
            // element reduces to the historical `words * len`, bit-for-bit.
            NativeType::Array { elem, len } => (elem.byte_size() * len).div_ceil(8),
            // A fat-pointer array descriptor: one data-pointer word plus one
            // length word.
            NativeType::FatArray { .. } => 2,
            // One tag word plus the shared payload region.
            NativeType::Enum { payload_words, .. } => 1 + payload_words,
        }
    }
}

/// The precision of a float value kept in an XMM register: an f64 `double` or an
/// f32 `single`. Selects the scalar SSE opcode family (`*sd` vs `*ss`) and drives
/// f32 single-precision rounding after each op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FloatWidth {
    F64,
    F32,
}

impl FloatWidth {
    /// The `FloatWidth` named by a Lullaby type name, or `None` for a non-float
    /// type. `f64`/`f32` are the only float types in the language.
    pub(crate) fn from_type_name(name: &str) -> Option<FloatWidth> {
        match name {
            "f64" => Some(FloatWidth::F64),
            "f32" => Some(FloatWidth::F32),
            _ => None,
        }
    }
}

/// Resolve a declared `TypeRef` into a `NativeType`. Arrays are not resolvable
/// from the type alone (their length is not encoded in `array<T>`); array
/// locals derive their layout from their initializer instead, so a bare
/// `array<...>` type reaching here is an error the caller turns into a skip.
pub(crate) fn resolve_native_type(
    ty: &TypeRef,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
) -> Result<NativeType, String> {
    match ty.name.as_str() {
        "i64" => Ok(NativeType::I64),
        // A fixed-width integer (`i8`…`usize`) is stored as its normalized `i64`
        // cell, so it occupies exactly one 8-byte word like `i64`.
        name if fixed_int_kind(name).is_some() => Ok(NativeType::I64),
        // `bool`/`char`/`byte` are single normalized `i64` cells (0/1 for bool,
        // the code point for char, the byte value for byte), so each occupies one
        // 8-byte word. These reach here only as enum payload types; a scalar local
        // of these types never selects this path (the frontend types them
        // directly), but sizing them here keeps the enum payload word count exact.
        "bool" | "char" | "byte" => Ok(NativeType::I64),
        // `f64`/`f32` each occupy one 8-byte word. An f32 keeps only its low four
        // bytes meaningful but is stored in a full word for uniform layout.
        "f64" => Ok(NativeType::F64),
        "f32" => Ok(NativeType::F32),
        // A heap `string`: a single pointer word to a `[char_len][byte_len][utf8]`
        // record. Immutable, so it passes/returns by value (pointer) with no copy.
        "string" => Ok(NativeType::String),
        // A raw pointer `ptr<T>` (or the legacy `ptr_T` spelling that `alloc`
        // produces) is a single 64-bit machine-address word. It flows through the
        // native backend exactly like an `i64` scalar — one stack word, passed and
        // returned in a GPR — so modeling it as `I64` reuses every scalar path
        // unchanged. Its only distinguished behavior is at the FFI boundary, where
        // it marshals to a C `T*` (see `emit_extern_call`).
        name if is_raw_pointer_type_name(name) => Ok(NativeType::I64),
        // A heap-backed `array<string>` — the `split`/`words` result and
        // `array<string>` literals — is a single pointer word to a `[len][cap]
        // [slot…]` block of shared immutable string pointers, laid out and
        // deep-copied exactly like a `list<string>`. (Scalar `array<i64>`/
        // `array<f64>` stay stack-flattened with a statically-inferred length,
        // handled by the `array<` arm below / the signature length path.)
        _ if heap_string_array_element(ty).is_some() => Ok(NativeType::List {
            elem: Box::new(NativeType::String),
        }),
        name if name.starts_with("array<") => Err(format!(
            "array length for `{name}` is unknown from its type"
        )),
        // A growable `list<T>` (scalar `T`): a single pointer word. The element
        // must be a native scalar; a heap element (`list<string>` etc.) is
        // deferred and rejected so the enclosing function skips gracefully.
        name if name.starts_with("list<") => {
            let elem = supported_list_element(ty).ok_or_else(|| {
                format!(
                    "list element of `{name}` is not a native scalar, `string`, \
                     one-level struct, or one-level nested list \
                     (deeper nesting and map/array elements are deferred)"
                )
            })?;
            // Depth-0 element classification: a scalar/string flat word, a struct
            // (`HeapStruct`), or a nested list — bounded to one mutable level.
            let elem_native = native_collection_slot(&elem, structs, enums, 0)
                .ok_or_else(|| format!("list element of `{name}` is not layable-out (deferred)"))?;
            Ok(NativeType::List {
                elem: Box::new(elem_native),
            })
        }
        // A growable `map<K, V>` (scalar key/value): a single pointer word. The key
        // must be an integer-cell scalar and the value a native scalar; a heap
        // key/value or float key is deferred and rejected so the enclosing
        // function skips gracefully.
        name if name.starts_with("map<") => {
            let (key, value) = supported_map_kv(ty).ok_or_else(|| {
                format!(
                    "map `{name}` key/value is not a supported native type \
                     (heap keys and float keys are deferred)"
                )
            })?;
            let key_native = resolve_native_type(&key, structs, enums)?;
            // Value classification: a scalar/string flat word, a struct
            // (`HeapStruct`), or a nested list — bounded to one mutable level.
            let value_native = native_collection_slot(&value, structs, enums, 0)
                .ok_or_else(|| format!("map value of `{name}` is not layable-out (deferred)"))?;
            Ok(NativeType::Map {
                key: Box::new(key_native),
                value: Box::new(value_native),
            })
        }
        // The built-in generic enums and user enums (generic or not) with scalar
        // payloads. A user generic enum instantiation (`Opt<i64>`) is monomorphized
        // inside `resolve_enum_type` by substituting the spelling's type arguments.
        name if is_enum_type_name(name, enums) => resolve_enum_type(ty, structs, enums),
        name => {
            // A non-generic struct declared under its exact name.
            if let Some(def) = structs.iter().find(|s| s.name == name) {
                return resolve_struct_fields(name, &def.fields, structs, enums);
            }
            // A user-generic struct instantiation `Name<args>` (`Box<i64>`,
            // `Pair<i64, bool>`): MONOMORPHIZE it. Look up the generic declaration by
            // its head name, zip its declared type parameters against the spelling's
            // concrete type arguments, substitute them into each field type, then
            // resolve the resulting concrete layout with the ordinary struct
            // machinery. The native layout of a monomorphized `Box<i64>` is identical
            // to a hand-written `struct BoxI64 { value i64 }`, so this is
            // value-neutral. The struct keeps its BASE name (`Box`) in the layout so
            // the constructor-name check (`Box(...)`) and all downstream paths match.
            if let Some((head, args)) = split_user_generic(ty)
                && let Some(def) = structs.iter().find(|s| s.name == head)
                && !def.type_params.is_empty()
            {
                let subst = subst_map(&def.type_params, &args);
                let concrete_fields: Vec<(String, TypeRef)> = def
                    .fields
                    .iter()
                    .map(|(fname, fty)| (fname.clone(), substitute_type(fty, &subst)))
                    .collect();
                let native = resolve_struct_fields(&head, &concrete_fields, structs, enums)?;
                // Native-supported-layout scope gate (default-deny), mirroring the
                // non-generic heap-field aggregate boundary. A monomorphized generic
                // struct compiles natively when its whole layout is scalar-only OR its
                // fields are scalars plus one-level immutable `string` words (a heap
                // type argument like `Box<string>` -> a `string` field, or
                // `Pair<string, i64>`). Such a `string` field is stored/copied/dropped
                // exactly as a hand-written string-field struct (shared immutable word;
                // the recursive `rc_dec` drop-glue reclaims it, composing with RC +
                // arena). A DEEPER heap shape is still DEFERRED: a mutable heap field
                // (`Stack<i64>` -> a `list<i64>` field), a nested heap-carrying
                // aggregate, or a two-level `string` nesting — exactly what the
                // non-generic path also defers. Skipping here keeps native default-deny.
                if !is_native_supported_generic_layout(&native) {
                    return Err(format!(
                        "generic struct instantiation `{name}` has a deeper-than-one-level heap \
                         field after substitution (a mutable heap value or a nested heap-carrying \
                         aggregate); it is deferred to the interpreters"
                    ));
                }
                return Ok(native);
            }
            Err(format!("type `{name}` is not in the native stack subset"))
        }
    }
}

/// Split a `TypeRef` spelling into `(head, args)` when it is a user-generic
/// instantiation `Name<args>` (`Box<i64>` -> `("Box", ["i64"])`). Returns `None`
/// for a plain type name, a function type (`fn(...) -> R`), or a spelling that is
/// not `<...>`-bracketed. Built-in generics (`list<...>`, `option<...>`, ...) are
/// handled by earlier arms of [`resolve_native_type`] and never reach this helper.
fn split_user_generic(ty: &TypeRef) -> Option<(String, Vec<TypeRef>)> {
    let open = ty.name.find('<')?;
    if !ty.name.ends_with('>') || ty.name.starts_with("fn(") {
        return None;
    }
    let head = ty.name[..open].to_string();
    let args = ty.generic_args(&head)?;
    Some((head, args))
}

/// Build the substitution map that pins each declared generic type parameter to
/// its concrete type argument (`["T"]` + `["i64"]` -> `{T: i64}`). Semantics has
/// already validated arity; a shorter argument list (which cannot occur for a
/// checked program) simply leaves the surplus parameters unbound so their field
/// types fail to resolve and the function skips.
fn subst_map(type_params: &[String], args: &[TypeRef]) -> HashMap<String, TypeRef> {
    type_params
        .iter()
        .cloned()
        .zip(args.iter().cloned())
        .collect()
}

/// Whether a resolved native layout is composed ENTIRELY of scalars — an `i64`/
/// fixed-width/`bool`/`char`/`byte` cell, an `f64`/`f32`, or a struct/array/enum
/// whose fields/elements/payloads are themselves scalar-only. Any heap word
/// (`string`, `list`, `map`, a heap struct, or a fat-array descriptor) makes it
/// non-scalar. This is the scalar-`T` scope gate for generic monomorphization: a
/// monomorphized generic type compiles natively only when its whole layout is
/// scalar-only, so heap type arguments (and scalar arguments that still produce a
/// heap-typed field, like `Stack<i64>`'s `list<i64>`) are deferred cleanly.
pub(crate) fn is_scalar_only_layout(ty: &NativeType) -> bool {
    match ty {
        NativeType::I64 | NativeType::F64 | NativeType::F32 => true,
        // A packed narrow array element IS a scalar — an integer at its C width,
        // carrying no heap word — so a narrow-element array is a scalar-only layout
        // and stays inside the generic-monomorphization scope, exactly like the
        // `array<i64>` it now sits beside.
        NativeType::Narrow { .. } => true,
        // `Void` is a RETURN-only layout and can never be a field/element/payload,
        // so this arm is unreachable through the generic-monomorphization gate that
        // calls it. `false` is the default-deny answer regardless: "no value" is not
        // a scalar, so a hypothetical void-carrying layout defers rather than
        // compiling to a zero-word field.
        NativeType::Void => false,
        NativeType::String
        | NativeType::List { .. }
        | NativeType::Map { .. }
        | NativeType::HeapStruct { .. }
        | NativeType::FatArray { .. } => false,
        NativeType::Struct { fields, .. } => fields.iter().all(|(_, f)| is_scalar_only_layout(f)),
        NativeType::Array { elem, .. } => is_scalar_only_layout(elem),
        NativeType::Enum { variants, .. } => variants
            .iter()
            .all(|v| v.payload.iter().all(is_scalar_only_layout)),
    }
}

/// The native-supported-layout gate for a monomorphized user-generic instantiation
/// (`Box<string>`, `Pair<string, i64>`, `Opt<string>`, `Either<i64, string>`). It
/// WIDENS [`is_scalar_only_layout`] to the exact boundary the non-generic heap-field
/// aggregate path supports: a scalar-only layout, OR a `struct`/`enum` whose
/// **immediate** fields/variant payloads are scalars plus **one-level** immutable
/// `string` words. A one-level `string` field/payload is a shared immutable pointer
/// word — stored, value-copied, and reclaimed (by the recursive `rc_dec` drop-glue
/// composing with the arena rewind) exactly as a hand-written string-field
/// struct/enum. Anything DEEPER stays deferred (default-deny), matching what the
/// non-generic path also defers:
/// - a MUTABLE heap field/payload (`list`/`map`, i.e. `Stack<i64>`'s `list<i64>`);
/// - a nested heap-carrying aggregate (`HeapStruct`, a struct/enum used as a
///   collection/payload slot) — e.g. a nested generic aggregate;
/// - a two-level `string` nesting (a `string` inside a nested aggregate field).
///
/// The gate keys purely on the resolved layout, so a monomorphized `Box<string>`
/// (layout `Struct { name: "Box", fields: [("value", String)] }`) is accepted
/// identically to a hand-written `struct { value string }`, which is why the whole
/// existing heap-field machinery (construction, flat-word value copy, the
/// `is_owning_struct_with_strings` recursive drop-glue, arena escape analysis)
/// applies unchanged and the result stays value-neutral.
pub(crate) fn is_native_supported_generic_layout(ty: &NativeType) -> bool {
    // A whole scalar-only layout is always supported (the scalar-`T` case).
    if is_scalar_only_layout(ty) {
        return true;
    }
    match ty {
        // Immediate fields may add one-level immutable `string` words; any nested
        // aggregate field must itself be scalar-only (no deeper `string`).
        NativeType::Struct { fields, .. } => {
            fields.iter().all(|(_, f)| is_one_level_string_or_scalar(f))
        }
        // Immediate variant payloads may add one-level immutable `string` words.
        NativeType::Enum { variants, .. } => variants
            .iter()
            .all(|v| v.payload.iter().all(is_one_level_string_or_scalar)),
        _ => false,
    }
}

/// A field/payload slot that a monomorphized generic instantiation may carry at the
/// TOP level: an immutable `string` word, or an entirely scalar-only cell/aggregate.
/// A nested aggregate must be scalar-only here (a `string` reachable only through a
/// nested aggregate is a two-level nesting and stays deferred), and a mutable heap
/// value (`list`/`map`/`HeapStruct`/`FatArray`) is rejected by falling through
/// [`is_scalar_only_layout`].
fn is_one_level_string_or_scalar(ty: &NativeType) -> bool {
    matches!(ty, NativeType::String) || is_scalar_only_layout(ty)
}

/// Resolve a struct's `(field, type)` list into a stack-flattened
/// [`NativeType::Struct`] layout under `name`. Each field type must resolve to a
/// native type; an `f32` field and a MUTABLE heap-value field (`list`/`map`) are
/// rejected (the function then skips gracefully), while a `string` field is
/// supported (immutable, shared on the flat word-copy). Shared by the non-generic
/// struct path and the monomorphized generic-struct path.
fn resolve_struct_fields(
    name: &str,
    field_defs: &[(String, TypeRef)],
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
) -> Result<NativeType, String> {
    let mut fields = Vec::with_capacity(field_defs.len());
    for (field_name, field_ty) in field_defs {
        let native = resolve_native_type(field_ty, structs, enums).map_err(|_| {
            format!("struct `{name}` field `{field_name}` is not an all-i64 native type")
        })?;
        // An `f32` field is out of scope: the aggregate copy/pass paths move
        // whole 8-byte words through a GPR, which would not keep a 4-byte
        // f32 rounded. An `f64` is a full 8-byte word, so a GPR round-trip is
        // bit-lossless — f64 fields ARE supported (read/store route through
        // the float lowerer via `resolve_*_typed` + `float_width_of_expr`'s
        // Field arm; init/copy move the word unchanged). Reject only f32.
        if matches!(native, NativeType::F32) {
            return Err(format!(
                "struct `{name}` field `{field_name}` is an f32; f32 struct fields are not in the native subset (an f64 field is fine)"
            ));
        }
        // A MUTABLE heap-value field (`list`/`map`) inside an aggregate is
        // deferred: the aggregate copy/pass paths move flat words and would
        // SHARE (not deep-copy) the referenced heap block, breaking value
        // semantics for a mutable list/map field. A `string` field IS
        // supported: strings are immutable, so the flat word copy that the
        // struct construction/boundary/`match` paths already emit shares the
        // record pointer, which IS its value-semantic copy (exactly like a
        // `string` list element or enum payload). So permit `String` here and
        // reject only the mutable `List`/`Map` fields — the function then
        // skips gracefully for those.
        if matches!(native, NativeType::List { .. } | NativeType::Map { .. }) {
            return Err(format!(
                "struct `{name}` field `{field_name}` is a mutable heap value \
                 (list/map); mutable heap-value struct fields are not in the native \
                 subset (a `string` field is supported)"
            ));
        }
        fields.push((field_name.clone(), native));
    }
    Ok(NativeType::Struct {
        name: name.to_string(),
        fields,
    })
}

/// The base enum-constructor name of a type spelling: `option` for `option<i64>`
/// or a bare `option`, `result` for `result<i64, i64>`, and the enum name for a
/// user enum spelling (which never carries generic arguments).
fn enum_ctor_name(name: &str) -> &str {
    match name.split_once('<') {
        Some((ctor, _)) => ctor,
        None => name,
    }
}

/// Whether a type spelling names an enum: the built-in `option`/`result`
/// generics (with or without arguments) or a declared user enum.
fn is_enum_type_name(name: &str, enums: &[IrEnumDef]) -> bool {
    let ctor = enum_ctor_name(name);
    ctor == "option" || ctor == "result" || enums.iter().any(|e| e.name == ctor)
}

/// Resolve an enum type spelling into its native layout: the ordered variants
/// (each with its discriminant tag and scalar payload word layouts) and the
/// shared payload region width. The tag of a variant is its index in the
/// interpreter/IR variant order: declared order for a user enum, and
/// `some`(0)/`none`(1) for `option`, `ok`(0)/`err`(1) for `result`.
fn resolve_enum_type(
    ty: &TypeRef,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
) -> Result<NativeType, String> {
    let ctor = enum_ctor_name(&ty.name);
    // Set when `ctor` names a user-declared GENERIC enum (`enum Opt<T>`): the
    // instantiation is monomorphized (its variant payloads substituted) and gated
    // to the scalar-`T` scope below, exactly like a generic struct.
    let mut user_generic = false;
    // The ordered (variant name, payload types) pairs for this enum.
    let variant_specs: Vec<(String, Vec<TypeRef>)> = match ctor {
        "option" => {
            // `option<T>`: `some(T)` then `none`. The payload type is only known
            // when the spelling carries its argument (`option<i64>`); a bare
            // `option` is not sizable and is rejected so the function skips.
            let elem = ty.option_element().ok_or_else(|| {
                "native enum layout needs the concrete `option<T>` element type".to_string()
            })?;
            vec![
                ("some".to_string(), vec![elem]),
                ("none".to_string(), vec![]),
            ]
        }
        "result" => {
            // `result<T, E>`: `ok(T)` then `err(E)`. Both arguments must be
            // present and scalar; a heap `E` (e.g. `string`) is rejected below.
            let (ok_ty, err_ty) = ty.result_args().ok_or_else(|| {
                "native enum layout needs the concrete `result<T, E>` argument types".to_string()
            })?;
            vec![
                ("ok".to_string(), vec![ok_ty]),
                ("err".to_string(), vec![err_ty]),
            ]
        }
        _ => {
            let def = enums
                .iter()
                .find(|e| e.name == ctor)
                .ok_or_else(|| format!("enum `{ctor}` is not declared"))?;
            if def.type_params.is_empty() {
                def.variants
                    .iter()
                    .map(|v| (v.name.clone(), v.payload.clone()))
                    .collect()
            } else {
                // MONOMORPHIZE a user generic enum instantiation (`Opt<i64>`):
                // substitute the spelling's concrete type arguments into every
                // variant's payload types before computing the native layout.
                user_generic = true;
                let args = ty.generic_args(ctor).unwrap_or_default();
                let subst = subst_map(&def.type_params, &args);
                def.variants
                    .iter()
                    .map(|v| {
                        (
                            v.name.clone(),
                            v.payload
                                .iter()
                                .map(|p| substitute_type(p, &subst))
                                .collect(),
                        )
                    })
                    .collect()
            }
        }
    };

    let mut variants = Vec::with_capacity(variant_specs.len());
    let mut payload_words = 0usize;
    for (tag, (name, payload_types)) in variant_specs.into_iter().enumerate() {
        let mut payload = Vec::with_capacity(payload_types.len());
        for payload_ty in &payload_types {
            // A scalar or `string` payload is supported: an `i64`/fixed-width/bool/
            // char/byte cell (`NativeType::I64`), a float (`F64`/`F32`), or a
            // `string` (`NativeType::String`, an immutable heap pointer in one
            // slot, shared on the flat word-copy deep copy since strings are
            // immutable — so `option<string>`, `result<i64, string>`, and user
            // enums with a string payload are supported). A one-level MUTABLE
            // aggregate payload — a `struct` (`HeapStruct`) or a nested
            // `list<scalar|string>` (`List`) — is now also supported: it occupies
            // one payload pointer word and is DEEP-COPIED on the enum's
            // value-semantic copy (so `option<struct>` — the `map_get` result on a
            // `map<K, struct>` — and `result<i64, list<i64>>` lay out). Deeper
            // nesting and a `map`/`array` payload stay deferred.
            let native = if is_scalar_or_string_slot(payload_ty) {
                resolve_native_type(payload_ty, structs, enums).map_err(|_| {
                    format!(
                        "enum `{ctor}` variant `{name}` payload type `{}` is not a native scalar",
                        payload_ty.name
                    )
                })?
            } else {
                native_collection_slot(payload_ty, structs, enums, 0).ok_or_else(|| {
                    format!(
                        "enum `{ctor}` variant `{name}` payload type `{}` is not a native scalar, \
                         `string`, or one-level mutable aggregate (deeper payloads are deferred)",
                        payload_ty.name
                    )
                })?
            };
            match native {
                NativeType::I64
                | NativeType::F64
                | NativeType::F32
                | NativeType::String
                | NativeType::HeapStruct { .. }
                | NativeType::List { .. } => payload.push(native),
                _ => {
                    return Err(format!(
                        "enum `{ctor}` variant `{name}` has an unsupported payload type"
                    ));
                }
            }
        }
        let this_words: usize = payload.iter().map(NativeType::words).sum();
        payload_words = payload_words.max(this_words);
        variants.push(NativeEnumVariant {
            name,
            tag: tag as i64,
            payload,
        });
    }

    let native = NativeType::Enum {
        name: ctor.to_string(),
        variants,
        payload_words,
    };
    // Native-supported-layout scope gate (default-deny), mirroring the generic-struct
    // path: a user generic enum instantiation compiles natively when its whole layout
    // is scalar-only OR its variant payloads are scalars plus one-level immutable
    // `string` words (a heap type argument like `Opt<string>` -> a `string` payload,
    // or `Either<i64, string>`). Such a `string` payload is the same shared immutable
    // word a non-generic string-payload enum uses. A DEEPER heap payload is still
    // DEFERRED: a mutable-aggregate payload (`Stack<i64>`'s `list<Tree<i64>>`, an
    // `option<struct>`-shaped nested aggregate) or a two-level `string` nesting.
    // Built-in `option`/`result` and non-generic user enums keep their existing
    // string/one-level-aggregate payload support (they are not gated here).
    if user_generic && !is_native_supported_generic_layout(&native) {
        return Err(format!(
            "generic enum instantiation `{}` has a deeper-than-one-level heap payload after \
             substitution (a mutable heap value or a nested heap-carrying aggregate); it is \
             deferred to the interpreters",
            ty.name
        ));
    }
    Ok(native)
}

/// The **packed** layout of a narrow integer array element, or `None` for every
/// other element type (which keeps its existing 8-byte-word layout).
///
/// This set is not arbitrary — it is exactly the intersection of two conditions,
/// and both are load-bearing:
///
/// 1. **The interpreters already stride by it.** `Value::layout_size`
///    (`lullaby_runtime/src/lib.rs`) gives `Value::Int { ty }` a stride of
///    `width_bits / 8` and `Value::Byte` a stride of 1, and `RawPointerMemory`
///    builds an `addr_of` region with that stride. So for these element types the
///    interpreters DEFINE a walk whose size law answers 1/2/4 — and native must
///    answer the same number or not compile the program at all.
/// 2. **The raw-pointer surface can dereference it.** [`pointee_access`] admits
///    exactly the integer widths, so a `ptr_read`/`ptr_write` through the resulting
///    pointer is width-exact.
///
/// Types where the interpreters stride narrowly but [`pointee_access`] refuses the
/// pointee — `bool` (1), `char` (4), `f32` (4) — are deliberately NOT packed. They
/// need no packing to stay correct, because `addr_of` of one is refused before a
/// stride is ever observable (`is_addressable_word_type` rejects them and
/// `pointee_access` returns `None`), so no program can witness the difference.
/// Packing them anyway would churn the float SIMD path and the `bool`/`char` cell
/// invariants for no reachable behavior. The 8-byte types (`i64`/`u64`/`isize`/
/// `usize`/`ptr<T>`) are already width-exact as cells and are untouched — their
/// codegen stays bit-for-bit identical.
pub(crate) fn narrow_array_element(name: &str) -> Option<NativeType> {
    let (bytes, signed) = match name {
        "i8" => (1, true),
        "i16" => (2, true),
        "i32" => (4, true),
        "u8" | "byte" => (1, false),
        "u16" => (2, false),
        "u32" => (4, false),
        _ => return None,
    };
    Some(NativeType::Narrow { bytes, signed })
}

/// Infer the `NativeType` of an initializer expression, using its static type
/// plus (for array literals) the literal element count. This is how array
/// lengths enter the layout, since `array<T>` carries no length.
pub(crate) fn native_type_of_init(
    expr: &BytecodeExpr,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    signatures: &HashMap<String, NativeSignature>,
) -> Result<NativeType, String> {
    if let BytecodeExprKind::Array(elements) = &expr.kind {
        let first = elements
            .first()
            .ok_or("empty array literals are not in the native stack subset")?;
        // An array ELEMENT of a narrow integer type packs to its C width; every
        // other element keeps its 8-byte-word layout. This is the only position in
        // the backend that produces a `Narrow` (see `narrow_array_element`).
        let elem = match narrow_array_element(&first.ty.name) {
            Some(packed) => packed,
            None => native_type_of_init(first, structs, enums, signatures)?,
        };
        for other in &elements[1..] {
            let other_ty = match narrow_array_element(&other.ty.name) {
                Some(packed) => packed,
                None => native_type_of_init(other, structs, enums, signatures)?,
            };
            if other_ty != elem {
                return Err("array literal elements have differing native layouts".to_string());
            }
        }
        return Ok(NativeType::Array {
            elem: Box::new(elem),
            len: elements.len(),
        });
    }
    // An array local bound from a call takes its length from the callee's inferred
    // return layout (the `array<T>` type alone carries no length).
    if expr.ty.name.starts_with("array<")
        && let BytecodeExprKind::Call { name, .. } = &expr.kind
        && let Some(sig) = signatures.get(name)
        && matches!(sig.ret, NativeType::Array { .. })
    {
        return Ok(sig.ret.clone());
    }
    resolve_native_type(&expr.ty, structs, enums)
}

/// The concrete word length of an array-typed signature slot, keyed by parameter
/// name; the return array (if any) uses the reserved key `RETURN_ARRAY_KEY`.
/// Fixed arrays carry no length in their `array<T>` type, so a function that
/// passes or returns one has its length inferred (see [`infer_array_lengths`])
/// and pinned here so the callee's copy-in / hidden-return-write knows the count.
pub(crate) type ArrayLengths = HashMap<String, usize>;

/// Reserved [`ArrayLengths`] key for a function's return-array length.
pub(crate) const RETURN_ARRAY_KEY: &str = "\0return";

/// Sentinel [`ArrayLengths`] value marking an array **parameter** as a fat pointer
/// — a read-only scalar array whose length is not known at compile time, passed as
/// a `(data_ptr, runtime length)` descriptor (see [`fat_array_param_elem`]) rather
/// than copied in by value. `resolve_signature_native_type` maps this sentinel to
/// [`NativeType::FatArray`] instead of a fixed [`NativeType::Array`].
pub(crate) const FAT_ARRAY_LEN: usize = usize::MAX;

/// Resolve a function's **return** type into its `NativeType`.
///
/// The one and only place [`NativeType::Void`] is produced. A function that
/// declares no return type (`fn poke p ptr<i64> v i64`) has a `void` return type,
/// which is the ABSENCE of a return value rather than an unsupported one — so it
/// resolves to `Void` and the function stays native-eligible, instead of being
/// rejected by [`resolve_signature_native_type`] as a type "not in the native
/// stack subset" (which is the right answer for a `void` PARAMETER, and is why
/// this is a separate return-only entry point rather than a `void` arm added to
/// the shared signature resolver).
///
/// Every other return type delegates unchanged to [`resolve_signature_native_type`]
/// under the [`RETURN_ARRAY_KEY`] array-length key.
pub(crate) fn resolve_return_native_type(
    ty: &TypeRef,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    array_lengths: &ArrayLengths,
) -> Result<NativeType, String> {
    if ty.is_void() {
        return Ok(NativeType::Void);
    }
    resolve_signature_native_type(ty, structs, enums, array_lengths, RETURN_ARRAY_KEY)
}

/// Resolve a **signature** type (a parameter or return type) into its
/// `NativeType`. Identical to [`resolve_native_type`] except a fixed-array type
/// (`array<T>`), whose length is absent from the type, takes its length from
/// `array_lengths[key]` (populated by [`infer_array_lengths`]). A bare array with
/// no inferred length is rejected so the function skips gracefully.
///
/// `void` is NOT resolvable here — a `void` parameter/local/field is genuinely
/// outside the native subset. A void RETURN goes through
/// [`resolve_return_native_type`] instead.
pub(crate) fn resolve_signature_native_type(
    ty: &TypeRef,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    array_lengths: &ArrayLengths,
    key: &str,
) -> Result<NativeType, String> {
    // A heap-backed `array<string>` param/return is a pointer word (a `list<string>`
    // block), not a stack-flattened array, so it needs no inferred length.
    if heap_string_array_element(ty).is_some() {
        return Ok(NativeType::List {
            elem: Box::new(NativeType::String),
        });
    }
    if let Some(rest) = ty.name.strip_prefix("array<") {
        let elem_name = rest.strip_suffix('>').unwrap_or(rest);
        // Keep the signature's element layout in lockstep with the array-literal
        // path above: a narrow integer element packs to its C width, so a caller's
        // packed array and the callee's copy-in/fat-pointer view agree on the
        // stride. Resolving this through the general resolver instead would give
        // the callee an 8-byte-cell view of a packed buffer.
        let elem = match narrow_array_element(elem_name) {
            Some(packed) => packed,
            None => resolve_signature_native_type(
                &TypeRef::new(elem_name),
                structs,
                enums,
                array_lengths,
                key,
            )?,
        };
        let len = *array_lengths.get(key).ok_or_else(|| {
            format!("array length for signature slot `{key}` could not be inferred")
        })?;
        // A read-only array parameter with no inferable length is passed as a fat
        // pointer (descriptor of the element layout), not a fixed stack array.
        if len == FAT_ARRAY_LEN {
            return Ok(NativeType::FatArray {
                elem: Box::new(elem),
            });
        }
        return Ok(NativeType::Array {
            elem: Box::new(elem),
            len,
        });
    }
    resolve_native_type(ty, structs, enums)
}
