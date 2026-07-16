# WebAssembly Backend Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This note designs a WebAssembly (WASM) backend that compiles Lullaby's typed IR
to a real `.wasm` binary module. It is the highest-leverage step toward the web
path: a `.wasm` module runs in every browser and in server-side WASM runtimes.
The interpreters remain the correctness ground truth; the WASM backend must
produce the same results.

## Status

**DELIVERED: the scalar subset, the linear-memory step (memory/data/import +
`wasm_log`), heap types — strings and fixed aggregates — laid out in linear
memory, and the JS/DOM host interop layer (`console_log`, `dom_set_text`).** It
ships as a `wasm` module in `crates/lullaby_ir`
(`crates/lullaby_ir/src/wasm.rs`, `emit_wasm_module`), the
`lullaby wasm [--verbose] [-o out.wasm] <file.lby>` CLI command, structural
encoder unit tests, and node-gated execution-parity tests against the
interpreter (`crates/lullaby_cli/tests/cli.rs`). The encoder writes the module
header, the Type/Import/Function/Memory/Global/Export/Code/Data sections in
canonical order, LEB128 integers, and the stack-machine opcodes it needs — using
the Rust standard library only, no external crate. When no function is eligible,
the CLI reports diagnostic `L0338`.

### Linear-memory infrastructure (landed)

- **Memory section (id 5)** declares one linear memory (min 1 page, 64 KiB) and
  the **Export section** exports it as `"memory"` (export kind mem).
- **Global section (id 6)** declares a mutable `i32` bump pointer initialized past
  the reserved region AND the whole string-literal pool, so `__alloc` never
  overwrites static string data.
- **Data section (id 11)** is one active segment at offset 0: the reserved
  low-memory region (zeros, so a handed-out pointer is never `0`/null) followed by
  the interned string-literal pool at `RESERVED_BASE` (16).
- An internal `__alloc(size i32) -> i32` bump-allocator helper reads the global,
  advances it by `size`, and returns the old offset. Struct and array
  construction call it to reserve their layout.
- **Import section (id 2)** imports three host functions from module `env`, in a
  fixed order that defines their low WASM function indices:
  - `env.log_i64 (func (param i64))` (index 0), exposed as the builtin
    `wasm_log(x i64) -> void`.
  - `env.console_log (func (param i32 i32))` (index 1), exposed as
    `console_log(s string) -> void` — see **JS/DOM host interop** below.
  - `env.dom_set_text (func (param i32 i32 i32 i32))` (index 2), exposed as
    `dom_set_text(id string, text string) -> void`.
  Each import references a reserved leading entry of the Type section (type index
  == function index). A `wasm_log`/`console_log`/`dom_set_text` call lowers to a
  `call` of the matching import, which makes eligible functions side-effecting.
  On the interpreters (AST/IR/bytecode) they print deterministically, so
  cross-backend parity holds.
- **Import index fix-up:** imported functions occupy the LOW WASM function
  indices (`0..IMPORT_FUNC_COUNT`, now 3), so every internally-defined function is
  numbered from `IMPORT_FUNC_COUNT` up. Adding an import shifts every internal
  function index, call target, and function-export index by one; the fix-up is
  driven entirely by `IMPORT_FUNC_COUNT` (the eligibility pass assigns internal
  indices starting at `IMPORT_FUNC_COUNT`, and the Export section writes
  `IMPORT_FUNC_COUNT + i`), so extending it from one to three imports needed only
  the new constants and the extra import/type entries.

### JS/DOM host interop (landed)

The web-frontend interop layer builds on the same import mechanism as `wasm_log`:
a browser (or any WASM host) supplies the imports, and WASM-compiled Lullaby calls
them to talk to JavaScript and the DOM.

- `console_log(s string) -> void` lowers to `env.console_log(ptr i32, len i32)`:
  it pushes a pointer to the string's first UTF-8 byte and its UTF-8 byte length,
  then calls the import. A browser host implements it as `console.log`.
- `dom_set_text(id string, text string) -> void` lowers to
  `env.dom_set_text(id_ptr i32, id_len i32, text_ptr i32, text_len i32)`: it
  pushes each string's data pointer and byte length in order, then calls the
  import. A browser host implements it as
  `document.getElementById(id).textContent = text`.
- Each string operand's record pointer is evaluated once into a scratch `i32`
  local, then pushed as `(ptr, len)` where `ptr` is `record + STR_DATA_OFF` (the
  address of the first UTF-8 byte, past the two `i32` headers) and `len` is the
  record's `byte_len` header (the UTF-8 byte length, NOT the char count, so
  multi-byte text decodes correctly). The host slices `[ptr, ptr + len)` of
  `memory` directly — no header offset to add.
- On the interpreters, `console_log` prints the string as a stdout line and
  `dom_set_text` prints `id=text`, so all backends observe the same side effect
  and the parity harness stays green. Functions calling these builtins are
  eligible for WASM (the eligibility gate accepts them alongside `wasm_log` and
  `len`).

### Heap types (landed)

`string`, `struct`, and fixed `array` values are **`i32` pointers** into linear
memory. Their WASM slot type is the scalar's own type for a scalar, or `i32` for
a pointer (nested strings/structs/arrays).

