# Alpha 1 Language Surface

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This document freezes the installable Alpha 1 surface. The implemented parser grammar is drafted in [formal_grammar.md](formal_grammar.md). If another design document describes a feature that is not listed here, treat that feature as planned design material, not implemented Alpha 1 behavior.

## Source And Blocks

- Source files use the `.lby` extension (`.lullaby` is accepted as a legacy alias).
- Scope is indentation-only.
- Curly braces are rejected as block delimiters.
- Semicolons are rejected as statement terminators.
- Comments begin with `#` and continue to the end of the line.

## Declarations

- Functions use `fn name param Type -> ReturnType`.
- Function parameters require explicit types.
- Non-void functions return the last reachable expression unless `return expression` exits earlier.
- Void functions use `-> void` and may use bare `return`.
- Executable source passed to `lullaby compile` or source `lullaby run` must define `fn main -> Type` with zero parameters. `lullaby check` can still validate helper/library-style functions that do not define `main`.
- Local bindings use `let name Type = expression` for explicit annotations or `let name = expression` when the initializer type is unambiguous.
- Existing local bindings can be updated with `=`, `+=`, `-=`, `*=`, and `/=` when the types are valid.

## Types

- Implemented scalar types: `i64`, `bool`, `string`, and `void`.
- Implemented array spelling: `array<T>`.
- Array literals must be non-empty and homogeneous, such as `[1, 2, 3]`.
- Array indexing is bounds-checked at runtime and requires an `i64` index.
- Interim pointer type names use concrete spellings such as `ptr_i64`.
- Omitted local binding annotations are inferred from the initializer expression. Empty arrays and `void` initializers cannot supply an inferred local type.

## Expressions

- Implemented expressions include literals, variables, function calls with parentheses, grouped expressions, array literals, array indexing, arithmetic, equality, ordering, and logical operators.
- Arithmetic currently supports integer `+`, `-`, `*`, and `/`.
- Equality requires matching operand types.
- Ordering comparisons `<`, `<=`, `>`, and `>=` require `i64` operands.
- Logical operators are `and`, `or`, and `not`; `and` and `or` short-circuit.

## Control Flow

- Implemented branches: `if`, `elif`, and `else`.
- Implemented loops: `while`, inclusive range `for name from start to end`, optional `by step`, and `loop`.
- Implemented loop controls: `break` and `continue`.
- Loop control outside a loop is a semantic error.

## Builtins

- Memory builtins: `alloc(value)`, `load(ptr)`, `store(ptr, value)`, and `dealloc(ptr)`.
- Text file builtins: `read_file(path)`, `write_file(path, content)`, `append_file(path, content)`, and `file_exists(path)`.
- System command builtins: `sys_status(program, args)` and `sys_output(program, args)`, where `args` is `array<string>`.
- System command builtins execute a program with an argv array directly and do not invoke a shell.
- Standard stream builtins: `print(text)` and `println(text)` write a `string` to stdout, `warn(text)` writes a `string` line to stderr, and `flush()` flushes stdout. Each returns `void`.
- Reference types: `rc<T>` is a reference-counted shared owner, `ref<T>` is a non-null borrowed reference, and `ptr<T>` (legacy spelling `ptr_T`) is a raw pointer.
- Reference builtins: `rc_new(value)` creates an `rc<T>`; `rc_clone(rc<T>)` shares ownership; `rc_release(rc<T>)` drops one owner and frees at zero; `rc_get(rc<T>)`/`ref_get(ref<T>)` read the referent; `rc_borrow(rc<T>)` yields a `ref<T>`.
- `unsafe` block: an indented block introduced by `unsafe` in which raw-pointer operations are permitted. `ptr_read(ptr<T>)` and `ptr_write(ptr<T>, value)` require an `unsafe` context (`N0330` otherwise); `unsafe` is a transparent scope, so bindings inside it remain visible afterward.
- Region declarations: `region NAME: size=N[, align=N][, kind=static|dynamic][, mutable=true|false]` declares a named memory region. Size must be positive, alignment (if present) must be a power of two, and kind must be `static` or `dynamic` (`N0340`); region names must be unique within a function (`N0341`). Regions are compile-time metadata in Alpha 1 (surfaced as `RegionCreate` in memory analysis) with no runtime allocation yet.
- Lifetime analysis: a conservative compile-time pass rejects straight-line use-after-free and double-free of resources freed by `dealloc`/`rc_release` (`N0350`), and rejects returning a borrowed `ref<T>` from a function (`N0351`). Freeing inside a branch is not tracked out of that branch; the runtime `N0406` guard remains as defense-in-depth. Deterministic per-block cleanup ordering is provided by `lullaby_ir::frame_layout`.

