# Contributor Guide For Language Features

Canonical language rules: see [core_language_rules.md](core_language_rules.md). Current implemented behavior: see [alpha1_language_surface.md](alpha1_language_surface.md) and [formal_grammar.md](formal_grammar.md).

This guide is for adding or changing a Nous Lang language feature. Keep changes small enough that the parser, semantic checks, runtime or IR behavior, fixtures, docs, and verification can move together.

## Before Editing

1. Read [repository_map.md](repository_map.md) to find the owned files.
2. Read the relevant language document:
   - Syntax or grammar: [formal_grammar.md](formal_grammar.md), [nous_lang_syntax_design.md](nous_lang_syntax_design.md), and [core_language_rules.md](core_language_rules.md).
   - Types: [nous_lang_type_system.md](nous_lang_type_system.md).
   - Memory/runtime: [nous_lang_memory_management.md](nous_lang_memory_management.md).
   - Control flow: [nous_lang_control_structures.md](nous_lang_control_structures.md).
   - I/O, syscalls, or concurrency: [nous_lang_input_output.md](nous_lang_input_output.md).
   - Errors and diagnostics: [nous_lang_error_handling.md](nous_lang_error_handling.md) and [diagnostic_registry.md](diagnostic_registry.md).
3. Check the ClickUp task. If its acceptance criteria mention planned syntax that Alpha 1 does not support yet, narrow the implementation to the current feature surface or split the future work into a separate task.
4. Inspect the current git state with `git status --short` and preserve unrelated user changes.

## Implementation Path

Most language features flow through these layers:

1. `crates/nous_lexer/` for new tokens, keywords, source-shape diagnostics, or indentation behavior.
2. `crates/nous_parser/` for AST shape and syntax acceptance or rejection.
3. `crates/nous_semantics/` for static rules, type inference, scopes, entry-point rules, and diagnostic selection.
4. `crates/nous_runtime/` for AST execution behavior and runtime/resource errors.
5. `crates/nous_ir/` for typed IR lowering, optimizer safety, bytecode artifact compatibility, and backend parity.
6. `crates/nous_cli/` for user-facing commands, diagnostics, backend flags, and package behavior.

Do not accept syntax in the parser unless the semantic layer can validate it deterministically. Planned syntax should report `N0211` until the feature is implemented through the required execution or tooling path.

## Tests And Fixtures

Add focused coverage at the first layer that owns the behavior, then add an end-to-end fixture when users can exercise it:

- Parser syntax changes: add parser unit tests and refresh AST snapshots only when the AST shape intentionally changes.
- Semantic rules: add valid and invalid semantic tests, using stable diagnostic codes from [diagnostic_registry.md](diagnostic_registry.md).
- Runtime or backend behavior: add fixtures under `tests/fixtures/valid/` or `tests/fixtures/invalid/` and cover AST, IR, and bytecode paths when the feature reaches all backends.
- CLI behavior: add `crates/nous_cli/tests/` coverage when command output, diagnostics, artifact generation, or backend selection is affected.
- Offline docs examples: use `tests/fixtures/valid/docs_*.nl` or another checked-in fixture and make the HTML snippet match the file exactly.

Prefer small fixtures that prove one behavior. If a fixture is intentionally invalid, name it after the diagnostic or failure mode.

## Documentation

Update docs in the same change as the implementation:

- [alpha1_language_surface.md](alpha1_language_surface.md) when current accepted behavior changes.
- [formal_grammar.md](formal_grammar.md) when parser syntax changes.
- [language_specification.md](language_specification.md) for the current implementation summary.
- The subsystem design document listed in [repository_map.md](repository_map.md).
- [diagnostic_registry.md](diagnostic_registry.md) when adding or changing diagnostic codes, root causes, or suggested fixes.
- `offline_docs/index.html` and `offline_docs/verify_offline_docs.py` when user-facing syntax, CLI usage, diagnostics, examples, installation, or package behavior changes.
- [repository_map.md](repository_map.md) for new files, commands, fixtures, tests, responsibilities, or changed document ownership.

Keep planned design material clearly separated from implemented Alpha behavior.

## Verification

For implementation changes, run:

```powershell
cargo fmt --check
cargo test --all
cargo clippy --all-targets --all-features -- -D warnings
python offline_docs\verify_offline_docs.py
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

1. Confirm the implementation matches the current Alpha feature surface or explicitly records future-surface exclusions.
2. Confirm docs and offline docs are current.
3. Confirm [repository_map.md](repository_map.md) is accurate.
4. Commit with a focused message, for example `compiler: infer local binding types`.
5. Push the commit.
6. Update ClickUp status and attach verification evidence. If comments are unavailable, put the evidence in the task description.
