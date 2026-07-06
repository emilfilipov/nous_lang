# Alpha 1 Language Surface

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This document freezes the installable Alpha 1 surface. The implemented parser grammar is drafted in [formal_grammar.md](formal_grammar.md). If another design document describes a feature that is not listed here, treat that feature as planned design material, not implemented Alpha 1 behavior.

## Source And Blocks

- Source files use the `.lby` extension. It is the only accepted source extension; the original `.lullaby` extension has been retired.
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

- Implemented scalar types: `i64`, `f64`, `bool`, `string`, and `void`.
- Float literals contain a decimal point (e.g. `3.14`, `2.0`) and have type `f64`. `i64` and `f64` do not mix implicitly; combining them is a type error.
- Implemented array spelling: `array<T>`.
- Structs: `struct NAME` followed by indented `field type` lines declares a nominal record type (top level only). Construct positionally with call spelling — `Point(3, 4)` — read fields with `.` — `p.x` — and mutate fields with assignment — `p.x = 5`, `p.y += 1`, including nested `a.b.c = e`. Invalid declarations report `L0370`, bad field access `L0371`, and construction mismatches `L0372`; a field assignment with a wrong-type value reports `L0314`. Structs work across the AST, IR, and bytecode backends. Named-field construction is deferred (see `struct_design.md`).
- Array literals must be non-empty and homogeneous, such as `[1, 2, 3]`.
- Array indexing is bounds-checked at runtime and requires an `i64` index.
- Interim pointer type names use concrete spellings such as `ptr_i64`.
- Omitted local binding annotations are inferred from the initializer expression. Empty arrays and `void` initializers cannot supply an inferred local type.

## Expressions