- **Strings:** a `string` is a pointer to
  `[char_len: i32][byte_len: i32][utf8 bytes]` — the Unicode scalar (char) count
  at offset 0 (shared with the array/list length header), the UTF-8 byte length at
  offset 4, then the encoded bytes at `STR_DATA_OFF` (8). Each distinct string
  literal is interned ONCE into the Data section (a constant static offset is its
  value). `len(s)` lowers to `i32.load` of the char-count header (offset 0) then
  `i64.extend_i32_s` (the builtin returns `i64`, char count to match the
  interpreters). Storing the byte length explicitly (rather than assuming one byte
  per char) is what lets concatenation and the host imports handle multi-byte
  UTF-8 correctly.
  - **Runtime concatenation** (`a + b` on two `string` values) is lowered: it reads
    each operand's char-count and byte-count headers, `__alloc`s a fresh record of
    `STR_DATA_OFF + byte_a + byte_b` bytes, writes the summed headers (char count
    `char_a + char_b`, byte count `byte_a + byte_b`), and `memory.copy`s (the
    bulk-memory opcode `0xfc 0x0a 0x00 0x00`) each operand's UTF-8 byte range into
    place. Strings are immutable, so the result is always a NEW record with no
    aliasing; `len` of the result equals `len(a) + len(b)`, matching the
    interpreters bit-for-bit. Chained `a + b + c` nests naturally (the inner `+`
    yields a normal record consumed by the outer). The constant folder collapses a
    literal-only `"foo" + "bar"` to a single interned literal before codegen, so the
    runtime path is exercised only when at least one operand is computed at runtime.
  - **`to_string(x)`** is lowered for **integer / `bool` / `char` / `byte` /
    `string`** arguments, building a fresh `[char_len][byte_len][utf8]` record
    identical to the interpreters' `Value::Display`:
    - **Integers** (`i64`, the fixed-width kinds `i8`…`u64`, `isize`/`usize`) use an
      in-WASM itoa. A signed value records `sign = value < 0`, then computes the
      magnitude as `u64` (`0 - value`, computed with wrapping `i64.sub`, so
      `i64::MIN` yields its correct unsigned magnitude `0x8000000000000000` without
      the unrepresentable negation); an unsigned/`byte` value uses the magnitude
      directly. A first pass counts decimal digits (a `block { loop { … } }`
      dividing the magnitude down by 10 with `i64.div_u`, so `0` still counts one
      digit); the record `[STR_DATA_OFF + sign + ndigits]` is then `__alloc`'d, an
      optional leading `-` is written, and a second pass writes the digits
      least-significant-first from the tail backward (`i64.rem_u`/`i64.div_u`). All
      output is ASCII, so `char_len == byte_len == sign + ndigits`. Unsigned kinds
      print the `u64` reinterpretation of the normalized cell (matching the
      interpreters), so `to_u64(0 - 1)` renders `18446744073709551615`.
    - **`bool`** selects the interned `"true"` / `"false"` literal pointer via a
      typed `if`/`else`.
    - **`char`** encodes the Unicode scalar to its 1–4 byte UTF-8 sequence in a
      fresh record with `char_len == 1` and `byte_len` the encoded length (a
      nested `< 0x80` / `< 0x800` / `< 0x10000` / else chain writes the continuation
      bytes with `i32.store8`).
    - **`string`** is the identity — strings are immutable, so the same record
      pointer is returned.
    - **Floats** (`to_string(f32)` / `to_string(f64)`) are **deferred**: matching
      Rust's `Display` dtoa bit-for-bit is out of scope, so a function formatting a
      float still skips to the interpreters. Verified end-to-end by
      `tests/fixtures/valid/wasm_to_string.lby` (`main` = 78).
  - **Index-based string operations** are lowered, matching the interpreters
    (`builtin_substring` / `builtin_find` / `char_find` / `builtin_contains` /
    `builtin_starts_with` / `builtin_ends_with`) bit-for-bit. The dispatch is gated
    on a `string` first argument. All scans are inline WASM loops over the record's
    UTF-8 bytes, comparing `memory[hay + i]` against `memory[needle + j]` with
    `i32.load8_u`.
    - **`substring(s, start, end) -> string`** is the CHAR-indexed half-open
      `[start, end)` slice. `start`/`end` are `i64` char indices; the interpreters
      raise `L0413` when `start < 0 || end < 0 || start > end || end > char_count`,
      so the WASM path emits that exact bounds test and `unreachable` (traps) on
      failure rather than producing a wrong value. Otherwise it maps the char
      indices to byte offsets by walking the UTF-8 (advancing past one lead byte
      plus its continuation bytes per char — a byte is a continuation byte iff
      `(b & 0xC0) == 0x80`), `__alloc`s a fresh `[char_len][byte_len][utf8]` record
      sized `STR_DATA_OFF + (end_byte - start_byte)`, writes the summed headers, and
      `memory.copy`s the byte range in. `substring("café", 3, 4)` yields the
      multi-byte `é` with `char_len = 1`, `byte_len = 2`.
    - **`find(haystack, needle) -> i64`** returns the CHAR index of the first
      byte-level occurrence of `needle`, or `-1` if absent. It byte-searches every
      start position `0..=(hay_len - needle_len)` for the first full match, then
      counts the UTF-8 characters preceding that byte offset (the count of
      non-continuation bytes, `(b & 0xC0) != 0x80`) and extends it to `i64` — exactly
      `text[..byte_index].chars().count()`. An empty needle matches at byte 0, whose
      preceding char count is 0, so `find(s, "") == 0` (matching Rust's
      `find("") == Some(0)`).
    - **`contains(s, sub)` / `starts_with(s, prefix)` / `ends_with(s, suffix)`** are
      **byte-exact** `bool` tests (byte equality is char-position-independent, so no
      UTF-8 decode is needed). `contains` reuses the `find` byte search and yields its
      found flag; `starts_with`/`ends_with` short-circuit to `false` when the
      needle is longer than the haystack, else compare bytes at position `0` /
      `hay_len - needle_len`. An empty needle matches for all three.
    - Verified end-to-end by
      `crates/lullaby_cli/tests/cli.rs::wasm_string_ops_execution_parity_with_node`,
      fixture `tests/fixtures/valid/wasm_string_ops.lby` (`main` = 11), which
      exercises a multi-byte string across edge indices, present/absent/empty
      `find`, and true/false predicate cases under Node.
  - Other runtime string builders (`replace`, `upper`/`lower`, `split`/`join`,
    `chars`/`string_from_chars`) are not yet lowered — a function using one is
    skipped and still runs on the interpreters. `upper`/`lower` are deferred because
    Unicode case mapping is hard to match Rust bit-for-bit.
- **Structs:** a `struct` is a pointer to a contiguous run of one 8-byte slot per
  field in declared order (uniform 8-byte slots keep `i64`/`f64` naturally
  aligned and make offsets a simple `slot_index * 8`). Positional construction (a
  `Call` whose name is the struct, as the IR lowerer emits struct literals)
  `__alloc`s the run and stores each field; `.field` reads a slot with a typed
  `*.load`; `p.field = v` (and compound forms) writes a slot with a typed
  `*.store`.
- **Arrays:** a fixed `array` literal is a pointer to `[len: i32][elem slots...]`,
  one 8-byte slot per element. `a[i]` computes `base + 4 + i*8` (index truncated
  `i64 -> i32`) and loads; `a[i] = v` stores; `len(a)` loads the leading `i32`.
  WASM traps on out-of-bounds memory access, so no explicit bounds check is
  emitted this increment.
- **Assignment paths:** `a.b.c = v` and `xs[i].f = v` fold each hop into a running
  address; non-final hops load the nested pointer, the final hop leaves the slot
  address for the store (or a load-op-store for compound assignment).

- **Enums (scalar or `string` payloads):** an enum value is a pointer to
  `[tag: i32 (padded to 8)][slot0][slot1]...]`: an `i32` discriminant (the
  variant's index in declaration order, matching the interpreters) plus one
  8-byte payload slot per position, sized for the widest variant. Construction
  (`some(x)`/`none`/`ok(x)`/`err(e)` and user `Variant(payload...)`, all emitted
  by the IR lowerer as positional `Call`s) `__alloc`s the record, stores the tag
  at offset 0, and stores each payload value into its slot. `match` loads the tag
  (`i32.load` at offset 0) and dispatches with a chain of `i32.eq` + typed
  `if`/`else` blocks — a `Wildcard` arm is the final `else`, and with
  exhaustiveness guaranteed the last variant arm is emitted unconditionally so a
  value match always leaves a value — binding each arm's payload slots into locals
  before its body. A payload slot is a scalar (its own value type) or a `string`
  (an `i32` pointer to the immutable string record, stored/loaded/copied exactly
  like a scalar word). A payload slot may ALSO be a **mutable aggregate** — a
  `struct` or a one-level nested `list` — occupying one `i32`-pointer slot that is
  DEEP-COPIED per payload on the enum's value-semantic copy (`emit_deep_copy_enum`
  first flat-copies the whole record, then branches on the loaded tag and
  `emit_deep_copy`s each mutable-aggregate payload slot of the matching variant), and
  `match` binds a mutable-aggregate payload as an independent deep copy. This covers
  the built-in `option<T>`/`result<T, E>` and user enums when every variant payload
  is a scalar, a `string`, or a one-level mutable aggregate — notably
  `option<string>` and `option<struct>` (the results of `map_get` on a
  `map<K, string>`/`map<K, struct>`), `result<i64, string>`, and `result<i64,
  list<i64>>`. `map_get`'s `option<V>` layout is built directly (not via the generic
  `enum_layout` scalar/string gate) so a `struct` value lays out. An enum whose
  payload is a fixed `array` or a `map`, or is nested past one mutable level, is
  still deferred.

