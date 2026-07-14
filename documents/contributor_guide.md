# Contributor Guide For Language Features

Canonical language rules: see [core_language_rules.md](core_language_rules.md). Current implemented behavior: see [language_surface.md](language_surface.md) and [formal_grammar.md](formal_grammar.md).

This guide is for adding or changing a Lullaby language feature. Keep changes small enough that the parser, semantic checks, runtime or IR behavior, fixtures, docs, and verification can move together.

## Before Editing

1. Read [repository_map.md](repository_map.md) to find the owned files.
2. Read the relevant language document:
   - Syntax or grammar: [formal_grammar.md](formal_grammar.md), [lullaby_syntax_design.md](lullaby_syntax_design.md), and [core_language_rules.md](core_language_rules.md).
   - Types: [lullaby_type_system.md](lullaby_type_system.md).
   - Memory/runtime: [lullaby_memory_management.md](lullaby_memory_management.md).
   - Control flow: [lullaby_control_structures.md](lullaby_control_structures.md).
   - I/O, syscalls, or concurrency: [lullaby_input_output.md](lullaby_input_output.md).
   - Errors and diagnostics: [lullaby_error_handling.md](lullaby_error_handling.md) and [diagnostic_registry.md](diagnostic_registry.md).
3. Check the ClickUp task. If its acceptance criteria mention planned syntax that the current surface does not support yet, narrow the implementation to the current feature surface or split the future work into a separate task.
4. Inspect the current git state with `git status --short` and preserve unrelated user changes.

## Implementation Path

Most language features flow through these layers:

1. `crates/lullaby_lexer/` for new tokens, keywords, source-shape diagnostics, or indentation behavior.
2. `crates/lullaby_parser/` for AST shape and syntax acceptance or rejection.
3. `crates/lullaby_semantics/` for static rules, type inference, scopes, entry-point rules, and diagnostic selection.
4. `crates/lullaby_runtime/` for AST execution behavior and runtime/resource errors.
5. `crates/lullaby_ir/` for typed IR lowering, optimizer safety, bytecode artifact compatibility, and backend parity.
6. `crates/lullaby_cli/` for user-facing commands, diagnostics, backend flags, and package behavior.

Do not accept syntax in the parser unless the semantic layer can validate it deterministically. Planned syntax should report `L0211` until the feature is implemented through the required execution or tooling path.

## Tests And Fixtures

Add focused coverage at the first layer that owns the behavior, then add an end-to-end fixture when users can exercise it:

- Parser syntax changes: add parser unit tests and refresh AST snapshots only when the AST shape intentionally changes.
- Semantic rules: add valid and invalid semantic tests, using stable diagnostic codes from [diagnostic_registry.md](diagnostic_registry.md).
- Runtime or backend behavior: add fixtures under `tests/fixtures/valid/` or `tests/fixtures/invalid/` and cover AST, IR, and bytecode paths when the feature reaches all backends.
- CLI behavior: add `crates/lullaby_cli/tests/` coverage when command output, diagnostics, artifact generation, or backend selection is affected.

Prefer small fixtures that prove one behavior. If a fixture is intentionally invalid, name it after the diagnostic or failure mode.

## Documentation

Update docs in the same change as the implementation:

- [language_surface.md](language_surface.md) when current accepted behavior changes.
- [formal_grammar.md](formal_grammar.md) when parser syntax changes.
- [language_specification.md](language_specification.md) for the current implementation summary.
- The subsystem design document listed in [repository_map.md](repository_map.md).
- [diagnostic_registry.md](diagnostic_registry.md) when adding or changing diagnostic codes, root causes, or suggested fixes.
- [repository_map.md](repository_map.md) for new files, commands, fixtures, tests, responsibilities, or changed document ownership.

User-facing documentation is the hosted online website, maintained separately from this repository; there is no offline/bundled HTML doc artifact in this repo to update.

Keep planned design material clearly separated from implemented behavior.

## Verification

For implementation changes, run:

```powershell
cargo fmt --check
cargo test --all
cargo clippy --all-targets --all-features -- -D warnings
powershell -ExecutionPolicy Bypass -File scripts\verify_markdown_refs.ps1
git diff --check -- .
```

Run the release gate when the change affects syntax, CLI behavior, diagnostics, package contents, docs, examples, artifacts, installers, or user-facing runtime behavior:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\verify_release.ps1
```

If a required check cannot run, record the command, the blocker, and the residual risk in the ClickUp task and commit or PR notes.

## Completion

Before closing a task:

1. Confirm the implementation matches the current feature surface or explicitly records future-surface exclusions.
2. Confirm the Markdown docs are current.
3. Confirm [repository_map.md](repository_map.md) is accurate.
4. Commit with a focused message, for example `compiler: infer local binding types`.
5. Push the commit.
6. Update ClickUp status and attach verification evidence. If comments are unavailable, put the evidence in the task description.
