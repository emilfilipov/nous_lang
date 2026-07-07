# WebAssembly Backend Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This note designs a WebAssembly (WASM) backend that compiles Lullaby's typed IR
to a real `.wasm` binary module. It is the highest-leverage step toward the web
path: a `.wasm` module runs in every browser and in server-side WASM runtimes.
The interpreters remain the correctness ground truth; the WASM backend must
produce the same results.

## Status

**The scalar-subset first increment is DELIVERED, plus the first linear-memory
step (memory/data/import + `wasm_log`).** It ships as a `wasm` module in
`crates/lullaby_ir` (`crates/lullaby_ir/src/wasm.rs`, `emit_wasm_module`), the
`lullaby wasm [--verbose] [-o out.wasm] <file.lby>` CLI command, structural
encoder unit tests, and node-gated execution-parity tests against the
interpreter (`crates/lullaby_cli/tests/cli.rs`). The encoder writes the module
header, the Type/Import/Function/Memory/Global/Export/Code/Data sections in
canonical order, LEB128 integers, and the stack-machine opcodes it needs — using
the Rust standard library only, no external crate. When no function is eligible,
the CLI reports diagnostic `L0338`.

### Linear-memory step (landed)

- **Memory section (id 5)** declares one linear memory (min 1 page, 64 KiB) and
  the **Export section** exports it as `"memory"` (export kind mem).
- **Global section (id 6)** declares a mutable `i32` bump pointer initialized to
  a heap base past a small reserved region.
- **Data section (id 11)** seeds the reserved low-memory region at a constant
  offset, so a freshly handed-out offset is never `0` (null).
- An internal `__alloc(size i32) -> i32` bump-allocator helper reads the global,
  advances it by `size`, and returns the old offset. It is groundwork; the scalar
  subset does not call it yet.
- **Import section (id 2)** imports the host function
  `env.log_i64 (func (param i64))` and exposes it to Lullaby as the builtin
  `wasm_log(x i64) -> void`. A `wasm_log(n)` call lowers to a `call` of the
  imported function, which makes eligible functions side-effecting. On the
  interpreters (AST/IR/bytecode) `wasm_log` prints the value as a stdout line, so
  cross-backend parity holds.
- **Import index fix-up:** imported functions occupy the LOW WASM function
  indices (`0..IMPORT_FUNC_COUNT`), so every internally-defined function is
  numbered from `IMPORT_FUNC_COUNT` up. Call targets between compiled functions
  and the function-export indices are shifted by the import count; the imported
  `env.log_i64` is index `0`.

Full string/struct/enum/array layout in linear memory (using this allocator and
memory) remains deferred.

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
  arithmetic (`+ - * /`, integer division traps on 0 like WASM `div_s`),
  comparisons, `and`/`or`/`not`, and calls to other compiled functions.
- Statements: `let`, assignment, `return`, `if`/`elif`/`else`, `while`, `loop`
  with `break`/`continue`, and range `for` (lowered to a loop). These map to
  WASM's structured `block`/`loop`/`br`/`br_if`/`if`.
- A function that uses any non-scalar type, `match` over an enum, or a heap value
  is **rejected for WASM** with a clear diagnostic (it still runs on the
  interpreters); those await the linear-memory phase. The one allowed builtin is
  `wasm_log(x i64) -> void` (the host log import above); every other builtin is
  still rejected.

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
parity. Deferred (rest of the linear-memory phase): `string`/`struct`/`enum`/
`array` layout on top of this allocator, a free-list allocator, `match` lowering,
collections, and a richer JS/DOM interop layer (imports for `console.log`/DOM)
that builds on `wasm_log`.

## Why these choices

- **Compile the IR, not the AST**: types are resolved and control flow is
  normalized, so lowering is a direct walk.
- **Scalar subset first**: delivers real, runnable WASM (numeric/logic functions)
  without the large linear-memory design, and proves the encoder end to end.
- **Emit binary WASM with std only**: no dependency, runs in any WASM host; the
  encoding is small and well-specified.
- **Interpreter as ground truth**: reuses the existing correctness model; the
  WASM test just asserts equality where a runtime is available.