### Aggregates across call boundaries (landed)

A `struct`, fixed `array`, or supported `enum` may be a function **parameter, a
return value, or a call argument** — not just a local. At the WASM level the `i32`
pointer is passed and returned directly, so the ABI is the pointer itself.

To preserve Lullaby **value semantics** (an aggregate passed by value is an
independent snapshot; a callee mutating its parameter must not change the caller's
copy), each **mutable** aggregate argument is **deep-copied at the call site**
before the `call`: a fresh record is `__alloc`'d and every word copied, recursing
into nested mutable aggregate fields/elements. This mirrors the interpreters'
recursive `Value::clone` bit-for-bit. Specifics:

- **struct:** copy each field slot; a scalar/string slot is copied word-for-word,
  a nested mutable aggregate slot is itself deep-copied.
- **array:** read the `[len]` header, `__alloc` a fresh `[len][slots]` block, store
  the header, and copy each element in a runtime loop (recursing for nested
  aggregate elements).
- **enum:** copy the `[tag][payload slots]` record word-for-word — enum payloads
  are a scalar or a `string` pointer, and a `string` is immutable so sharing its
  pointer IS its value-semantic copy, so a flat copy is an exact deep copy.
- **string:** NOT copied. Strings are immutable, so sharing the pointer is already
  value-equivalent to the interpreters' clone. This is why a `string` **element**
  of a `list<string>`, **value** of a `map<K, string>`, or enum payload is copied
  as a shared pointer word (never deep-recursed) on the collection's deep copy.

A **returned** aggregate is the callee's own fresh record, so no extra copy is
needed on return. Fixtures `wasm_aggregate_args.lby` and
`wasm_aggregate_nested.lby` exercise struct/array take+return plus value-semantics
probes (a callee mutates its parameter; the caller's copy is verified unchanged),
node-gated against the interpreter.

**Deferred:** aggregates containing values the backend does not lay out — a
`list`/`map`/`enum` nested MORE than one **mutable** heap level deep
(`list<list<list<…>>>`, `map<K, map<…>>`, `map<K, list<…>>`, an enum whose payload
is a nested collection), a `list`/`map`/`enum` element/value/payload that is a
**fixed `array`** or a **`map`**, or a `map` with a **mutable-aggregate KEY**;
`to_string` of a **float** (`f32`/`f64`) and the string builders not yet lowered —
`replace`, `upper`/`lower`, `split`/`join`, `chars`/`string_from_chars` (the `+`
concat, `to_string`, `substring`, `find`, `contains`, `starts_with`, and
`ends_with` DO compile); and a free-list allocator (`__alloc` never frees this
increment). One level of **mutable-aggregate** element/value/payload DOES compile
now: `list<struct>`, `list<list<scalar|string>>`, `map<K, struct>`, and an enum
with a `struct`/one-level-`list` payload (`option<struct>`, `result<struct, E>`,
`result<i64, list<i64>>`) — each is deep-copied recursively, see **Mutable-heap
collection elements/values (landed)** below. (`string` **elements** of a
`list<string>`, `string` **keys** of a `map<string, V>` — compared by content —
`string` **values** of a `map<K, string>`, and `string` enum payloads also DO
compile.) Functions using any deferred construct are skipped with a reason and
still run on the interpreters.

### User generic types — monomorphization (landed, A1 parity with native)

A user-defined generic `struct`/`enum` instantiated with **scalar** type arguments
(`Box<i64>`, `Pair<i64, bool>`, `Opt<i64>`, `Either<i64, bool>`) **or with a
one-level `string` type argument** (`Box<string>`, `Pair<string, i64>`,
`Opt<string>`, `Either<i64, string>`) is **monomorphized** to a concrete linear-
memory layout and compiled to WASM — bringing the WASM backend to A1 parity with the
native backend. Lives in `crates/lullaby_ir/src/wasm_generics.rs`
(`expand_generic_instantiations`), run by `emit_wasm_module` right after the
`structs`/`enums` tables are built.

- **How it works.** Every reachable user-generic instantiation is collected from the
  module's signatures and bodies (a worklist that recurses through nested generic
  arguments and through substituted fields, so `Box<Pair<i64, bool>>` and a
  `Wrap<T> { inner Box<T> }` both reach their sub-instantiations). The declared type
  parameters are substituted with the instantiation's concrete arguments (the
  semantic `substitute_type`), and the resulting concrete `struct`/`enum` is
  **registered into the `structs`/`enums` tables under its full spelling** (`Box<i64>`,
  `Opt<string>`). Because every downstream classification/layout path (`is_pointer_type`,
  `slot_val_type`, `enum_layout`, `struct_field_slot`, the deep-copy and `match`
  paths) already keys off the concrete type spelling, the instantiation becomes a
  first-class concrete type with no other change — a monomorphized `Box<string>` has
  the byte-identical layout to a hand-written `struct { value string }` (one immutable-
  `string` pointer word, **shared** on the value-semantic copy), so the whole existing
  string-field / scalar-aggregate / string-payload-enum machinery applies unchanged.
- **Construction.** Constructor nodes carry the BASE type, not the instantiation
  (`Box(5)` is typed `Box`, `present(n)` is typed `Opt`), so the concrete type
  arguments are not on the expression. Struct construction therefore takes each
  field's slot value type from the **argument's own (concrete) type**; generic-enum
  construction builds the record shape from the base declaration's variant order and
  arities (both type-parameter-independent, so the record size matches the registered
  instantiation) and takes the constructed variant's payload slot types from the
  argument types (`generic_enum_construction_layout`). Field/payload read, `match`,
  value-semantic copy, and the by-pointer call boundary all use the registered
  concrete spelling and need no special-casing.
