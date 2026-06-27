# Repository Map

This file maps the repository layout and explains where to find core information. Keep it up to date whenever source files, directories, commands, tests, docs, or responsibilities change.

## Root

- [AGENTS.md](../AGENTS.md): operating guide for agents and contributors. Defines workflow, documentation rules, MCP/ClickUp/GitHub usage, testing expectations, Git rules, and where to find core language information.
- [README.md](../README.md): user-facing project overview, portable package installation, checksum verification, local build commands, CLI summary, and documentation links.
- `Cargo.toml`: Rust workspace manifest for the Nous Lang compiler/toolchain crates. The lockfile records the pinned dependency graph, including serde/serde_json for bytecode artifact serialization.
- `.gitignore`: ignores local build outputs, caches, editor state, and generated artifacts once implementation begins.
- `.gitattributes`: normalizes repository text files to LF line endings.
- `crates/`: Rust implementation crates.
- `examples/`: user-facing `.nl` examples packaged with the toolchain.
- `scripts/`: release packaging and verification scripts.
- `tests/`: shared `.nl` fixtures used by crate and CLI tests.
- `documents/`: core language documents and planning material.
- `offline_docs/`: self-contained browser documentation bundle that can be opened directly from disk.

## Documents

- `documents/core_language_rules.md`: canonical global rules for `.nl` source files, indentation-only scope, forbidden block delimiters, and no semicolon terminators.
- `documents/alpha1_language_surface.md`: frozen installable Alpha 1 feature surface for source rules, declarations, types, expressions, control flow, builtins, CLI commands, artifacts, diagnostics, packaging, and planned non-goals.
- `documents/language_specification.md`: top-level language specification and overview. Use this first for language behavior, philosophy, syntax reference, examples, and roadmap.
- `documents/implementation_plan.md`: implementation plan for the compiler, runtime, offline browser documentation bundle, CLI, installer, tests, and release workflow.
- `documents/alpha1_acceptance_criteria.md`: Alpha 1 release checklist covering required toolchain surface, documentation surface, verification gates, release evidence, non-goals, and the recommended next phase.
- `documents/alpha1_release_notes.md`: Alpha 1 package release notes, supported surface, commands, artifact contract, verification evidence, known limitations, and next-phase guidance.
- `documents/diagnostic_registry.md`: stable diagnostic code registry and output contract for concise, verbose, and JSON diagnostics.
- `documents/nous_lang_syntax_design.md`: syntax design details for declarations, functions, data structures, operators, naming, comments, and examples.
- `documents/nous_lang_type_system.md`: type-system details for primitives, composites, references, functions, inference, safety, aliases, generics, and OS-specific types.
- `documents/nous_lang_memory_management.md`: memory model covering regions, stack allocation, heap allocation, lifetime tracking, GC hooks, safety checks, runtime memory APIs, and kernel memory examples.
- `documents/nous_lang_control_structures.md`: control flow and operators, including the current alpha if/while/loop rules plus planned switches, try/catch, coroutines, arithmetic/logical/bitwise operators, collections, conversions, and utility operations.
- `documents/nous_lang_input_output.md`: I/O and concurrency model, including files, streams, memory-mapped files, threads, processes, async, multiplexing, IPC, sockets, and performance strategies.
- `documents/nous_lang_error_handling.md`: error model, compact error tokens, compile-time and runtime categories, throw/catch/recovery behavior, diagnostics, and compiler integration.
- `documents/nous_lang_compilation_architecture.md`: compiler architecture from tokenization through semantic analysis, IR, optimization, code generation, linking, and binary verification.
- `documents/repository_map.md`: this file. Use it as the first navigation aid and update it with repository changes.

## Offline Browser Docs

- `offline_docs/index.html`: local browser entry point for alpha user documentation. It must remain self-contained with no server, CDN, remote fonts, or internet dependency.
- `offline_docs/verify_offline_docs.py`: deterministic verifier for the offline docs entry point, required sections, required alpha topics, local anchors, lack of remote dependencies, and fixture-backed executable examples.

## Examples

