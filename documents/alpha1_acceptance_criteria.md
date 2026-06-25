# Alpha 1 Acceptance Criteria

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

Alpha 1 is the first reviewable Nous Lang toolchain checkpoint. It is not the full systems language, native compiler, installer, or standard library. It is a validated executable alpha that proves the frontend, semantic checks, diagnostics, runtime subset, fixture discipline, and offline documentation workflow are coherent.

## Required Toolchain Surface

Alpha 1 is acceptable when the repository provides:

- A Rust workspace with stable crates for lexing, parsing, semantic validation, diagnostics, runtime execution, CLI entry points, typed IR work, and initial bytecode execution.
- Source validation for the canonical `.nl` extension.
- Indentation-only block parsing with hard diagnostics for curly braces and semicolon terminators.
- Function declarations with typed parameters, explicit return types, last-expression returns, explicit `return`, and `void` functions.
- Local `let` bindings with explicit types.
- Assignment and numeric compound assignment for existing locals.
- Scalar values for `i64`, `bool`, `string`, and `void`.
- Homogeneous non-empty `array<T>` literals and bounds-checked indexing.
- Arithmetic, equality, ordering, grouped expressions, function calls, and boolean `and`/`or`/`not`.
- `if`/`elif`/`else`, `while`, inclusive range `for`, `loop`, `break`, and `continue`.
- Interim heap-slot memory builtins: `alloc`, `load`, `store`, and `dealloc`.
- Text file builtins: `read_file`, `write_file`, `append_file`, and `file_exists`.
- Conservative system command builtins: `sys_status` and `sys_output` with direct program-plus-argv execution and no shell invocation.
- `nlang check` and `nlang run` through `cargo run -p nous_cli -- ...` during development, with `run --backend ast|ir|bytecode` for the current executable subset.
- Concise, verbose, and deterministic JSON diagnostics for representative source, lexer, parser, semantic, IR, runtime, and resource failures.

## Required Documentation Surface

Alpha 1 documentation is acceptable when:

- `documents/language_specification.md` accurately lists the current executable alpha separately from planned design material.
- `documents/diagnostic_registry.md` lists every stable `N####` code emitted by the alpha.
- `documents/implementation_plan.md` records which epics are alpha-complete, partially complete, or pending.
- `documents/repository_map.md` accurately maps source layout, docs, fixtures, commands, and verification responsibilities.
- `offline_docs/index.html` is self-contained and opens directly from disk without a server, CDN, remote font, or internet dependency.
- Offline documentation examples that claim to work are backed by `.nl` fixtures and verified by `offline_docs/verify_offline_docs.py`.
- Planned syntax in design documents is clearly distinguishable from implemented syntax.

## Required Verification Gate

Alpha 1 cannot be called done unless all of these commands pass from the repository root:

```powershell
cargo fmt --check
cargo test --all
cargo clippy --all-targets --all-features -- -D warnings
python offline_docs/verify_offline_docs.py
git diff --check -- .
```

The stale-source marker search from `AGENTS.md` should return no matches. Markdown local-reference checks should also pass with a file-like local target filter so language examples such as `[FUNCTION_NAME]([ARGUMENTS])` are not misclassified as broken file links.

## Release Evidence

The Alpha 1 release note should include:

- The commit hash being released.
- The exact verification commands and pass/fail outcome.
- A short list of supported `.nl` language features.
- A short list of known limitations and non-goals.
- Links or references to representative valid and invalid fixtures.
- Confirmation that ClickUp tracking has been updated for completed, deferred, and next-phase work.

## Explicit Non-Goals

Alpha 1 does not require:

- Native code generation, linking, or binary output.
- A packaged installer or direct installed `nlang` executable.
- Modules, packages, structs, traits, interfaces, pattern matching, or user-defined generics beyond current `array<T>` spelling.
- Full region memory, ARC/reference counting, lifetime analysis, or GC hooks.
- Streams, binary I/O, memory mapping, async, sockets, IPC, or OS syscall abstractions beyond the current safe system command builtins.
- A generated offline-docs pipeline from Markdown. The current hand-authored self-contained HTML bundle is acceptable for Alpha 1 if verification passes.

## Suggested Next Phase After Alpha 1

Once Alpha 1 is accepted, the next phase should harden the typed semantic IR and initial bytecode backend with optimizer passes, backend snapshot tests, and a clearer lower-level bytecode instruction format before native code generation. This keeps the project conservative: preserve the working AST runtime while proving that a lower-level contract can support later optimization and native backends.