- **Default-deny scope gate (matches native's boundary exactly).** An instantiation is
  registered only when its monomorphized layout is scalar-only OR scalars plus one-
  level immutable `string` words. A DEEPER heap shape is left unregistered so its
  spelling stays unresolvable and the enclosing function skips cleanly (`L0338`),
  never miscompiled: a mutable heap field/payload (`Box<list<i64>>`, `Stack<i64>`'s
  `list<i64>`), a recursion-through-indirection generic enum (`Tree<T>` via
  `list<Tree<T>>`), a nested heap-carrying aggregate, or a two-level `string` nesting.
- **Generic methods are deferred** (skip cleanly). An inherent-`impl` method call on a
  generic type lowers to an unknown function and the calling function is skipped —
  matching native, where inherent methods on generic types are also ineligible. A
  PLAIN generic function is unaffected (`multi_param`'s `fold`, a `match` over
  `Either<i64, string>`, compiles).
- **Verification.** `wasm_tests.rs` proves the monomorphized code section is
  **byte-identical** to the equivalent hand-written concrete type
  (`monomorphized_*_matches_handwritten_bytes`) — since the hand-written path is
  already verified against the interpreters and native, this proves result-parity by
  construction. Fixtures `native_generic_scalar.lby`, `native_generic_heap_string.lby`
  (interpreter result 63), `generics/box_pair.lby`, and `generics/opt_res.lby`
  (interpreter result 141) compile with no function skipped; `generics/tree_indirection.lby`
  and a `Box<list<i64>>` probe defer with `L0338`. Purely additive — a module without
  generics leaves both tables untouched, so non-generic output is byte-identical.

### Growable `list<T>` — scalar and `string` elements (landed)

The growable, value-semantic `list<T>` collection compiles to linear memory for
**scalar element types** (`i64`, the fixed-width ints `i8`…`usize`, `f32`/`f64`,
`bool`, `char`, `byte`) **and `string`**. A `list<T>` is an **`i32` pointer** to a
header `[len: i32][cap: i32][elem slots...]`: the live element count, the allocated
capacity, then `cap` uniform 8-byte element slots (`SLOT_SIZE`, like struct/array
elements, so a scalar element stays naturally aligned and element `i` lives at
`LIST_DATA_OFF + i * SLOT_SIZE`). The `len` field shares offset 0 with the
string/array length header, so `len(l)` reuses the array length path unchanged.

A **`string` element** occupies a single slot as an `i32` pointer to the immutable
`[char_len][byte_len][utf8]` record — the SAME slot representation as a scalar, so
`push`/`get`/`set` store and load it exactly like a scalar (with an `i32.store` /
`i32.load` slot op). Because strings are immutable, a `string` element is copied by
**sharing its pointer** on the value-semantic deep copy (the flat 8-byte word copy
already does this — the pointer word is copied, never deep-recursed into the string
record), which is the value semantics the interpreters give (a `Value::String`
clone is a cheap shared clone with no observable aliasing).

- **`list_new() -> list<T>`** `__alloc`s an empty header `[len=0][cap=4][slots]`
  (a small initial capacity so the first few pushes do not each realloc) and
  leaves its pointer on the stack.
- **`push(l, x) -> list<T>`** is **value-semantic** (it returns a NEW list): it
  deep-copies `l` into a fresh `[len][cap][slots]` block, and if the copy is full
  (`len == cap`) **grows** it — reallocating a block of doubled capacity (or the
  initial capacity from an empty list), copying the live elements, and orphaning
  the old block in the no-reclaim bump heap (exactly like existing
  string/struct/array growth). It then stores `x` into slot `len`, bumps `len`,
  and leaves the fresh list pointer. Because `push` always copies before
  appending, `l = push(l, x)` matches the interpreters' `Value::clone`-then-append,
  and no aliased binding can observe the append.
- **`set(l, i, x) -> list<T>`** deep-copies `l`, stores `x` into element slot `i`
  of the copy, and returns the fresh list — value-semantic like `push`.
- **`pop(l) -> list<T>`** deep-copies `l` and decrements the copy's `len` (the last
  element's slot stays allocated, exactly like the interpreters' `Vec::pop`
  shrinks the length), returning the fresh list.
- **`get(l, i) -> T`** loads element `i` directly from
  `l + LIST_DATA_OFF + i * SLOT_SIZE` (index `i64` truncated to `i32` with
  `i32.wrap_i64`, like array indexing). **`len(l) -> i64`** loads the leading
  `i32` and sign-extends to `i64`.

**Value semantics.** A `list<T>` (scalar or `string` element) is classified as a
**mutable aggregate**, so it is deep-copied when it crosses a call boundary — a
callee pushing to its parameter cannot alter the caller's list. A `string` element
is copied as a shared pointer word (the string itself is immutable), not
deep-recursed. Combined with the
copy-on-`push`/`set`/`pop` discipline, `let b = a` (which shares the `i32`
pointer) is safe: any later `push`/`set`/`pop` on either binding produces a fresh
block and reassigns that binding, so the other still points at the untouched
original. This mirrors the interpreters' `Value::clone` bit-for-bit. The
`emit_list_deep_copy` helper duplicates the `[len][cap][slots]` block; a list
nested inside a struct field or array element is deep-copied recursively by the
existing aggregate copy paths.

**Bounds behavior.** The interpreters bounds-check `get`/`set` and raise `L0413`
on an out-of-range index. The WASM backend performs an in-bounds `get`/`set`
identically to the interpreters; a truly out-of-range index **traps** on the
linear-memory access (a consistent, documented behavior) rather than returning a
poisoned value. In-bounds programs — the common case, and what every parity
fixture exercises — agree bit-for-bit.

**Mutable-aggregate elements (landed).** A `list<struct>` and a one-level
`list<list<scalar|string>>` now compile: the element is an `i32` pointer, and the
list's value-semantic deep copy RECURSES per element — `emit_list_copy_elems` loads
each element pointer, `emit_deep_copy`s the element (struct/nested list) into a
fresh independent record, and stores that fresh pointer — instead of sharing it,
matching the interpreters' recursive `Value::clone`. `get(l, i)` on such a list
returns an **independent deep copy** of the element (the interpreters'
`values[i].clone()`), so mutating the retrieved struct never affects the list's
stored element; `push`/`set` likewise deep-copy the incoming element so a later
mutation of the source value never leaks in. `supported_list_element` accepts a
scalar, a `string` (shared), or one level of mutable aggregate (`struct` /
nested `list`); a fixed-`array` element, a `map` element, or nesting past one
mutable level is DEFERRED (skipped, still runs on the interpreters).

**Out of scope (deferred):** lists nested past one mutable level
(`list<list<list<…>>>`), a `list` whose element is a fixed `array` or a `map`
(`list<array<…>>`, `list<map<…>>`) — the enclosing function is skipped (still runs
on the interpreters) rather than miscompiled. (`list<string>`, `list<struct>`, and
`list<list<scalar>>` are NO LONGER deferred.) `__alloc` still never frees, so a
grown or copied list orphans its old block (a free-list allocator is future work).