- Implemented expressions include literals, variables, function calls with parentheses, grouped expressions, array literals, array indexing, arithmetic, equality, ordering, and logical operators.
- Arithmetic `+`, `-`, `*`, and `/` operate on two `i64`s or two `f64`s (operands must match); `+` also concatenates two `string`s. Integer division by zero is a runtime error; `f64` division follows IEEE 754 (division by zero yields infinity/NaN).
- Equality requires matching operand types.
- Ordering comparisons `<`, `<=`, `>`, and `>=` require two `i64`s or two `f64`s.
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
- String operations: `to_string(x)` converts an `i64`, `bool`, or `string` to a `string`; `+` concatenates when both operands are `string` (and still adds when both are `i64`). Mixed `string`/`i64` operands to `+` are a type error (`L0307`). This makes computed values printable, e.g. `println("answer: " + to_string(40 + 2))`.
- Reference types: `rc<T>` is a reference-counted shared owner, `ref<T>` is a non-null borrowed reference, and `ptr<T>` (legacy spelling `ptr_T`) is a raw pointer.
- Reference builtins: `rc_new(value)` creates an `rc<T>`; `rc_clone(rc<T>)` shares ownership; `rc_release(rc<T>)` drops one owner and frees at zero; `rc_get(rc<T>)`/`ref_get(ref<T>)` read the referent; `rc_borrow(rc<T>)` yields a `ref<T>`.
- `unsafe` block: an indented block introduced by `unsafe` in which raw-pointer operations are permitted. `ptr_read(ptr<T>)` and `ptr_write(ptr<T>, value)` require an `unsafe` context (`L0330` otherwise); `unsafe` is a transparent scope, so bindings inside it remain visible afterward.
- Region declarations: `region NAME: size=N[, align=N][, kind=static|dynamic][, mutable=true|false]` declares a named memory region. Size must be positive, alignment (if present) must be a power of two, and kind must be `static` or `dynamic` (`L0340`); region names must be unique within a function (`L0341`). Regions are compile-time metadata in Alpha 1 (surfaced as `RegionCreate` in memory analysis) with no runtime allocation yet.
- Lifetime analysis: a conservative compile-time pass rejects straight-line use-after-free and double-free of resources freed by `dealloc`/`rc_release` (`L0350`), and rejects returning a borrowed `ref<T>` from a function (`L0351`). Freeing inside a branch is not tracked out of that branch; the runtime `L0406` guard remains as defense-in-depth. Deterministic per-block cleanup ordering is provided by `lullaby_ir::frame_layout`.
- Structured error handling: `throw EXPR` raises a `string` error value; `try` / `catch NAME` runs a protected block and, if it throws, binds the error message to `NAME` (a `string`) and runs the handler for recovery. Like `if`/`else`, a `try`/`catch` is an expression: when both arms yield the same type it produces that value (so it can be a function's final expression); otherwise it is a void statement. Uncaught throws surface as runtime `L0420`. Only user-thrown errors are caught; a `try` without `catch` reports `L0213`. Structured error handling runs identically on the AST, IR, and bytecode backends (including under optimization).
- Type aliases: `alias NAME = TYPE` declares a top-level type alias (e.g. `alias Count = i64`, `alias Numbers = array<i64>`). Aliases resolve structurally to their canonical target before type checking, so an alias and its target are interchangeable, and aliases carry no runtime representation. Aliases inside generic arguments (`array<Count>`) resolve too. Duplicate aliases report `L0360` and cyclic aliases report `L0361`. Struct/record and map generics remain planned until those types exist.

## CLI And Artifacts

- Development commands are available through `cargo run -p lullaby_cli -- ...`.
- The release package exposes `bin\lullaby.exe`.
- Supported commands:
  - `lullaby check [--verbose|--format json] <file.lby>`
  - `lullaby compile [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>`
  - `lullaby build [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>`
  - `lullaby inspect [--verbose|--format json] <file.lbc>`
  - `lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] [--verbose|--format json] <file.lby>`
  - `lullaby run [--verbose|--format json] <file.lbc>`
  - `lullaby docs`
  - `lullaby examples`
  - `lullaby --version`
- `--diagnostic-format json` is accepted as a JSON diagnostics alias.
- `lullaby compile`, `lullaby build`, and source `lullaby run` require a zero-argument `main` entry point before lowering or execution. Invalid executable entry points report `L0329`.
- `lullaby compile` and its artifact-generation alias `lullaby build` write a versioned `.lbc` instruction-bytecode artifact with a format marker, artifact version, metadata, entry point, function table, ordered memory operation metadata, compatibility checks, and bytecode module instructions. `lullaby inspect` prints artifact metadata, function signatures, and memory operation counts without executing the program.

## Diagnostics

- Alpha diagnostics use stable `L####` codes documented in [diagnostic_registry.md](diagnostic_registry.md).
- Concise diagnostics are the default.
- `--verbose` adds source excerpts, caret markers, root-cause text, suggested fixes, related notes, and runtime traceback frames when available.
- `--format json` emits deterministic machine-readable diagnostics for tools, CI, editors, and LLM agents.

## Packaging

- `scripts/package_windows_portable.ps1` builds the Windows Alpha 1 portable package and zip archive under `dist/`.
- The package contains `bin\lullaby.exe`, `docs\index.html`, valid `.lby` examples, invalid diagnostic examples, optional PATH setup/cleanup helpers, README/VERSION metadata, a zip checksum, and a repository license file if one exists.
- `scripts/verify_release.ps1` is the Alpha 1 release gate for the packaged toolchain.
- `scripts/publish_github_release.ps1` verifies the package, tags the current commit, and creates a GitHub prerelease with the portable zip plus checksum asset.

## Planned Beyond Alpha 1

The following are not implemented Alpha 1 behavior:

- Native code generation for the full language, linking, and machine-code binary output (a COFF object prototype exists for a small subset).
- Modules, packages, imports, unions, traits, interfaces, classes, and pattern matching (structs are implemented; struct methods, field mutation, and named-field construction are deferred).
- User-defined generics beyond `array<T>` and the `ptr<T>`/`ref<T>`/`rc<T>` reference types and type aliases (struct/record and map generics remain planned).
- GC hooks and runtime region allocation (region *declarations* are analyzed as compile-time metadata; reference counting via `rc<T>` and conservative lifetime analysis are implemented).
- Binary I/O, memory mapping, async, sockets, IPC, and general syscall APIs (standard text streams and file/system builtins are implemented).
- A full installer; Alpha 1 uses a Windows portable archive with optional user PATH helper scripts.

When planned syntax keywords such as `import`, `module`, `match`, or a bare `catch` appear as source syntax, the parser reports `L0211` instead of accepting a partial or ambiguous construct.
