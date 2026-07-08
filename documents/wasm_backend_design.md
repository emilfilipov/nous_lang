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
  it pushes the string's linear-memory pointer and its length header, then calls
  the import. A browser host implements it as `console.log`.
- `dom_set_text(id string, text string) -> void` lowers to
  `env.dom_set_text(id_ptr i32, id_len i32, text_ptr i32, text_len i32)`: it
  pushes each string's pointer and length in order, then calls the import. A
  browser host implements it as
  `document.getElementById(id).textContent = text`.
- Each string operand is evaluated once into a scratch `i32` local, then pushed as
  `(ptr, len)` where `len` is the `i32` length header of the interned
  `[len i32][utf8 bytes]` layout (the char count, equal to the byte length for
  ASCII). The host decodes the bytes out of `memory` starting at `ptr + 4`.
- On the interpreters, `console_log` prints the string as a stdout line and
  `dom_set_text` prints `id=text`, so all backends observe the same side effect
  and the parity harness stays green. Functions calling these builtins are
  eligible for WASM (the eligibility gate accepts them alongside `wasm_log` and
  `len`).

### Heap types (landed)

`string`, `struct`, and fixed `array` values are **`i32` pointers** into linear
memory. Their WASM slot type is the scalar's own type for a scalar, or `i32` for
a pointer (nested strings/structs/arrays).

- **Strings:** a `string` is a pointer to `[len: i32 char-count][utf8 bytes]`.
  Each distinct string literal is interned ONCE into the Data section (a constant
  static offset is its value). `len(s)` lowers to `i32.load` of the header then
  `i64.extend_i32_s` (the builtin returns `i64`, char count to match the
  interpreters). Runtime string building (`+` concat, `to_string`, `substring`,
  …) is not yet lowered — a function using it is skipped.
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

**Deferred:** enums/tagged unions and `match` lowering (the tag+payload memory
representation and branch-on-tag); the built-in generic enums `option`/`result`;
the growable `list`/`map` collections; runtime string construction; and a
free-list allocator (`__alloc` never frees this increment). Functions using any
of these are skipped with a reason and still run on the interpreters.

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
- A function that uses an enum/`match`, `option`/`result`/`list`/`map`, a runtime
  string builder, or any type still outside the supported set is **rejected for
  WASM** with a clear diagnostic (it still runs on the interpreters). The allowed
  builtins are `wasm_log(x i64) -> void` (the host log import above),
  `console_log(s string) -> void` and `dom_set_text(id string, text string) ->
  void` (the JS/DOM host imports above), and `len(string|array) -> i64`; every
  other builtin is still rejected. Strings, structs, and fixed arrays are now
  supported — see **Heap types (landed)** above.

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
Deferred: enum/tagged-union + `match` lowering (tag+payload memory, branch on
tag), `option`/`result`, growable `list`/`map`, runtime string construction, a
free-list allocator, and a richer DOM interop surface (reading DOM values,
events) that builds on these imports.

## Why these choices

- **Compile the IR, not the AST**: types are resolved and control flow is
  normalized, so lowering is a direct walk.
- **Scalar subset first**: delivers real, runnable WASM (numeric/logic functions)
  without the large linear-memory design, and proves the encoder end to end.
- **Emit binary WASM with std only**: no dependency, runs in any WASM host; the
  encoding is small and well-specified.
- **Interpreter as ground truth**: reuses the existing correctness model; the
  WASM test just asserts equality where a runtime is available.