Fixtures `wasm_list_build.lby` (build via `push` crossing the initial capacity to
trigger a grow+copy, then `get`/`len`/`set`/`pop`, `main` = 5879) and
`wasm_list_value_semantics.lby` (an aliased binding, a push-derived list, a
set-derived list, and a callee that pushes to its parameter, `main` = 334211) run
on all interpreters and, under node, their exported `main` matches the interpreter
(`crates/lullaby_cli/tests/cli.rs::wasm_list_build_execution_parity_with_node` and
`wasm_list_value_semantics_execution_parity_with_node`). The `string`-element/value
fixture `wasm_list_string.lby` (a `list<string>` of literal/concatenated/`to_string`
strings read with `get`/`len` and passed to helpers, a `map<i64, string>` with
`map_set`/`map_get`/`map_has`/`map_len`, and a `grow_probe` that pushes to its list
parameter to prove the caller's list is unaffected; `main` = 13444740) runs on all
three interpreter backends and, under node, matches
(`crates/lullaby_cli/tests/cli.rs::wasm_list_string_and_map_string_execution_parity_with_node`).

### Growable `map<K, V>` — scalar or `string` keys, scalar or `string` values (landed)

The value-semantic `map<K, V>` collection compiles to linear memory for a **scalar
key** (`i64`, the fixed-width ints `i8`…`usize`, `f32`/`f64`, `bool`, `char`,
`byte`) or a **`string` key**, and a **scalar or `string` value**. A `map<K, V>` is an **`i32` pointer**
to a header `[len: i32][cap: i32][(key, value) slot pairs...]`: the live entry
count, the allocated capacity (in entries), then `cap` entry records. Each entry is
two uniform 8-byte slots — the key slot then the value slot (`MAP_ENTRY_SIZE =
2 * SLOT_SIZE`) — so entry `i` lives at `MAP_DATA_OFF + i * MAP_ENTRY_SIZE`, its
key at offset `0` and its value at `MAP_VALUE_OFF`. Uniform 8-byte slots keep
every scalar key/value naturally aligned. `len` shares offset 0 with the
string/array/list length header.

A **`string` value** occupies the value slot as an `i32` pointer to the immutable
string record — the SAME slot representation as a scalar, so `map_set` stores and
`map_get` loads it exactly like a scalar (an `i32` slot op). Because strings are
immutable, a `string` value is copied by **sharing its pointer** on the deep copy
(the flat two-word entry copy already does this). `map_get` on a `map<K, string>`
returns `option<string>` — the same option/enum layout with the string pointer in
the `some` payload slot.

A **`string` key** likewise occupies the key slot as an `i32` pointer to the
immutable string record (shared on the deep copy). The one difference from a scalar
key is **equality**: a scalar key is compared with an integer slot op, but a
`string` key is compared by **CONTENT** — `emit_string_eq` first compares the two
records' `byte_len` headers and, if equal, walks the UTF-8 bytes with `i32.load8_u`,
returning equal only when every byte matches (a pointer-identity fast path
short-circuits when the same record is passed for both sides). This matches the
interpreters' `Value::String` equality, so two **distinct** string objects with the
same bytes are the **same key**: a key built by concatenation (`"a" + "b"`, a fresh
record) is found by a separately-built literal `"ab"`, and re-setting a
content-equal key **overwrites** the existing entry in place rather than appending a
duplicate. `map_set`/`map_get`/`map_has` route the find scan through the content
compare when the key type is `string`; insertion order and update-in-place-vs-append
semantics are otherwise identical to the scalar-key path.

This mirrors the interpreters' `Value::Map` — an **insertion-ordered association
list** scanned linearly with `Value` content equality — bit-for-bit:

- **`map_new() -> map<K, V>`** `__alloc`s an empty header `[len=0][cap=4][entries]`
  (a small initial capacity so the first few inserts do not each realloc) and
  leaves its pointer on the stack.
- **`map_set(m, k, v) -> map<K, V>`** is **value-semantic** (it returns a NEW map):
  it deep-copies `m` into a fresh block, then linear-scans (front-to-back, so the
  first matching key wins) for `k`. If `k` is present, it **overwrites that entry's
  value slot in place** — preserving the entry's position and the map's insertion
  order, exactly like the interpreters' `iter_mut().find(...)`. If `k` is absent,
  it **grows** the copy when full (doubling the capacity, or seeding the initial
  capacity from an empty map, copying the live entries and orphaning the old block)
  and **appends** a new `(k, v)` entry at index `len`, bumping `len`. Because
  `map_set` always copies before mutating, `m = map_set(m, k, v)` matches the
  interpreters' clone-then-mutate and no aliased binding observes the change.
- **`map_get(m, k) -> option<V>`** linear-scans for `k` and constructs `some(v)`
  (loading the found entry's value slot) or `none`, **reusing the option/enum
  linear-memory layout** (`[tag i32][payload slot]`) — so a matched `map_get`
  unwraps to the stored value and a miss unwraps to the `none` arm, identically to
  the interpreters.
- **`map_has(m, k) -> bool`** linear-scans and yields `found != len` (1 if present,
  0 if absent).
- **`map_len(m) -> i64`** loads the leading `i32` `len` header and sign-extends to
  `i64`.

**Value semantics.** A supported `map<K, V>` (scalar key; scalar or `string`
value) is classified as a **mutable aggregate**, so it is deep-copied when it
crosses a call boundary — a callee inserting into its parameter cannot alter the
caller's map. Combined with the copy-on-`map_set` discipline, `let b = a` (which
shares the `i32` pointer) is safe: any later `map_set` on either binding produces a
fresh block and reassigns that binding, so the other still points at the untouched
original. `emit_map_deep_copy` duplicates the `[len][cap][entries]` block (both
words of every live entry); a key is a scalar and a value is a scalar or a `string`
pointer (immutable, so shared), so a flat word copy is an exact deep copy.

**Lookup/ordering fidelity.** The scan visits entries in insertion order and
compares keys either with the slot-typed equality opcode (`i64.eq` for
`i64`/fixed-width keys, `i32.eq` for `bool`/`char`/`byte`, ordered `f*.eq` for
floats) or, for a `string` key, with the `emit_string_eq` **content** compare
(byte-length header then a `i32.load8_u` byte loop) — matching how the interpreters
compare `Value` keys by content. In-bounds programs agree bit-for-bit; there is no
out-of-bounds trap surface because every access is a header-bounded scan.

**Mutable-aggregate values (landed).** A `map<K, struct>` now compiles: the value
slot is an `i32` pointer, and the map's value-semantic deep copy RECURSES per entry
— `emit_map_copy_entries` copies the key word flat (a scalar or shared `string`)
and, for a mutable-aggregate value, loads the value pointer, `emit_deep_copy`s the
struct into a fresh record, and stores that fresh pointer. `map_get(m, k)` returns
`option<struct>` whose `some` payload is an **independent deep copy** of the stored
value (built directly so the option lays out a struct payload — see below), and
`map_set` deep-copies the incoming value before storing it, matching the
interpreters' clone semantics. `supported_map_kv` accepts a value that is a scalar,
a `string` (shared), or one level of mutable aggregate (`struct` / nested `list`);
the KEY stays scalar-or-`string`.

**Out of scope (deferred):** maps whose value is a fixed `array` or a `map`
(`map<K, array<…>>`, `map<K, map<…>>`), or nested past one mutable level
(`map<K, list<list<…>>>`) — such a value the backend cannot lay out, so
`supported_map_kv` rejects it and the enclosing function is skipped (still runs on
the interpreters) rather than miscompiled. (`map<string, V>`, `map<K, string>`, and
`map<K, struct>` are NO LONGER deferred. The semantic layer already restricts `map`
KEYS to `i64` or `string` — diagnostic `L0388` — so no non-string heap key ever
reaches this backend.) `map_keys`/`map_values` (which the interpreters return as
`Value::Array`) and `map_del` are also deferred to a later increment. `__alloc`
still never frees, so a grown or copied map orphans its old block.

Fixtures `wasm_map_build.lby` (build via `map_set` with an in-place key update,
crossing the initial capacity to trigger a grow+copy, then `map_get` matched
through its `option<V>`, `map_has`, and `map_len`, `main` = 5999509) and
`wasm_map_value_semantics.lby` (an aliased binding, an insert-derived map, an
update-derived map, and a callee that inserts into its parameter, `main` =
2231100), and `wasm_map_string_key.lby` (a `map<string, i64>` and a
`map<string, string>` built with concatenated/`to_string` keys, a content-equal
re-set that updates in place, and `map_get`/`map_has`/`map_len` — proving a
concatenation-built key matches a separately-built literal by content, `main` =
325634) run on all interpreters and, under node, their exported `main` matches
the interpreter
(`crates/lullaby_cli/tests/cli.rs::wasm_map_build_execution_parity_with_node`,
`wasm_map_value_semantics_execution_parity_with_node`, and
`wasm_map_string_key_execution_parity_with_node`).

### Mutable-heap collection elements/values (landed)

A growable `list`/`map` element/value (and an enum payload) may now be a **mutable
aggregate** — a named `struct`, or a one-level nested `list<scalar|string>` — not
just a scalar or immutable `string`. The key requirement is **value semantics**: the
collection's element/value deep-copy must RECURSE into a mutable-aggregate element
(deep-copy the element struct/list) rather than share its pointer, exactly like the
interpreters' recursive `Value::clone`. Immutable `string` elements stay shared;
scalar elements stay flat-copied.

- **Classification.** `collection_slot_type(ty, structs, enums, depth)` accepts a
  scalar, a `string` (shared), or — at `depth < MAX_COLLECTION_NEST_DEPTH` (1) — a
  `struct` (each field re-classified one level deeper) or a nested growable `list`
  (its element one level deeper). `supported_list_element`/`supported_map_kv`/
  `enum_layout` route through it, so `list<struct>`, `list<list<scalar|string>>`,
  `map<K, struct>`, and `option<struct>`/`result<struct, E>`/`result<i64, list<…>>`
  compile, while a fixed-`array` element, a `map` element, a mutable-aggregate map
  KEY, or nesting past one mutable level is skipped gracefully (the function still
  runs on the interpreters).
- **Recursive deep copy.** `emit_list_copy_elems`/`emit_map_copy_entries` take the
  element/value type: a scalar/`string` slot is a flat 8-byte word copy, a
  mutable-aggregate slot loads the element pointer, `emit_deep_copy`s it, and stores
  the fresh pointer. `emit_deep_copy_enum` flat-copies the record, then tag-branches
  to `emit_deep_copy` each mutable-aggregate payload of the actual variant.
- **Value-semantic reads and writes.** `get(l, i)` and `map_get`'s `some` payload
  return an **independent deep copy** of a mutable-aggregate element/value (the
  interpreters' `values[i].clone()` / value clone), so mutating the retrieved struct
  never affects the collection's stored element. `push`/`set`/`map_set` deep-copy the
  incoming mutable-aggregate value before storing it, so a later mutation of the
  source never leaks in. `match` binds a mutable-aggregate payload as a deep copy.

Fixture `wasm_list_struct.lby` builds a `list<Point>` (push structs, read a field,
`set` an element), a `list<list<i64>>` (nested, summed through nested `get`s), and a
`map<i64, Point>` (`map_set`/`map_get`→`option<Point>`/`map_len`). Its
value-semantics probe reads `get(ps, 2)`, mutates the retrieved struct's `.x`/`.y`,
then re-reads `get(ps, 2)` and confirms the ORIGINAL element is unchanged — proving
the per-element deep copy. It runs on all three interpreter backends (`main` =
503411108) and, under node, its exported `main` matches
(`crates/lullaby_cli/tests/cli.rs::wasm_list_struct_and_nested_and_map_struct_execution_parity_with_node`).

### Overflow-aware arithmetic (landed)

The overflow-aware builtins `checked_<op>`, `saturating_<op>`, and `wrapping_<op>`
for `add`/`sub`/`mul` compile on every fixed-width kind (`i8`…`u64`,
`isize`/`usize`; `i64` is excluded by the type checker), signed and unsigned:

- **`wrapping_*`** reuses the default fixed-width `+`/`-`/`*` (`i64.add`/`sub`/`mul`
  then re-normalize to the width) — the wrapping result.
- **`saturating_*`** and **`checked_*`** compute the wrapped result plus an overflow
  boolean using **comparison-only formulas** on the normalized operands (WASM has no
  host carry/overflow flags): e.g. unsigned add overflows iff `a >u MAX - b`, signed
  add iff `(b > 0 & a > MAX - b) | (b < 0 & a < MIN - b)`. The narrow multiply
  range-checks the exact product; the 64-bit multiply uses a **guarded division**
  test (`i64.div_u`/the guarded `div_s`) wrapped in a structured `if` on a zero
  divisor, so no case can trap. `saturating_*` selects the clamped bound with
  `select`; `checked_*` builds an `option<T>` record in linear memory (`some(result)`
  tag + payload, or `none`) reusing the enum/option layout — matched in place like a
  `map_get` result.

Results are bit-identical to the interpreters' `overflow_arith` for every width and
sign; node-parity-tested by
`crates/lullaby_cli/tests/cli.rs::wasm_overflow_arith_execution_parity_with_node`
(shared fixture `tests/fixtures/valid/run_overflow_codegen.lby`, `main` = 233).

### Scalar math builtins (landed)

The scalar math builtins lower to inline WASM opcode sequences (recognized by
name + arg count + operand type in the call lowerer, never emitted as real calls),
bit-for-bit with the interpreters (`builtin_sqrt`/`builtin_abs`/`builtin_min`/…
and `gcd_i64`) and matching the native backend's defer decisions:

- **`sqrt(x f64) -> f64`** is the single opcode `f64.sqrt` (0x9F) — correctly
  rounded IEEE-754, identical to the interpreters' `f64::sqrt` (a negative operand
  yields NaN). A `sqrt` node is reliably `f64`, so it is registered in
  `float_val_type_of` (like `to_f64`), keeping `sqrt(x) + y` on the float path.
- **`abs(x f64) -> f64`** is `f64.abs` (0x99) — the IEEE sign-bit clear
  (`|-0.0| = +0.0`, a NaN keeps its payload). **`abs(x i64) -> i64`** is the
  branchless two's-complement idiom `(x ^ (x >> 63)) - (x >> 63)` (`i64.shr_s` sign
  mask, `i64.xor`, `i64.sub`), matching release `i64::abs` — `abs(i64::MIN)` wraps
  to `i64::MIN` (the `i64.sub` wraps), consistent with the wrapping-arithmetic
  contract. `abs` follows its argument's width in `float_val_type_of` (f64 → float,
  i64 → integer), so both dispatch correctly.
