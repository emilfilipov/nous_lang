//! Native type model and resolution: `NativeType`/`NativeEnumVariant`/`FloatWidth`
//! and their impls, resolution of source `TypeRef`s and enum definitions into
//! native layout types, and the array-length bookkeeping types. Split out of
//! native_object.rs; sees the parent's items via `use super::*`.

use super::*;

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
    /// A fixed-length array of a supported element type.
    Array { elem: Box<NativeType>, len: usize },
    /// A **fat-pointer** `array<T>` parameter: a `(data_ptr, length)` descriptor
    /// occupying two frame words — word 0 is a pointer to the caller's element 0
    /// (the array's highest stack address; elements descend from there like a
    /// stack array), word 1 is the runtime element count. Used for a **read-only**
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

    /// The number of 8-byte words this value occupies on the stack.
    pub(crate) fn words(&self) -> usize {
        match self {
            // A string, list, map, or heap struct is a single pointer word, like a
            // scalar.
            NativeType::I64
            | NativeType::F64
            | NativeType::F32
            | NativeType::String
            | NativeType::List { .. }
            | NativeType::Map { .. }
            | NativeType::HeapStruct { .. } => 1,
            NativeType::Struct { fields, .. } => fields.iter().map(|(_, t)| t.words()).sum(),
            NativeType::Array { elem, len } => elem.words() * len,
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
        // The built-in generic enums and user enums with scalar payloads.
        name if is_enum_type_name(name, enums) => resolve_enum_type(ty, structs, enums),
        name => {
            let def = structs
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| format!("type `{name}` is not in the native stack subset"))?;
            let mut fields = Vec::with_capacity(def.fields.len());
            for (field_name, field_ty) in &def.fields {
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
    }
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
            def.variants
                .iter()
                .map(|v| (v.name.clone(), v.payload.clone()))
                .collect()
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

    Ok(NativeType::Enum {
        name: ctor.to_string(),
        variants,
        payload_words,
    })
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
        let elem = native_type_of_init(first, structs, enums, signatures)?;
        for other in &elements[1..] {
            let other_ty = native_type_of_init(other, structs, enums, signatures)?;
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

/// Resolve a **signature** type (a parameter or return type) into its
/// `NativeType`. Identical to [`resolve_native_type`] except a fixed-array type
/// (`array<T>`), whose length is absent from the type, takes its length from
/// `array_lengths[key]` (populated by [`infer_array_lengths`]). A bare array with
/// no inferred length is rejected so the function skips gracefully.
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
        let elem = resolve_signature_native_type(
            &TypeRef::new(elem_name),
            structs,
            enums,
            array_lengths,
            key,
        )?;
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
