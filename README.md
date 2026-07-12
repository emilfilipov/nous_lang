<p align="center">
  <img src="assets/brand/lullaby-wordmark.png" alt="Lullaby" width="360">
</p>

<p align="center"><strong>Serious systems code. Sweet dreams.</strong></p>

<hr>

Lullaby is a compiled, statically typed systems programming language with an
indentation-only syntax. It is designed for concise, readable source that is
strongly typed, memory-safety aware, and easy for both humans and LLMs to
generate. Lullaby is in active development toward a 1.0 release; the toolchain
already runs real programs end to end across five backends.

The compiler runs Lullaby on five parity-checked backends — an AST interpreter,
a typed-IR interpreter, an instruction-bytecode VM (with an optimizer), a
WebAssembly emitter, and a native x86-64 code generator (with linking,
freestanding / no-std builds, and inline assembly) — plus a versioned `.lbc`
bytecode artifact path, an editor language server, a test runner, and a
self-contained offline documentation bundle.

## Benchmarks

Lullaby is measured against a **434-function cross-language corpus** (26
categories, the same programs written idiomatically in six languages) for token
efficiency, plus C-referenced workloads for native performance. Full methodology
and an interactive report live in [`benchmarks/crosslang/`](benchmarks/crosslang/).

**Token efficiency** — Lullaby is the tersest language measured except Python,
and the tersest *statically-typed* one (o200k_base tokens, definitions only):

| Language    | Tokens | vs Lullaby |
| ----------- | -----: | ---------: |
| Python      | 20,112 |      −4.5% |
| **Lullaby** | **21,058** |      — |
| JavaScript  | 23,432 |     +11.3% |
| Rust        | 26,329 |     +25.0% |
| C           | 27,472 |     +30.5% |
| C++         | 28,423 |     +35.0% |

That is ~10% terser than JavaScript, ~20% than Rust, ~23% than C, and ~26% than
C++ — within +4.7% of Python while keeping full static typing. The remaining gap
is concentrated in string/parse-heavy code; on structured and numeric code
Lullaby already beats Python.

**Native performance** (x86-64, best-of-N whole-program wall time vs `cl /O2`):

- Prime sieve: **at parity with C** (0.99–1.01×, at/below C++) and **17–27×
  faster than CPython**.
- SIMD auto-vectorization of `i64` array reductions and maps (`+`, `-`,
  `& | ^`): **2.89–3.36× faster** than the scalar loop, bit-for-bit identical.
- Deep recursion (`fib(35)`): **~1.00× C — at parity** (from 1.26×): the per-call
  `if n < 2` compares the promoted register directly (no `rax` reload), and the
  recursive args `fib(n-1)`/`fib(n-2)` form with one `lea rcx,[rbx-k]`, as C does.
- Tight counting-`sum` loop: **0.52× C — faster than C** — the backend
  ILP-unrolls `while i < N: acc = acc + i; i = i + 1`, folding four iterations
  per step into one dependent add and breaking the serial `acc` chain.

**Interpreter performance**: the AST / IR / bytecode tiers are development tools
(~200–1200× C). Range `for`-loops run **~2× faster** across all three after
binding the loop variable once instead of per-iteration, and **slot-based
variable resolution** now indexes each local read by `(depth, slot)` on the IR
and bytecode tiers instead of scanning names — so those tiers run **faster than
the AST tree-walker** (which still name-scans) on numeric code. A **tighter
`Value` cell** (boxing the four largest variants) shrank the shared per-op
footprint from **32 to 24 bytes**, cutting every clone/move — the AST tier
dropped ~7% on the sum loop (1.30× → 1.20× C). And the **bytecode tier now runs
a flat VM** — a `loop { match }` dispatch over a linear op stream with
slot-indexed locals, instead of the recursive tree-walk — making it **2× faster
than the tree-walkers** on tight loops and **~23% faster across the whole
corpus**, distinctly the fastest interpreter tier.

<sub>Measured on Windows/MSVC; compiled tiers at `-O2`/release, CPython for
Python. Regenerate with `benchmarks/crosslang/run_benchmark.ps1`.</sub>

## Language At A Glance

The implemented surface runs identically on the interpreter backends (with the
WebAssembly and native backends supporting growing subsets):

- **Types**: `i64`, the fixed-width integers `i8`/`i16`/`i32`/`u8`/`u16`/`u32`/`u64`
  and `isize`/`usize`, `f64` and `f32` (with typed literal suffixes like `5i32` /
  `1.5f32`), `bool`, `string`, `char`, `byte`, `void`; fixed `array<T>`, growable
  `list<T>`, and `map<K, V>`; nominal `struct` and `enum` (tagged unions); the
  built-in generic enums `option<T>` and `result<T, E>`; function values
  `fn(T) -> R`; and `rc<T>` / `ref<T>` / `ptr<T>` references.
- **Data & collections**: struct construction (positional and named
  `Point(x: 3, y: 4)`), field access and mutation, UFCS method calls
  (`p.dist()`), enum variants with payloads, `list<T>` / `map<K, V>` with
  iteration and `sort`.
- **Control flow**: `if` / `elif` / `else`, `while`, range `for`, `loop`,
  `break` / `continue`, exhaustive `match` with payload binding, `throw` /
  `try` / `catch`, and the `?` operator for `option` / `result` propagation.
- **Abstraction**: user-defined generic functions (`fn f<T> ...`) with call-site
  inference, traits with `impl` and bounded generics (`<T: Show>`), and
  first-class functions passed and returned by value.
- **Programs**: multi-file `import` with `pub` visibility and a `lullaby.json`
  project manifest with local-path dependencies, so programs can span multiple
  files and packages.