- **`min(a, b)` / `max(a, b)` on `i64`** lower with `i64.lt_s` / `i64.gt_s` +
  `select` (0x1B): `min` is `a < b ? a : b`, `max` is `a > b ? a : b` — matching
  `i64::min` / `i64::max` (equal operands yield the equal value either way). The
  **`f64` case is DEFERRED** (it falls through to the interpreters): WASM's
  `f64.min` / `f64.max` NaN/`±0.0` tie-breaking diverges from Rust's
  `f64::min` / `f64::max`, so shipping it would not be bit-exact.
- **`gcd(a, b)` on `i64`** reduces each operand to its `u64` magnitude (the `abs`
  idiom, whose `i64::MIN` result reinterprets to the unsigned `2^63`) and runs
  unsigned-magnitude Euclid with `i64.rem_u` inside a `block`/`loop`, matching
  `gcd_i64` — including `gcd(i64::MIN, 0) = i64::MIN` (the loop exits immediately
  with `x = 2^63`, whose bits are `i64::MIN`).
- **`sign(x) -> i64`** (`-1`/`0`/`1`) on `i64` is two nested `select`s over
  `i64.lt_s` / `i64.gt_s` — `x < 0 ? -1 : (x > 0 ? 1 : 0)` — matching
  `i64::signum`. The **`f64` case is DEFERRED**.
- **`clamp(x, lo, hi) -> i64`** on `i64` is two nested `select`s comparing the
  ORIGINAL `x` — `x < lo ? lo : (x > hi ? hi : x)` — matching the interpreters'
  `if x < lo { lo } else if x > hi { hi } else { x }` for every ordering of
  `lo`/`hi` (including `lo > hi`, which yields `lo`). The **`f64` case is
  DEFERRED**.