## CLI And Artifacts

- Development commands are available through `cargo run -p lullaby_cli -- ...`.
- The release package exposes `bin\lullaby.exe`.
- Supported commands:
  - `lullaby check [--verbose|--format json] <file.lullaby>`
  - `lullaby compile [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lullaby>`
  - `lullaby build [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lullaby>`
  - `lullaby inspect [--verbose|--format json] <file.lbc>`
  - `lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] [--verbose|--format json] <file.lullaby>`
  - `lullaby run [--verbose|--format json] <file.lbc>`
  - `lullaby docs`
  - `lullaby examples`
  - `lullaby --version`
- `--diagnostic-format json` is accepted as a JSON diagnostics alias.
- `lullaby compile`, `lullaby build`, and source `lullaby run` require a zero-argument `main` entry point before lowering or execution. Invalid executable entry points report `N0329`.
- `lullaby compile` and its artifact-generation alias `lullaby build` write a versioned `.lbc` instruction-bytecode artifact with a format marker, artifact version, metadata, entry point, function table, ordered memory operation metadata, compatibility checks, and bytecode module instructions. `lullaby inspect` prints artifact metadata, function signatures, and memory operation counts without executing the program.

## Diagnostics

- Alpha diagnostics use stable `N####` codes documented in [diagnostic_registry.md](diagnostic_registry.md).
- Concise diagnostics are the default.
- `--verbose` adds source excerpts, caret markers, root-cause text, suggested fixes, related notes, and runtime traceback frames when available.
- `--format json` emits deterministic machine-readable diagnostics for tools, CI, editors, and LLM agents.

## Packaging

- `scripts/package_windows_portable.ps1` builds the Windows Alpha 1 portable package and zip archive under `dist/`.
- The package contains `bin\lullaby.exe`, `docs\index.html`, valid `.lullaby` examples, invalid diagnostic examples, optional PATH setup/cleanup helpers, README/VERSION metadata, a zip checksum, and a repository license file if one exists.
- `scripts/verify_release.ps1` is the Alpha 1 release gate for the packaged toolchain.
- `scripts/publish_github_release.ps1` verifies the package, tags the current commit, and creates a GitHub prerelease with the portable zip plus checksum asset.

## Planned Beyond Alpha 1

The following are not implemented Alpha 1 behavior:

- Native code generation, linking, and machine-code binary output.
- Modules, packages, imports, structs, unions, traits, interfaces, classes, and pattern matching.
- User-defined generics beyond current `array<T>` spelling.
- Region memory, ARC/reference counting, lifetime analysis, and GC hooks.
- Streams, binary I/O, memory mapping, async, sockets, IPC, and general syscall APIs.
- Language-level `try`/`catch`, throws, recovery blocks, and error union/control syntax.
- A generated offline-docs pipeline from Markdown.
- A full installer; Alpha 1 uses a Windows portable archive with optional user PATH helper scripts.

When planned syntax keywords such as `import`, `module`, `struct`, `match`, or `try` appear as source syntax, the parser reports `N0211` instead of accepting a partial or ambiguous construct.