- **Standard library** (the always-in-scope prelude): a rich math library,
  string / bytes / UTF-8 utilities plus number parsing and char helpers,
  time / clock and OS randomness, memory builtins, and text / directory /
  binary file I/O — all cataloged in the
  [standard library reference](documents/standard_library.md).
- **Systems & I/O**: bitwise operators and intrinsics; TCP/UDP socket I/O with a
  working HTTP client and HTTP/1.1 server written in pure Lullaby; and
  concurrency via threads, channels, a shared mutex, atomics, and data
  parallelism (`parallel_map`).
- **Tooling**: strong diagnostics (concise / verbose / JSON), a canonical
  formatter (`lullaby fmt`), an editor language server (`lullaby lsp`), a
  language-level test runner (`lullaby test`), and a `.lbc` bytecode artifact
  with an inspector.

For the authoritative, always-current list of what is implemented, see the
[repository map](documents/repository_map.md) and the
[language specification](documents/language_specification.md).

## Roadmap

Lullaby is not yet at 1.0. Genuinely planned work includes
environment-capturing (capturing) closures, generic user types and trait
objects, a broader browser/DOM interop surface, wider FFI breadth, and the
packaging / installer work (native install channels). See
[documents/roadmap_1_0.md](documents/roadmap_1_0.md) for the current sequencing.
(Wider integer types, `f32`, and the WebAssembly heap phase have since shipped.)

## Build From Source

Lullaby currently builds from source with Cargo. Prebuilt install channels
(winget, MSI, and similar) are planned but not yet available.

Prerequisites:

- A Rust toolchain with Cargo.
- On Windows, the native backend's link step best-effort uses `rust-lld` and the
  MSVC `LIB` environment; without them `lullaby native` still emits the object
  file and reports why it could not link.

Common commands (run from the repository root):

```powershell
cargo run -p lullaby_cli -- check examples\valid\calculator.lby
cargo run -p lullaby_cli -- run examples\valid\calculator.lby
cargo run -p lullaby_cli -- run --backend bytecode examples\valid\calculator.lby
cargo run -p lullaby_cli -- compile --optimize full -o target\calculator.lbc examples\valid\calculator.lby
cargo run -p lullaby_cli -- inspect target\calculator.lbc
cargo run -p lullaby_cli -- run target\calculator.lbc
```

Run the test suite and lints with the standard workspace commands:

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

## CLI

Invoke the CLI during development with `cargo run -p lullaby_cli -- <command>`
(shown below as `lullaby` for brevity):

- `lullaby new <name>` — scaffold a new project directory (`lullaby.json`, a
  runnable `src/main.lby`, and a `.gitignore`).
- `lullaby check [--verbose|--format json] <file.lby>` — type-check source,
  including helper/library files without `main`.
- `lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|full] [--verbose|--format json] <file.lby>` — run source on any interpreter backend.
- `lullaby run [--verbose|--format json] <file.lbc>` — run a compiled bytecode artifact.
- `lullaby compile [--optimize none|constant-fold|dead-code|full] [-o output.lbc] [--verbose|--format json] <file.lby>` — emit a versioned `.lbc` artifact.
- `lullaby build [--optimize ...] [-o output.lbc] [--verbose|--format json] <file.lby>` — artifact-generation alias for `compile`.
- `lullaby inspect [--verbose|--format json] <file.lbc>` — summarize a `.lbc` artifact.
- `lullaby fmt [--write|--check] <file.lby>` — canonical source formatter.
- `lullaby wasm [--verbose] [-o out.wasm] <file.lby>` — emit a `.wasm` module for the eligible subset.
- `lullaby native [--verbose] [--freestanding|--no-std] [--debug|-g] [-o out.exe] <file.lby>` — emit an x86-64 COFF object and best-effort link a native `.exe`.
- `lullaby test [--verbose] <file.lby>` — run `test_*` functions and report pass/fail.
- `lullaby lsp` — run the editor language server over stdio.
- `lullaby docs` — open / locate the offline documentation.
- `lullaby examples` — list bundled examples.
- `lullaby help`, `lullaby --version`.

`lullaby compile`, `lullaby build`, source `lullaby run`, `lullaby wasm`, and
`lullaby native` require executable source with a zero-argument `main`; invalid
entry points report `L0329`.

`lullaby native` compiles the i64-scalar subset (plus the fixed-width integer
operations within those functions, stack-allocated scalar aggregates, and a
first string-heap step) to an x86-64 Windows COFF object and,
best-effort, links it into a runnable `.exe`. `--freestanding` (alias
`--no-std`) builds a no-C-runtime executable that links `kernel32.lib` only and
exits through `kernel32!ExitProcess`; it is still a Windows PE, not a bare-metal
binary. `--debug` (alias `-g`) emits CodeView source-line debug info, opt-in so
the default object bytes are unchanged.

## Documentation

- [Standard library reference (the prelude)](documents/standard_library.md)
- [Language specification](documents/language_specification.md)
- [Road to 1.0](documents/roadmap_1_0.md)
- [Implementation plan](documents/implementation_plan.md)
- [Diagnostic registry](documents/diagnostic_registry.md)
- [Contributor guide for language features](documents/contributor_guide.md)
- [Repository map](documents/repository_map.md)

Runnable example programs live under [`examples/`](examples/); the offline
browser documentation is a self-contained HTML bundle that can be generated and
opened directly from disk (no server or internet access required).

## License

Lullaby is open source under the [MIT License](LICENSE) — free to use, modify, and
redistribute, including in proprietary and commercial work. The only condition is
retaining the copyright and license notice.