The `i64`-only `min`/`max`/`sign`/`clamp` gates also reject a float-arithmetic
operand (which the IR annotates `i64`) via `float_val_type_of`, so a float value
never slips into the integer path. Default-deny: any operand shape not proven
bit-exact (the deferred `f64` cases, an `f32`/fixed-width operand) is skipped and
the enclosing function runs on the interpreters. The transcendental/rounding math
builtins (`sin`/`cos`/`floor`/`ceil`/`round`/`exp`/`ln`/…) stay deferred — they need
a library or a polynomial not bit-matchable to Rust — as does `pow`.

Verified by the structural encoder tests
`crates/lullaby_ir/src/wasm.rs::f64_sqrt_and_abs_emit_their_opcodes`,
`i64_math_builtins_compile_with_expected_opcodes`, and
`f64_min_max_sign_clamp_are_deferred`, and — under node — by
`crates/lullaby_cli/tests/cli.rs::wasm_math_builtins_execution_parity_with_node`
(fixture `tests/fixtures/valid/wasm_math_builtins.lby`, `main` = 70; it exercises
`sqrt`/`abs` on `f64`, `abs`/`min`/`max`/`gcd`/`sign`/`clamp` on `i64`, and
`gcd(i64::MIN, 0)`). `abs(i64::MIN)` also wraps correctly on WASM (proven by direct
node runs), but is not folded through the interpreter ground truth because
`i64::abs` panics on that single input under a debug/overflow-checked build.

## First increment — the scalar subset

WASM has a clean core (functions, `i32`/`i64`/`f32`/`f64`, structured control
flow, a stack machine) but no built-in strings, records, or GC. Modeling
Lullaby's heap types (`string`, `struct`, `enum`, `array`, `list`, `map`,
`option`, `result`) requires laying them out in **linear memory** — a large
second phase. So the first increment compiles the **scalar subset** only:

- Types: `i64` → wasm `i64`, `f64` → wasm `f64`, `bool` → wasm `i32` (0/1),
  `char`/`byte` → wasm `i32`. `void` → no result.
- Functions: any top-level function whose parameter and return types are all in
  the scalar subset compiles to a WASM function and is exported by name.
- Expressions: integer/float/bool literals, variables (params + `let` locals),
  arithmetic (`+ - * /`; signed integer division still traps on a zero divisor
  like WASM `div_s`, but the `i64::MIN / -1` overflow case is guarded so it wraps
  to `i64::MIN` — matching the interpreters — instead of trapping),
  comparisons, `and`/`or`/`not`, and calls to other compiled functions.
- Statements: `let`, assignment, `return`, `if`/`elif`/`else`, `while`, `loop`
  with `break`/`continue`, and range `for` (lowered to a loop). These map to
  WASM's structured `block`/`loop`/`br`/`br_if`/`if`.
- A function that uses a `list`/`map`/enum nested past one **mutable** heap level, a
  `list`/`map` element/value (or enum payload) that is a fixed `array` or a `map`, a
  `map` with a mutable-aggregate KEY, a runtime string builder other than `+` concat,
  or any type still outside the supported set is **rejected for WASM** with a clear
  diagnostic (it still runs on the interpreters). Note: later increments added enum
  values and `match` for scalar-/`string`-/one-level-mutable-payload enums
  (`option`/`result`/user enums, including `option<struct>` and
  `result<i64, list<i64>>`), the growable `list<T>` collection for scalar,
  `string`, and one-level mutable-aggregate element types (`list<struct>`,
  `list<list<scalar|string>>`), the `map<K, V>` collection for a scalar or `string`
  key (string keys compared by content) and a scalar, `string`, or `struct` value
  (`map<K, struct>`), and runtime `string` `+` concatenation — see the linear-memory
  sections above. The allowed builtins are `wasm_log(x i64) -> void` (the host log
  import above), `console_log(s string) -> void` and `dom_set_text(id string, text
  string) -> void` (the JS/DOM host imports above), `len(string|array|list) -> i64`,
  the `list` builtins `list_new`/`push`/`get`/`set`/`pop`, the `map` builtins
  `map_new`/`map_set`/`map_get`/`map_has`/`map_len`, `to_string` (non-float
  arguments), the index-based string operations
  `substring`/`find`/`contains`/`starts_with`/`ends_with` (see **Heap types
  (landed)** above), the overflow-aware arithmetic builtins
  `checked_<op>`/`saturating_<op>`/`wrapping_<op>` for `add`/`sub`/`mul` on the
  fixed-width kinds (see **Overflow-aware arithmetic (landed)** below), and the
  scalar math builtins `sqrt`/`abs` on `f64`, `abs` on `i64`, and
  `min`/`max`/`gcd`/`sign`/`clamp` on `i64` (the `f64` cases of
  `min`/`max`/`sign`/`clamp` are deferred — see **Scalar math builtins (landed)**
  above); every other builtin (transcendental math `sin`/`cos`/`floor`/…, `pow`,
  …) is still rejected. Strings, structs, fixed
  arrays, lists of scalar/`string`/`struct`/nested-list elements, and maps with a
  scalar or `string` key and a scalar, `string`, or `struct` value are now supported
  — see **Heap types (landed)**, **Growable `list<T>` (landed)**, **Growable
  `map<K, V>` (landed)**, and **Mutable-heap collection elements/values (landed)**
  above.

## From IR to WASM

Compile from the **typed IR** (`lullaby_ir`), not the AST — types are already
resolved. A new crate/module (e.g. `crates/lullaby_wasm` or a `wasm` module in
`lullaby_ir`) walks each eligible `IrFunction`:

- Map IR value types to WASM value types as above.
- Parameters and `let` bindings become WASM locals; keep a name→local-index map.
- Emit the function body as a stack-machine instruction sequence (an expression
  pushes its value; a binary op emits its operands then the op; `if`/loops use
  structured control flow with explicit result types).
- A **value-producing tail `if`/`elif`/`else`** (every reachable branch, including
  the final `else`, ends in a value expression of the same type) emits each WASM
  `if` with that value's **block result type** so the branch value is left on the
  stack (mirroring the typed-`if` chain used by value-producing `match`). A
  statement `if`, or one with no `else`, keeps the void (`0x40`) block type. The
  value-vs-statement decision comes from the IR branch bodies' tails
  (`if_result_type`), matching how the interpreters and native backend decide.
- Emit a **binary `.wasm` module** directly (no external crate): the standard
  encoding — magic + version, then the Type, Import, Function, Memory, Global,
  Export, Code, and Data sections in canonical id order, with LEB128-encoded
  integers. This is well-specified and dependency-free. (A `.wat` text option can
  come later; binary runs everywhere.)

## CLI

- `lullaby wasm [--verbose] [-o out.wasm] <file.lby>` — compile the eligible
  functions of a source file to a `.wasm` module. Report which functions were
  compiled and which were skipped (non-scalar) and why.
- The command validates and lowers to IR exactly as `compile` does, then runs
  the WASM emitter over the eligible functions.

## Testing (the WASM verification story)

The parity harness compares interpreter results; to check emitted WASM we need a
WASM runtime, which we will not take as a Cargo dependency. Strategy:

- For a fixture, obtain the ground-truth result from the interpreter
  (`lullaby run`).
- Emit the `.wasm`, then execute it with an EXTERNAL tool **if available** —
  probe for `node` (via `WebAssembly.instantiate` in a tiny generated script) or
  `wasmtime`/`wasm-tools`. Assert the exported function's result equals the
  interpreter's.
