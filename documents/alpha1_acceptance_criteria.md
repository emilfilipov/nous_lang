# Alpha 1 Acceptance Criteria

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

Alpha 1 is the first installable Lullaby toolchain checkpoint. It is not the full systems language, native compiler, or standard library. It is a minimal working language and tooling release that proves the frontend, semantic checks, diagnostics, runtime subset, bytecode artifact path, fixture discipline, offline documentation workflow, and Windows-first packaging are coherent.

## Required Toolchain Surface

Alpha 1 is acceptable when the repository provides:

- A Rust workspace with stable crates for lexing, parsing, semantic validation, diagnostics, runtime execution, CLI entry points, typed IR work, and initial bytecode execution.
- Source validation for the canonical `.lby` extension.
- Indentation-only block parsing with hard diagnostics for curly braces and semicolon terminators.
- Function declarations with typed parameters, explicit return types, last-expression returns, explicit `return`, and `void` functions.
- Local `let` bindings with explicit types or initializer-based inferred types.
- Assignment and numeric compound assignment for existing locals.
- Scalar values for `i64`, `bool`, `string`, and `void`.
- Homogeneous non-empty `array<T>` literals and bounds-checked indexing.
- Arithmetic, equality, ordering, grouped expressions, function calls, and boolean `and`/`or`/`not`.
- `if`/`elif`/`else`, `while`, inclusive range `for`, `loop`, `break`, and `continue`.
- Interim heap-slot memory builtins: `alloc`, `load`, `store`, and `dealloc`.
- Text file builtins: `read_file`, `write_file`, `append_file`, and `file_exists`.
- Conservative system command builtins: `sys_status` and `sys_output` with direct program-plus-argv execution and no shell invocation.
- `lullaby check`, `lullaby compile`, `lullaby build`, `lullaby inspect`, `lullaby run`, `lullaby docs`, and `lullaby examples`, with `cargo run -p lullaby_cli -- ...` equivalents during development. `build` is the build-oriented alias for the `.lbc` artifact-generation path, `run --backend ast|ir|bytecode` supports source execution, `inspect file.lbc` summarizes compiled bytecode artifacts, and `run file.lbc` executes compiled bytecode artifacts.
- A versioned `.lbc` bytecode artifact with a format marker, version, metadata, entry point, function table, compatibility checks, and bytecode module.
- A release `lullaby` binary usable outside Cargo.
- A Windows-first installer or portable archive containing the CLI, offline docs, examples, readme/license, setup instructions, optional PATH setup/cleanup helpers, and a checksum artifact.
- Concise, verbose, and deterministic JSON diagnostics for representative source, lexer, parser, semantic, IR, bytecode artifact, runtime, and resource failures.

## Required Documentation Surface

Alpha 1 documentation is acceptable when:

- `documents/language_specification.md` accurately lists the current executable alpha separately from planned design material.
- `documents/alpha1_language_surface.md` is the canonical frozen feature surface for installable Alpha 1.
- `documents/diagnostic_registry.md` lists every stable `N####` code emitted by the alpha.
- `documents/implementation_plan.md` records which epics are alpha-complete, partially complete, or pending.
- `documents/repository_map.md` accurately maps source layout, docs, fixtures, commands, and verification responsibilities.
- `offline_docs/index.html` is self-contained and opens directly from disk without a server, CDN, remote font, or internet dependency.
- Offline documentation is bundled with the release package and discoverable from the installed or unpacked toolchain.
- Offline documentation examples that claim to work are backed by `.lby` fixtures and verified by `offline_docs/verify_offline_docs.py`.
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

`scripts/verify_markdown_refs.ps1` should pass. It covers stale-source markers, file-like local Markdown links, and backticked `.md` references while filtering out language examples such as `[FUNCTION_NAME]([ARGUMENTS])`.

`scripts/verify_release.ps1` should be the release proof command for Alpha 1. It must also prove the packaged or release-built `lullaby` binary can:

- report `lullaby --version`;
- report the local offline documentation path through `lullaby docs`;
- report the local examples path through `lullaby examples`;
- check a valid `.lby` fixture;
- run a valid `.lby` fixture;
- compile and build a valid `.lby` fixture into `.lbc`;
- inspect the compiled `.lbc` artifact;
- run the compiled `.lbc` artifact;
- run dry-run PATH setup/cleanup helpers;
- verify the generated archive checksum;
- locate or include the offline docs bundle.

## Release Evidence

The Alpha 1 release note should include:

- The commit hash being released.
- The exact verification commands and pass/fail outcome.
- The packaged artifact name, checksum artifact, and install/unpack instructions.
- A short list of supported `.lby` language features.
- The supported CLI commands, including `check`, `compile`, `build`, `inspect file.lbc`, `run`, `run file.lbc`, `docs`, and `examples`.
- A short list of known limitations and non-goals.
- Links or references to representative valid and invalid fixtures.
- Confirmation that ClickUp tracking has been updated for completed, deferred, and next-phase work.

## Explicit Non-Goals

Alpha 1 does not require:

- Native code generation, linking, or binary output.
- Modules, packages, structs, traits, interfaces, pattern matching, or user-defined generics beyond current `array<T>` spelling.
- Full region memory, ARC/reference counting, lifetime analysis, or GC hooks.
- Streams, binary I/O, memory mapping, async, sockets, IPC, or OS syscall abstractions beyond the current safe system command builtins.
- A generated offline-docs pipeline from Markdown. The current hand-authored self-contained HTML bundle is acceptable for Alpha 1 if verification passes.

## Suggested Next Phase After Alpha 1

Once Alpha 1 is accepted, the next phase should harden the typed semantic IR and initial bytecode backend with backend snapshot tests, stricter bytecode instruction validation, memory-operation metadata, and a fuller instruction-bytecode VM before native code generation. This keeps the project conservative: preserve the working AST runtime while proving that a lower-level contract can support later optimization and native backends. See [post_alpha_roadmap.md](post_alpha_roadmap.md) for the active sequence.