- `examples/README.md`: user-facing instructions for valid and invalid example programs.
- `examples/valid/`: executable `.nl` examples for calculator, arrays/control flow, file I/O, and Windows system command status.
- `examples/invalid/`: intentionally invalid `.nl` examples for inspecting diagnostics.

## Source Layout

The implementation is a Rust workspace. Unless changed by an explicit architecture decision, keep this layout:

- `crates/nous_lexer/`: source extension validation, tokenization, indentation scanning, forbidden brace/semicolon diagnostics, current and planned keyword recognition, and lexical tests.
- `crates/nous_diagnostics/`: shared diagnostic data structures, serializable span/traceback metadata, registry metadata, concise/verbose renderers, and deterministic JSON rendering.
- `crates/nous_parser/`: AST model and parser for function declarations, typed parameters, return types, indentation blocks, `let`, assignment, `return`, `break`, `continue`, if/elif/else, while/loop/range-for, calls, literals, array literals/indexing, variables, arithmetic, comparison, logical expressions, and `N0211` rejection for planned syntax keywords that are not in Alpha 1.
- `crates/nous_semantics/`: static validation for duplicate declarations, local binding types, assignment targets/types, function call arity/types, return behavior, bool conditions, loop-control placement, arithmetic/comparison/logical expression operand types, homogeneous non-empty arrays, array indexing, interim pointer-style memory builtins, text file I/O builtins, safe system command builtins, and executable zero-argument `main` validation for compile/run. Successful validation returns `CheckedProgram` with function signatures and inferred expression-type metadata.
- `crates/nous_ir/`: typed semantic IR schema, lowering from `CheckedProgram`, deterministic optimization pass configuration with opt-in constant folding, conservative block-local copy propagation, block-local dead-code elimination, executable IR interpreter, inline and fixture-driven AST/IR/bytecode parity tests including optimized backend variants, explicit instruction-bytecode lowering, versioned `.nbc` artifact encoding/decoding with metadata/function-table compatibility checks, and bytecode VM entry point for the current alpha subset.
- `crates/nous_runtime/`: in-process AST runtime for the current alpha subset, including `main`, function calls, scoped locals, assignment, branch results, while/loop/range-for execution, break/continue, array literals/indexing with runtime bounds checks, arithmetic/comparison/logical expressions with short-circuiting, `alloc`/`load`/`store`/`dealloc` heap-slot memory builtins, text file I/O builtins, direct program-plus-argv system command builtins, and categorized runtime/resource errors.
- `crates/nous_cli/`: `nlang` command-line interface. Current commands: `check [--verbose|--format json] <file.nl>`, `compile [--optimize none|constant-fold|dead-code|alpha] [-o output.nbc] [--verbose|--format json] <file.nl>`, `inspect [--verbose|--format json] <file.nbc>`, `run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] [--verbose|--format json] <file.nl>`, `run [--verbose|--format json] <file.nbc>`, `docs`, `examples`, `help`, and `--version`. `check` allows helper/library-style validation without `main`; `compile` and source `run` require zero-argument `main`. The crate declares an explicit `nlang` binary target for release packaging.
- `crates/nous_cli/tests/`: binary-level integration tests for the CLI pipeline, including valid checks, backend execution, bytecode artifact inspection, lexical errors, and semantic errors.
- `scripts/package_windows_portable.ps1`: builds the release CLI, creates `dist\nous-lang-alpha1-windows-x64\`, copies `bin\nlang.exe`, bundles `docs\index.html`, release notes, user-facing examples, and optional PATH install/uninstall helpers, copies a repository license file if one exists, writes package metadata/readme files, and creates a `.zip` archive plus `.sha256` checksum file.
- `scripts/verify_release.ps1`: runs formatting, test, clippy, offline-doc verification, builds the portable package, and smoke-tests the packaged `nlang.exe` against version, docs, examples, check, run, compile, bytecode artifact inspection/execution, invalid example diagnostics, dry-run PATH install/uninstall helpers, and archive checksum verification.
- `scripts/publish_github_release.ps1`: requires a clean worktree, runs release verification, creates/pushes a release tag, and creates a GitHub prerelease with the portable zip and checksum assets using `documents/alpha1_release_notes.md` as the release body.
- `scripts/install_windows_path.ps1` and `scripts/uninstall_windows_path.ps1`: package-root PowerShell helpers copied as `install.ps1` and `uninstall.ps1` for optional user PATH setup/cleanup.
- `scripts/install.cmd` and `scripts/uninstall.cmd`: cmd wrappers copied to the package root for users who prefer `cmd.exe`.
- `tests/fixtures/valid/`: valid `.nl` smoke fixtures used by the frontend, CLI, and offline documentation example verification. Files prefixed with `docs_` back executable examples shown in `offline_docs/index.html`.
- `tests/fixtures/invalid/`: invalid source fixtures for diagnostics and negative tests, including planned-but-unsupported syntax such as imports, modules, structs, try, and catch, plus executable entry-point failures such as missing `main` and parameterized `main`.

## Current Commands

- `cargo fmt --check`: formatting check.
- `cargo test --all`: unit tests for all crates.
- `cargo clippy --all-targets --all-features -- -D warnings`: lint all crates and integration tests.
- `cargo build --release -p nous_cli`: build the release `nlang.exe` binary.
- `cargo run -p nous_cli -- check tests/fixtures/valid/add.nl`: check a valid fixture through source validation, lexing, parsing, and semantic validation.
- `cargo run -p nous_cli -- check --verbose tests/fixtures/invalid/brace.nl`: print verbose source excerpt, root-cause, and suggested-fix diagnostics.
- `cargo run -p nous_cli -- check --format json tests/fixtures/invalid/type_mismatch.nl`: print deterministic JSON diagnostics. `--diagnostic-format json` is also accepted.
- `cargo run -p nous_cli -- run --verbose tests/fixtures/invalid/array_index_out_of_bounds.nl`: print runtime diagnostics with source context and traceback frames.
- `cargo run -p nous_cli -- compile --optimize alpha -o target/run_arithmetic.nbc tests/fixtures/valid/run_arithmetic.nl`: compile a valid source fixture into a versioned `.nbc` instruction-bytecode artifact with metadata, a function table, and dedicated function instructions.
- `cargo run -p nous_cli -- inspect target/run_arithmetic.nbc`: print `.nbc` artifact metadata and function signatures without executing it.
- `cargo run -p nous_cli -- run target/run_arithmetic.nbc`: execute a compiled `.nbc` bytecode artifact.
- `cargo run -p nous_cli -- docs`: print the local offline documentation entry path.
- `cargo run -p nous_cli -- examples`: print the local example fixture directory path.
- `cargo run -p nous_cli -- run examples/valid/calculator.nl`: run a user-facing packaged example.
- `cargo run -p nous_cli -- check examples/invalid/type_mismatch.nl`: inspect a user-facing invalid example diagnostic.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_arithmetic.nl`: run a valid fixture through source validation, lexing, parsing, semantic validation, runtime execution, and stdout output.
- `cargo run -p nous_cli -- run --backend ir tests/fixtures/valid/run_arithmetic.nl`: run a valid fixture through typed IR lowering and the IR interpreter.
- `cargo run -p nous_cli -- run --backend bytecode tests/fixtures/valid/run_arithmetic.nl`: run a valid fixture through typed IR lowering, instruction-bytecode lowering, and the bytecode VM entry point.
- `cargo run -p nous_cli -- run --backend ir --optimize constant-fold tests/fixtures/valid/run_logic.nl`: run a valid fixture through typed IR lowering, the opt-in constant-folding pass, and the IR interpreter.
- `cargo run -p nous_cli -- run --backend bytecode --optimize constant-fold tests/fixtures/valid/run_logic.nl`: run a valid fixture through typed IR lowering, the opt-in constant-folding pass, instruction-bytecode lowering, and the bytecode VM entry point.
- `cargo run -p nous_cli -- run --backend ir --optimize dead-code tests/fixtures/valid/run_arithmetic.nl`: run a valid fixture through typed IR lowering, block-local dead-code elimination, and the IR interpreter.
- `cargo run -p nous_cli -- run --backend bytecode --optimize alpha tests/fixtures/valid/run_arithmetic.nl`: run a valid fixture through typed IR lowering, the current alpha optimizer pipeline of constant folding, copy propagation, and dead-code elimination, instruction-bytecode lowering, and the bytecode VM entry point.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_memory.nl`: run the current memory builtin fixture through `alloc`, `load`, and `dealloc`.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_store.nl`: run heap-slot mutation through `alloc`, `store`, `load`, and `dealloc`.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_while.nl`: run assignment plus while-loop execution.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_loop.nl`: run infinite-loop execution with break/continue.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_logic.nl`: run boolean logic with `and`, `or`, and `not`.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_for.nl`: run inclusive range-for execution.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_for_step.nl`: run stepped and descending range-for execution.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_array.nl`: run homogeneous array literals and bounds-checked indexing.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_file_io.nl`: run text file write, append, and read builtins.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/brace.nl`: verify forbidden block delimiter diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/missing_indented_body.nl`: verify parser diagnostics for missing indentation.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/type_mismatch.nl`: verify semantic type mismatch diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/assignment_type_mismatch.nl`: verify assignment type mismatch diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/break_outside_loop.nl`: verify loop-control placement diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/logical_type_mismatch.nl`: verify logical operand type diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/ordering_type_mismatch.nl`: verify non-numeric ordering comparison diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/for_range_type_mismatch.nl`: verify range-for bound type diagnostics.
- `cargo run -p nous_cli -- run tests/fixtures/invalid/for_zero_step.nl`: verify range-for zero-step runtime diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/array_literal_type_mismatch.nl`: verify homogeneous array literal diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/array_index_type_mismatch.nl`: verify array index type diagnostics.
- `cargo run -p nous_cli -- run tests/fixtures/invalid/array_index_out_of_bounds.nl`: verify runtime array bounds diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/store_type_mismatch.nl`: verify `store` value type diagnostics.
- `cargo run -p nous_cli -- run tests/fixtures/invalid/store_after_dealloc.nl`: verify invalid pointer diagnostics after deallocation.
- `cargo run -p nous_cli -- run tests/fixtures/invalid/read_missing_file.nl`: verify structured resource diagnostics for missing file reads.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/read_file_path_type.nl`: verify file builtin path type diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/write_file_content_type.nl`: verify file builtin content type diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/sys_args_type.nl`: verify system command builtin argv type diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/unsupported_import.nl`: verify `N0211` planned-syntax diagnostics.
- `cargo run -p nous_cli -- compile -o target/missing_main.nbc tests/fixtures/invalid/missing_main.nl`: verify `N0329` executable entry-point diagnostics.
- Running a malformed `.nbc` artifact verifies `N0601 [bytecode error]` diagnostics for unsupported bytecode artifacts.
- `python offline_docs/verify_offline_docs.py`: verify the self-contained offline browser documentation entry point, including metadata, fixture content, compile/run/inspect/examples command coverage, and `nlang check`/`nlang run` execution for documented `.nl` examples.
- `powershell -ExecutionPolicy Bypass -File scripts/package_windows_portable.ps1`: build the Windows Alpha 1 portable package, zip archive, and SHA-256 checksum under `dist/`.
- `powershell -ExecutionPolicy Bypass -File scripts/verify_release.ps1`: run the full Alpha 1 release gate and smoke-test the packaged toolchain, including dry-run install/uninstall PATH helpers and checksum verification.
- `powershell -ExecutionPolicy Bypass -File scripts/publish_github_release.ps1`: publish a GitHub prerelease for the clean current commit after the release gate passes.

## Planning And Tracking

- ClickUp folder: `Nous Lang` under the available `general` space.
- Current ClickUp lists:
  - `01 Project Foundation`
  - `01.5 Offline Browser Docs`
  - `02 Lexer Parser AST`
  - `03 Type System`
  - `04 Memory Runtime`
  - `05 IO Errors Syscalls`
  - `06 IR Optimization Codegen`
  - `07 CLI Build Installer`
  - `08 Tests Docs Release`
  - `09 Diagnostics UX`

## Update Rules

- Update this map when adding, moving, renaming, or deleting files or directories.
- Update this map when build, test, lint, fixture, release, or documentation commands change.
- Update this map when a document becomes canonical for a concept or stops being canonical.
- Keep this file factual. Do not use it for speculative design notes.
