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
  - Other runtime string builders (`to_string`, `substring`, `find`, `replace`,
    `upper`/`lower`, `split`/`join`) are not yet lowered — a function using one is
    skipped and still runs on the interpreters.
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

- **Enums (scalar payloads):** an enum value is a pointer to
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
  before its body. This covers the built-in `option<T>`/`result<T, E>` when
  `T`/`E` are scalar and user enums whose every variant payload is scalar.

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
  are always scalar, so a flat copy is an exact deep copy.
- **string:** NOT copied. Strings are immutable, so sharing the pointer is already
  value-equivalent to the interpreters' clone.

A **returned** aggregate is the callee's own fresh record, so no extra copy is
needed on return. Fixtures `wasm_aggregate_args.lby` and
`wasm_aggregate_nested.lby` exercise struct/array take+return plus value-semantics
probes (a callee mutates its parameter; the caller's copy is verified unchanged),
node-gated against the interpreter.

**Deferred:** aggregates containing heap values the backend does not lay out (a
`list` with a heap element, or `map`, or an enum with a heap payload); enums with
a **heap** payload (`string`/`list`/`array`/`map` — notably `result<i64,
string>`); the `map` collection and lists of heap elements; runtime string
builders other than `+` concat (`to_string`, `substring`, `find`, `replace`,
`upper`/`lower`, `split`/`join`); and a free-list allocator (`__alloc` never frees
this increment).
Functions using any of these are skipped with a reason and still run on the
interpreters.

### Growable `list<T>` — scalar elements (landed)

The growable, value-semantic `list<T>` collection compiles to linear memory for
**scalar element types** (`i64`, the fixed-width ints `i8`…`usize`, `f32`/`f64`,
`bool`, `char`, `byte`). A `list<T>` is an **`i32` pointer** to a header
`[len: i32][cap: i32][elem slots...]`: the live element count, the allocated
capacity, then `cap` uniform 8-byte element slots (`SLOT_SIZE`, like struct/array
elements, so a scalar element stays naturally aligned and element `i` lives at
`LIST_DATA_OFF + i * SLOT_SIZE`). The `len` field shares offset 0 with the
string/array length header, so `len(l)` reuses the array length path unchanged.

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

**Value semantics.** A `list<T>` (scalar element) is classified as a **mutable
aggregate**, so it is deep-copied when it crosses a call boundary — a callee
pushing to its parameter cannot alter the caller's list. Combined with the
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

**Out of scope (deferred):** lists of **heap** elements
(`list<string>`/`list<struct>`/`list<list<…>>`/`list<map<…>>`) — the element must
be scalar this increment, so `supported_list_element` rejects a heap element and
the enclosing function is skipped (still runs on the interpreters) rather than
miscompiled. `__alloc` still never frees, so a grown or copied list orphans its
old block (a free-list allocator is future work).

Fixtures `wasm_list_build.lby` (build via `push` crossing the initial capacity to
trigger a grow+copy, then `get`/`len`/`set`/`pop`, `main` = 5879) and
`wasm_list_value_semantics.lby` (an aliased binding, a push-derived list, a
set-derived list, and a callee that pushes to its parameter, `main` = 334211) run
on all interpreters and, under node, their exported `main` matches the interpreter
(`crates/lullaby_cli/tests/cli.rs::wasm_list_build_execution_parity_with_node` and
`wasm_list_value_semantics_execution_parity_with_node`).

### Growable `map<K, V>` — scalar keys and values (landed)

The value-semantic `map<K, V>` collection compiles to linear memory for **scalar
key and value types** (`i64`, the fixed-width ints `i8`…`usize`, `f32`/`f64`,
`bool`, `char`, `byte`). A `map<K, V>` is an **`i32` pointer** to a header
`[len: i32][cap: i32][(key, value) slot pairs...]`: the live entry count, the
allocated capacity (in entries), then `cap` entry records. Each entry is two
uniform 8-byte slots — the key slot then the value slot (`MAP_ENTRY_SIZE =
2 * SLOT_SIZE`) — so entry `i` lives at `MAP_DATA_OFF + i * MAP_ENTRY_SIZE`, its
key at offset `0` and its value at `MAP_VALUE_OFF`. Uniform 8-byte slots keep
every scalar key/value naturally aligned. `len` shares offset 0 with the
string/array/list length header.

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

**Value semantics.** A scalar-key/value `map<K, V>` is classified as a **mutable
aggregate**, so it is deep-copied when it crosses a call boundary — a callee
inserting into its parameter cannot alter the caller's map. Combined with the
copy-on-`map_set` discipline, `let b = a` (which shares the `i32` pointer) is safe:
any later `map_set` on either binding produces a fresh block and reassigns that
binding, so the other still points at the untouched original. `emit_map_deep_copy`
duplicates the `[len][cap][entries]` block (both words of every live entry); map
keys and values are always scalar, so a flat word copy is an exact deep copy.

**Lookup/ordering fidelity.** The scan visits entries in insertion order and
compares keys with the slot-typed equality opcode (`i64.eq` for `i64`/fixed-width
keys, `i32.eq` for `bool`/`char`/`byte`, ordered `f*.eq` for floats), matching how
the interpreters compare `Value` keys by content. In-bounds programs agree
bit-for-bit; there is no out-of-bounds trap surface because every access is a
header-bounded scan.

**Out of scope (deferred):** maps with a **heap** key or value
(`map<string, V>`, `map<K, string>`, `map<K, list<…>>`, `map<K, struct>`, …) — the
key and value must both be scalar this increment, so `supported_map_kv` rejects a
heap key/value and the enclosing function is skipped (still runs on the
interpreters) rather than miscompiled. String keys are deferred specifically
because the interpreters compare keys by decoded **content**, not the interned
pointer; content comparison of two strings in linear memory is future work.
`map_keys`/`map_values` (which the interpreters return as `Value::Array`) and
`map_del` are also deferred to a later increment. `__alloc` still never frees, so a
grown or copied map orphans its old block.

Fixtures `wasm_map_build.lby` (build via `map_set` with an in-place key update,
crossing the initial capacity to trigger a grow+copy, then `map_get` matched
through its `option<V>`, `map_has`, and `map_len`, `main` = 5999509) and
`wasm_map_value_semantics.lby` (an aliased binding, an insert-derived map, an
update-derived map, and a callee that inserts into its parameter, `main` =
2231100) run on all interpreters and, under node, their exported `main` matches
the interpreter
(`crates/lullaby_cli/tests/cli.rs::wasm_map_build_execution_parity_with_node` and
`wasm_map_value_semantics_execution_parity_with_node`).

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
- A function that uses a `list` or `map` with a heap element/key/value, an enum
  with a heap payload, a runtime string builder other than `+` concat, or any type
  still outside the supported set is **rejected for WASM** with a clear diagnostic
  (it still runs on the interpreters). Note: later increments added enum values and
  `match` for scalar-payload enums (`option`/`result`/user enums), the growable
  `list<T>` collection for scalar element types, the `map<K, V>` collection for
  scalar key/value types, and runtime `string` `+` concatenation — see the
  linear-memory sections above. The allowed builtins are
  `wasm_log(x i64) -> void` (the host log import above), `console_log(s string) ->
  void` and `dom_set_text(id string, text string) -> void` (the JS/DOM host imports
  above), `len(string|array|list) -> i64`, the scalar-element `list` builtins
  `list_new`/`push`/`get`/`set`/`pop`, and the scalar-key/value `map` builtins
  `map_new`/`map_set`/`map_get`/`map_has`/`map_len`; every other builtin is still
  rejected. Strings, structs, fixed arrays, scalar-element lists, and
  scalar-key/value maps are now supported — see **Heap types (landed)**, **Growable
  `list<T>` (landed)**, and **Growable `map<K, V>` (landed)** above.

## From IR to WASM

Compile from the **typed IR** (`lullaby_ir`), not the AST — types are already
resolved. A new crate/module (e.g. `crates/lullaby_wasm` or a `wasm` module in
`lullaby_ir`) walks each eligible `IrFunction`:

- Map IR value types to WASM value types as above.
- Parameters and `let` bindings become WASM locals; keep a name→local-index map.
- Emit the function body as a stack-machine instruction sequence (an expression
  pushes its value; a binary op emits its operands then the op; `if`/loops use
  structured control flow with explicit result types).
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
Enum values and `match` for scalar-payload enums (`option`/`result`/user enums,
tag+payload linear-memory records, branch-on-tag dispatch) now compile and are
node-parity-tested
(`crates/lullaby_cli/tests/cli.rs::wasm_enum_match_execution_parity_with_node`,
fixture `tests/fixtures/valid/wasm_enum_match.lby`). Growable `list<T>` for
**scalar element types** (`[len][cap][slots]` linear-memory blocks with
value-semantic `list_new`/`push`/`get`/`set`/`len`/`pop` and capacity-doubling
grow+copy) now compiles and is node-parity-tested — see **Growable `list<T>`
(landed)** above (fixtures `wasm_list_build.lby`, `wasm_list_value_semantics.lby`).
Growable `map<K, V>` for **scalar key/value types** (`[len][cap][(k,v) pairs]`
linear-memory blocks — an insertion-ordered association list with value-semantic
`map_new`/`map_set`/`map_get`/`map_has`/`map_len`, in-place key updates, and
capacity-doubling grow+copy, mirroring the interpreters' `Value::Map`) now compiles
and is node-parity-tested — see **Growable `map<K, V>` (landed)** above (fixtures
`wasm_map_build.lby`, `wasm_map_value_semantics.lby`). Runtime `string` `+`
concatenation now compiles: the string record gained a second `byte_len` header
(`[char_len][byte_len][utf8]`) so a fresh record can be `__alloc`'d and the two
operands' UTF-8 byte ranges `memory.copy`'d in, handling multi-byte text — see
**Heap types (landed) → Strings** above. It is node-parity-tested
(`crates/lullaby_cli/tests/cli.rs::wasm_string_concat_execution_parity_with_node`,
fixture `tests/fixtures/valid/wasm_string_concat.lby`, `main` = 33). Deferred:
enums with a heap payload (`string`/`list`/`array`/`map`, e.g. `result<i64,
string>`), lists of heap elements, maps with a heap key or value (including string
keys), `map_keys`/`map_values`/`map_del`, runtime string builders other than `+`
concat (`to_string`, `substring`, `find`, `replace`, `upper`/`lower`,
`split`/`join`), a free-list allocator, and a richer DOM interop surface (reading
DOM values, events) that builds on these imports.

## Why these choices

- **Compile the IR, not the AST**: types are resolved and control flow is
  normalized, so lowering is a direct walk.
- **Scalar subset first**: delivers real, runnable WASM (numeric/logic functions)
  without the large linear-memory design, and proves the encoder end to end.
- **Emit binary WASM with std only**: no dependency, runs in any WASM host; the
  encoding is small and well-specified.
- **Interpreter as ground truth**: reuses the existing correctness model; the
  WASM test just asserts equality where a runtime is available.