- If no runtime is found on the machine, the WASM-execution test **skips
  gracefully** (documented), while the emitter's structural output is still
  unit-tested (valid magic/version, sections present, function count) so the
  encoder itself is always covered.

This keeps the interpreters as the correctness anchor and verifies real WASM
execution wherever a runtime exists (CI can install one).

## Scope and sequencing

First increment (DELIVERED): the scalar subset above, binary `.wasm` output, the
`wasm` CLI command, structural encoder tests, and node-gated execution parity.
Linear-memory step (DELIVERED): exported `"memory"`, a mutable bump-pointer
global, a seeded Data section, the internal `__alloc` helper, and the
`env.log_i64` host import surfaced as `wasm_log` with node-gated call-sequence
parity. Heap types (DELIVERED): `string` literals and `len(s)` in the Data
section, `struct` and fixed `array` construction/field/index load-store through
`__alloc`, and `len(a)` — see **Heap types (landed)**. The
`tests/fixtures/valid/wasm_heap.lby` fixture runs on all interpreters (`main` =
133) and, under node, its exports and the interned string layout in `memory`
match (`crates/lullaby_cli/tests/cli.rs::wasm_heap_types_execution_parity_with_node`).
JS/DOM host interop (DELIVERED): the `console_log`/`dom_set_text` host imports
surfaced as builtins — see **JS/DOM host interop (landed)**. The
`tests/fixtures/valid/wasm_interop.lby` fixture runs on all interpreters (`main` =
22, printing the console/dom lines) and, under node, the harness decodes each
`(ptr, len)` string out of `memory` and asserts the captured `console_log`/
`dom_set_text` strings and the exported `main` match the interpreter
(`crates/lullaby_cli/tests/cli.rs::wasm_js_dom_interop_execution_parity_with_node`).
Full-stack path (DEMONSTRATED): `examples/valid/fullstack/` compiles one `shared`
domain module (kept inside the scalar+string WASM-eligible surface) two ways — a
WASM `frontend.lby` that renders shared classification labels through
`console_log`/`dom_set_text` (built with `lullaby wasm frontend.lby` and loaded by
a self-contained `index.html` supplying the `env.*` imports), and a pure-Lullaby
HTTP `backend.lby` that serves the same shared `classify`/`priority_score` values
on `/classify`. It reuses the delivered imports without any backend change; the
CLI tests `fullstack_frontend_wasm_matches_shared_logic` (WASM emit + node-gated
render/score parity, `main` = 4) and `fullstack_shared_logic_round_trip` (real
HTTP client on all three backends) prove both sides agree with the interpreter.
Enum values and `match` for scalar-or-`string`-payload enums
(`option`/`result`/user enums, tag+payload linear-memory records, branch-on-tag
dispatch) now compile and are node-parity-tested
(`crates/lullaby_cli/tests/cli.rs::wasm_enum_match_execution_parity_with_node`,
fixture `tests/fixtures/valid/wasm_enum_match.lby`). Growable `list<T>` for
**scalar and `string` element types** (`[len][cap][slots]` linear-memory blocks
with value-semantic `list_new`/`push`/`get`/`set`/`len`/`pop` and capacity-doubling
grow+copy; a `string` element is a shared immutable pointer word) now compiles and
is node-parity-tested — see **Growable `list<T>` (landed)** above (fixtures
`wasm_list_build.lby`, `wasm_list_value_semantics.lby`, `wasm_list_string.lby`).
Growable `map<K, V>` for a **scalar or `string` key and a scalar or `string`
value** (`[len][cap][(k,v) pairs]` linear-memory blocks — an insertion-ordered
association list with value-semantic
`map_new`/`map_set`/`map_get`/`map_has`/`map_len`, in-place key updates, and
capacity-doubling grow+copy, mirroring the interpreters' `Value::Map`; a `string`
key is compared by **content** — `byte_len` header then a byte loop — so two
distinct string objects with equal bytes are the same key, and a `string` value is
a shared immutable pointer while `map_get` returns `option<string>`) now compiles
and is node-parity-tested — see **Growable `map<K, V>` (landed)** above (fixtures
`wasm_map_build.lby`, `wasm_map_value_semantics.lby`, `wasm_map_string_key.lby`,
`wasm_list_string.lby`). Runtime `string` `+`
concatenation now compiles: the string record gained a second `byte_len` header
(`[char_len][byte_len][utf8]`) so a fresh record can be `__alloc`'d and the two
operands' UTF-8 byte ranges `memory.copy`'d in, handling multi-byte text — see
**Heap types (landed) → Strings** above. It is node-parity-tested
(`crates/lullaby_cli/tests/cli.rs::wasm_string_concat_execution_parity_with_node`,
fixture `tests/fixtures/valid/wasm_string_concat.lby`, `main` = 33).
`to_string(x)` now compiles for integer/`bool`/`char`/`byte`/`string` arguments
(in-WASM itoa for integers incl. `i64::MIN`/`u64::MAX`, interned `true`/`false`,
1–4 byte UTF-8 char encoding, and the string identity), node-parity-tested
(`crates/lullaby_cli/tests/cli.rs::wasm_to_string_execution_parity_with_node`,
fixture `tests/fixtures/valid/wasm_to_string.lby`, `main` = 78). The index-based
string operations now compile: char-indexed `substring`/`find` (which decode UTF-8
to map char indices to byte offsets) and byte-exact `contains`/`starts_with`/
`ends_with`, matching the interpreters' `builtin_substring`/`char_find`/etc.
bit-for-bit — see **Heap types (landed) → Strings** above. They are
node-parity-tested
(`crates/lullaby_cli/tests/cli.rs::wasm_string_ops_execution_parity_with_node`,
fixture `tests/fixtures/valid/wasm_string_ops.lby`, `main` = 11, including a
multi-byte string). Mutable-heap collection elements/values now compile for one
level of nesting — `list<struct>`, `list<list<scalar|string>>`, `map<K, struct>`,
and an enum with a `struct`/one-level-`list` payload (`option<struct>`,
`result<i64, list<i64>>`) — with recursive per-element/value/payload deep copy
matching the interpreters, node-parity-tested
(`crates/lullaby_cli/tests/cli.rs::wasm_list_struct_and_nested_and_map_struct_execution_parity_with_node`,
fixture `tests/fixtures/valid/wasm_list_struct.lby`, `main` = 503411108) — see
**Mutable-heap collection elements/values (landed)** above. Deferred: a `list`/`map`
element/value/enum-payload that is a fixed `array` or a `map`, a `map` with a
mutable-aggregate KEY, nesting past one mutable level (`list<list<list<…>>>`,
`map<K, map<…>>`), `map_keys`/`map_values`/`map_del`, `to_string` of a **float**
(`f32`/`f64`) and the string builders not yet lowered — `replace`, `upper`/`lower`
(Unicode case mapping is hard to match Rust), `split`/`join`,
`chars`/`string_from_chars` — a free-list allocator, and a richer DOM interop
surface (reading DOM values, events) that builds on these imports.

## Why these choices

- **Compile the IR, not the AST**: types are resolved and control flow is
  normalized, so lowering is a direct walk.
- **Scalar subset first**: delivers real, runnable WASM (numeric/logic functions)
  without the large linear-memory design, and proves the encoder end to end.
- **Emit binary WASM with std only**: no dependency, runs in any WASM host; the
  encoding is small and well-specified.
- **Interpreter as ground truth**: reuses the existing correctness model; the
  WASM test just asserts equality where a runtime is available.
