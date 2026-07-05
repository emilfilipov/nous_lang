# Repository Map

This file maps the repository layout and explains where to find core information. Keep it up to date whenever source files, directories, commands, tests, docs, or responsibilities change.

## Root

- [CLAUDE.md](../CLAUDE.md): operating guide for agents and contributors. Defines workflow, documentation rules, MCP/ClickUp/GitHub usage, testing expectations, Git rules, and where to find core language information.
- [README.md](../README.md): user-facing project overview, portable package installation, checksum verification, local build commands, CLI summary, and documentation links.
- `Cargo.toml`: Rust workspace manifest for the Lullaby compiler/toolchain crates. The lockfile records the pinned dependency graph, including serde/serde_json for bytecode artifact serialization.
- `.gitignore`: ignores local build outputs, caches, editor state, and generated artifacts once implementation begins.
- `.gitattributes`: normalizes repository text files to LF line endings.
- `crates/`: Rust implementation crates.
- `examples/`: user-facing `.lullaby` examples packaged with the toolchain.
- `scripts/`: release packaging and verification scripts.
- `tests/`: shared `.lullaby` fixtures used by crate and CLI tests.
- `documents/`: core language documents and planning material.
- `offline_docs/`: self-contained browser documentation bundle that can be opened directly from disk.

## Documents

- `documents/core_language_rules.md`: canonical global rules for `.lullaby` source files, indentation-only scope, forbidden block delimiters, and no semicolon terminators.
- `documents/alpha1_language_surface.md`: frozen installable Alpha 1 feature surface for source rules, declarations, types, expressions, control flow, builtins, CLI commands, artifacts, diagnostics, packaging, and planned non-goals.
- `documents/formal_grammar.md`: formal EBNF-style draft for the implemented Alpha 1 parser grammar, including lexical/block rules, functions, statements, expressions, types, builtins-as-calls, planned syntax rejection, and the boundary between parsing and semantic validation.
- `documents/language_specification.md`: top-level language specification and overview. Use this first for language behavior, philosophy, syntax reference, examples, and roadmap.
- `documents/implementation_plan.md`: implementation plan for the compiler, runtime, offline browser documentation bundle, CLI, installer, tests, and release workflow.
- `documents/contributor_guide.md`: contributor checklist for adding or changing language features across lexer/parser/semantics/runtime/IR/CLI layers, fixtures, docs, offline docs, verification, commits, and ClickUp evidence.
- `documents/name_research.md`: rename research for replacing the Lullaby name, including candidate assessment, collision evidence, recommendation, and clearance checklist.
- `documents/post_alpha_roadmap.md`: repo-owned sequence for the current post-Alpha 1 backlog: memory-aware IR operations, static offline-doc generation, platform-agnostic build orchestration, installer packaging, and native code generation.
- `documents/portable_package_ci_workflow.yml`: GitHub Actions workflow template for explicit target-triple portable-package verification on Ubuntu, Windows, macOS ARM64, and macOS x64 runners. Copy it to `.github/workflows/portable-package.yml` from an authenticated session with GitHub `workflow` scope to activate it.
- `documents/alpha1_acceptance_criteria.md`: Alpha 1 release checklist covering required toolchain surface, documentation surface, verification gates, release evidence, non-goals, and the recommended next phase.
- `documents/alpha1_release_notes.md`: Alpha 1 package release notes, supported surface, commands, artifact contract, verification evidence, known limitations, and next-phase guidance.
- `documents/diagnostic_registry.md`: stable diagnostic code registry and output contract for concise, verbose, and JSON diagnostics.
- `documents/lullaby_syntax_design.md`: syntax design details for declarations, functions, data structures, operators, naming, comments, and examples.
- `documents/lullaby_type_system.md`: type-system details for primitives, composites, references, functions, inference, safety, aliases, generics, and OS-specific types.
- `documents/lullaby_memory_management.md`: memory model covering regions, stack allocation, heap allocation, lifetime tracking, GC hooks, safety checks, runtime memory APIs, and kernel memory examples.
- `documents/native_backend_contract.md`: Alpha 1 native backend contract for target family, internal calling convention, stack-frame slots, value layouts, pointer/array lowering rules, cleanup sequencing, and native diagnostics.
- `documents/lullaby_control_structures.md`: control flow and operators, including the current alpha if/while/loop rules plus planned switches, try/catch, coroutines, arithmetic/logical/bitwise operators, collections, conversions, and utility operations.
- `documents/lullaby_input_output.md`: I/O and concurrency model, including files, streams, memory-mapped files, threads, processes, async, multiplexing, IPC, sockets, and performance strategies.
- `documents/lullaby_error_handling.md`: error model, compact error tokens, compile-time and runtime categories, throw/catch/recovery behavior, diagnostics, and compiler integration.
- `documents/lullaby_compilation_architecture.md`: compiler architecture from tokenization through semantic analysis, IR, optimization, code generation, linking, and binary verification.
- `documents/repository_map.md`: this file. Use it as the first navigation aid and update it with repository changes.

## Offline Browser Docs

- `offline_docs/index.html`: checked-in local browser entry point for alpha user documentation. It remains self-contained with no server, CDN, remote fonts, or internet dependency while generated docs are now used for packaging.
- `offline_docs/generate_offline_docs.py`: standard-library generator that renders canonical Markdown sources plus package-ready quick start, CLI reference, package layout, diagnostics, limitations, shipped-doc parity sections, and fixture-backed examples into a self-contained HTML bundle under `target/offline_docs/index.html` by default.
- `offline_docs/verify_offline_docs.py`: deterministic verifier for the checked-in and generated offline docs entry points, required sections, required alpha topics, generated package-ready user sections, generated parity coverage, local anchors, lack of remote dependencies, and fixture-backed executable examples.

## CI

- `documents/portable_package_ci_workflow.yml`: workflow template for formatting, workspace tests, clippy, shipped offline docs, generated offline docs, and explicit target-triple portable package driver verification on Ubuntu, Windows, macOS ARM64, and macOS x64 GitHub Actions runners. The template is not active until copied under `.github/workflows/` with GitHub `workflow` scope.

## Examples

- `examples/README.md`: user-facing instructions for valid and invalid example programs.
- `examples/valid/`: executable `.lullaby` examples for calculator, arrays/control flow, file I/O, and Windows system command status.
- `examples/invalid/`: intentionally invalid `.lullaby` examples for inspecting diagnostics.

## Source Layout

The implementation is a Rust workspace. Unless changed by an explicit architecture decision, keep this layout:

- `crates/lullaby_lexer/`: source extension validation, tokenization, indentation scanning, forbidden brace/semicolon diagnostics, current and planned keyword recognition, and lexical tests.
- `crates/lullaby_diagnostics/`: shared diagnostic data structures, serializable span/traceback metadata, registry metadata, concise/verbose renderers, and deterministic JSON rendering.
- `crates/lullaby_parser/`: AST model and parser for function declarations, typed parameters, return types, indentation blocks, explicit and inferred `let`, assignment, `return`, `break`, `continue`, if/elif/else, while/loop/range-for, calls, literals, array literals/indexing, variables, arithmetic, comparison, logical expressions, and `N0211` rejection for planned syntax keywords that are not in Alpha 1.
- `crates/lullaby_parser/tests/`: parser integration tests, including checked-in AST golden snapshots under `crates/lullaby_parser/tests/snapshots/`. Refresh snapshots intentionally with `$env:LULLABY_UPDATE_PARSER_SNAPSHOTS='1'; cargo test -p lullaby_parser --test ast_snapshots; Remove-Item Env:LULLABY_UPDATE_PARSER_SNAPSHOTS`.
- `crates/lullaby_semantics/`: static validation for duplicate declarations, explicit and inferred local binding types, assignment targets/types, function call arity/types, return behavior, bool conditions, loop-control placement, arithmetic/comparison/logical expression operand types, homogeneous non-empty arrays, array indexing, interim pointer-style memory builtins, text file I/O builtins, standard stream builtins (`print`/`println`/`warn`/`flush`), safe system command builtins, and executable zero-argument `main` validation for compile/run. Successful validation returns `CheckedProgram` with function signatures and inferred expression-type metadata.
- `crates/lullaby_ir/`: typed semantic IR schema, lowering from `CheckedProgram`, Alpha 1 memory-operation analysis with sequence and safety metadata for allocation/load/store/deallocation and bounds-checked indexing, reserved safety semantics for future region/copy/cleanup operations, serializable native backend contract data for Alpha 1 target/layout/calling/cleanup policy, first `x86_64-pc-windows-msvc` COFF object-emission prototype for literal-return, stack-backed `i64` local arithmetic, and straight-line `i64` assignment entry functions, deterministic optimization pass configuration with opt-in constant folding, conservative block-local CSE, conservative loop-invariant motion, conservative block-local copy propagation, block-local dead-code elimination, executable IR interpreter, inline and fixture-driven AST/IR/bytecode parity tests including optimized backend variants, explicit instruction-bytecode lowering, versioned `.lbc` artifact encoding/decoding with metadata/function-table/memory-operation/instruction-contract compatibility checks, checked-in backend memory metadata snapshots, and bytecode VM entry point for the current alpha subset.
- `crates/lullaby_ir/tests/`: integration tests and checked-in snapshots for backend metadata. `tests/memory_snapshots.rs` compares bytecode memory-operation metadata for representative Alpha 1 fixtures against `tests/snapshots/*.memory.json`; `tests/native_contract_snapshots.rs` compares the Alpha 1 native backend contract against `tests/snapshots/alpha1_native_backend_contract.json`; `tests/native_object_snapshots.rs` compares literal-return, local-arithmetic, and assignment COFF object-emission output against `tests/snapshots/alpha1_return_42.coff.json`, `tests/snapshots/alpha1_locals_add.coff.json`, and `tests/snapshots/alpha1_assignments.coff.json`.
- `crates/lullaby_runtime/`: in-process AST runtime for the current alpha subset, including `main`, function calls, scoped locals, assignment, branch results, while/loop/range-for execution, break/continue, array literals/indexing with runtime bounds checks, arithmetic/comparison/logical expressions with short-circuiting, `alloc`/`load`/`store`/`dealloc` heap-slot memory builtins, text file I/O builtins, standard stream builtins (`print`/`println`/`warn`/`flush`), direct program-plus-argv system command builtins, and categorized runtime/resource errors.
- `crates/lullaby_cli/`: `lullaby` command-line interface. Current commands: `check [--verbose|--format json] <file.lullaby>`, `compile [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lullaby>`, `build [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lullaby>`, `inspect [--verbose|--format json] <file.lbc>`, `run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] [--verbose|--format json] <file.lullaby>`, `run [--verbose|--format json] <file.lbc>`, `docs`, `examples`, `help`, and `--version`. `check` allows helper/library-style validation without `main`; `compile`, `build`, and source `run` require zero-argument `main`; `inspect` reports artifact metadata, function signatures, memory operation metadata, and verbose/JSON memory operation sequence numbers. `build` is an alias for the artifact-generation path used by `compile`. The crate declares an explicit `lullaby` binary target for release packaging.
- `crates/lullaby_cli/tests/`: binary-level integration tests for the CLI pipeline, including valid checks, backend execution, bytecode artifact inspection/execution, malformed artifact and invalid instruction-contract diagnostics, lexical errors, and semantic errors.
- `scripts/package_windows_portable.ps1`: builds the release CLI, creates `dist\lullaby-alpha1-windows-x64\`, copies `bin\lullaby.exe`, generates and verifies `docs\index.html`, bundles release notes, user-facing examples, and optional PATH install/uninstall helpers, copies a repository license file if one exists, writes package metadata/readme files, and creates a `.zip` archive plus `.sha256` checksum file.
- `scripts/package_portable.py`: cross-platform portable package driver. It builds or reuses the release CLI, generates offline docs into the package, verifies generated docs, copies examples/release notes/license metadata and platform PATH helpers, writes `README.txt` and `MANIFEST.json`, creates a `.zip` or `.tar.gz` archive plus `.sha256`, and can run host-compatible package smoke tests with `--verify`.
- `scripts/verify_markdown_refs.ps1`: verifies file-like Markdown links, backticked `.md` references, and stale-source markers while ignoring language syntax examples that only resemble Markdown links.
- `scripts/verify_release.ps1`: runs formatting, test, clippy, shipped offline-doc verification, generated offline-doc build/verification, Markdown reference verification, builds the Windows portable package, verifies the cross-platform portable package driver, and smoke-tests the packaged `lullaby.exe` against version, docs, examples, check, run, compile, build, bytecode artifact inspection/execution, invalid example diagnostics, dry-run PATH install/uninstall helpers, and archive checksum verification.
- `scripts/publish_github_release.ps1`: requires a clean worktree, runs release verification, creates/pushes a release tag, and creates a GitHub prerelease with the portable zip and checksum assets using `documents/alpha1_release_notes.md` as the release body.
- `scripts/install_windows_path.ps1` and `scripts/uninstall_windows_path.ps1`: package-root PowerShell helpers copied as `install.ps1` and `uninstall.ps1` for optional user PATH setup/cleanup.
- `scripts/install.cmd` and `scripts/uninstall.cmd`: cmd wrappers copied to the package root for users who prefer `cmd.exe`.
- `scripts/install_unix_path.sh` and `scripts/uninstall_unix_path.sh`: package-root POSIX shell helpers copied as `install.sh` and `uninstall.sh` for optional user PATH setup/cleanup in Linux and macOS portable archives.
- `tests/fixtures/valid/`: valid `.lullaby` smoke fixtures used by the frontend, CLI, and offline documentation example verification. Files prefixed with `docs_` back executable examples shown in `offline_docs/index.html`.
- `tests/fixtures/invalid/`: invalid source fixtures for diagnostics and negative tests, including planned-but-unsupported syntax such as imports, modules, structs, try, and catch, plus executable entry-point failures such as missing `main` and parameterized `main`.

## Current Commands

- `cargo fmt --check`: formatting check.
- `cargo test --all`: unit tests for all crates.
- `cargo clippy --all-targets --all-features -- -D warnings`: lint all crates and integration tests.
- `cargo build --release -p lullaby_cli`: build the release `lullaby.exe` binary.
- `cargo run -p lullaby_cli -- check tests/fixtures/valid/add.lullaby`: check a valid fixture through source validation, lexing, parsing, and semantic validation.
- `cargo run -p lullaby_cli -- check --verbose tests/fixtures/invalid/brace.lullaby`: print verbose source excerpt, root-cause, and suggested-fix diagnostics.
- `cargo run -p lullaby_cli -- check --format json tests/fixtures/invalid/type_mismatch.lullaby`: print deterministic JSON diagnostics. `--diagnostic-format json` is also accepted.
- `cargo run -p lullaby_cli -- run --verbose tests/fixtures/invalid/array_index_out_of_bounds.lullaby`: print runtime diagnostics with source context and traceback frames.
- `cargo run -p lullaby_cli -- compile --optimize alpha -o target/run_arithmetic.lbc tests/fixtures/valid/run_arithmetic.lullaby`: compile a valid source fixture into a versioned `.lbc` instruction-bytecode artifact with metadata, a function table, and dedicated function instructions.
- `cargo run -p lullaby_cli -- build --optimize alpha -o target/run_arithmetic.lbc tests/fixtures/valid/run_arithmetic.lullaby`: build the same versioned `.lbc` instruction-bytecode artifact through the build-oriented alias.
- `cargo run -p lullaby_cli -- inspect target/run_arithmetic.lbc`: print `.lbc` artifact metadata, function signatures, and memory operation counts without executing it; verbose/JSON modes include memory operation sequence numbers.
- `cargo run -p lullaby_cli -- run target/run_arithmetic.lbc`: execute a compiled `.lbc` bytecode artifact.
- `cargo run -p lullaby_cli -- docs`: print the local offline documentation entry path.
- `cargo run -p lullaby_cli -- examples`: print the local example fixture directory path.
- `cargo run -p lullaby_cli -- run examples/valid/calculator.lullaby`: run a user-facing packaged example.
- `cargo run -p lullaby_cli -- check examples/invalid/type_mismatch.lullaby`: inspect a user-facing invalid example diagnostic.
- `cargo run -p lullaby_cli -- run tests/fixtures/valid/run_arithmetic.lullaby`: run a valid fixture through source validation, lexing, parsing, semantic validation, runtime execution, and stdout output.
- `cargo run -p lullaby_cli -- run tests/fixtures/valid/run_inferred_let.lullaby`: run initializer-inferred local bindings through source validation, semantic inference, runtime execution, and stdout output.
- `cargo run -p lullaby_cli -- run --backend ir tests/fixtures/valid/run_arithmetic.lullaby`: run a valid fixture through typed IR lowering and the IR interpreter.
- `cargo run -p lullaby_cli -- run --backend bytecode tests/fixtures/valid/run_arithmetic.lullaby`: run a valid fixture through typed IR lowering, instruction-bytecode lowering, and the bytecode VM entry point.
- `cargo run -p lullaby_cli -- run --backend ir --optimize constant-fold tests/fixtures/valid/run_logic.lullaby`: run a valid fixture through typed IR lowering, the opt-in constant-folding pass, and the IR interpreter.
- `cargo run -p lullaby_cli -- run --backend bytecode --optimize constant-fold tests/fixtures/valid/run_logic.lullaby`: run a valid fixture through typed IR lowering, the opt-in constant-folding pass, instruction-bytecode lowering, and the bytecode VM entry point.
- `cargo run -p lullaby_cli -- run --backend ir --optimize dead-code tests/fixtures/valid/run_arithmetic.lullaby`: run a valid fixture through typed IR lowering, block-local dead-code elimination, and the IR interpreter.
- `cargo run -p lullaby_cli -- run --backend bytecode --optimize alpha tests/fixtures/valid/run_arithmetic.lullaby`: run a valid fixture through typed IR lowering, the current alpha optimizer pipeline of constant folding, CSE, loop-invariant motion, copy propagation, and dead-code elimination, instruction-bytecode lowering, and the bytecode VM entry point.
- `cargo run -p lullaby_cli -- run tests/fixtures/valid/run_memory.lullaby`: run the current memory builtin fixture through `alloc`, `load`, and `dealloc`.
- `cargo run -p lullaby_cli -- run tests/fixtures/valid/run_store.lullaby`: run heap-slot mutation through `alloc`, `store`, `load`, and `dealloc`.
- `cargo run -p lullaby_cli -- run tests/fixtures/valid/run_while.lullaby`: run assignment plus while-loop execution.
- `cargo run -p lullaby_cli -- run tests/fixtures/valid/run_loop.lullaby`: run infinite-loop execution with break/continue.
- `cargo run -p lullaby_cli -- run tests/fixtures/valid/run_logic.lullaby`: run boolean logic with `and`, `or`, and `not`.
- `cargo run -p lullaby_cli -- run tests/fixtures/valid/run_for.lullaby`: run inclusive range-for execution.
- `cargo run -p lullaby_cli -- run tests/fixtures/valid/run_for_step.lullaby`: run stepped and descending range-for execution.
- `cargo run -p lullaby_cli -- run tests/fixtures/valid/run_array.lullaby`: run homogeneous array literals and bounds-checked indexing.
- `cargo run -p lullaby_cli -- run tests/fixtures/valid/run_file_io.lullaby`: run text file write, append, and read builtins.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/brace.lullaby`: verify forbidden block delimiter diagnostics.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/missing_indented_body.lullaby`: verify parser diagnostics for missing indentation.
- `cargo test -p lullaby_parser --test ast_snapshots`: verify selected valid `.lullaby` fixtures still produce the checked-in parser AST golden snapshots.
- `cargo test -p lullaby_ir --test memory_snapshots`: verify selected valid `.lullaby` fixtures still produce the checked-in bytecode memory metadata golden snapshots.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/type_mismatch.lullaby`: verify semantic type mismatch diagnostics.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/assignment_type_mismatch.lullaby`: verify assignment type mismatch diagnostics.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/break_outside_loop.lullaby`: verify loop-control placement diagnostics.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/logical_type_mismatch.lullaby`: verify logical operand type diagnostics.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/ordering_type_mismatch.lullaby`: verify non-numeric ordering comparison diagnostics.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/for_range_type_mismatch.lullaby`: verify range-for bound type diagnostics.
- `cargo run -p lullaby_cli -- run tests/fixtures/invalid/for_zero_step.lullaby`: verify range-for zero-step runtime diagnostics.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/array_literal_type_mismatch.lullaby`: verify homogeneous array literal diagnostics.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/array_index_type_mismatch.lullaby`: verify array index type diagnostics.
- `cargo run -p lullaby_cli -- run tests/fixtures/invalid/array_index_out_of_bounds.lullaby`: verify runtime array bounds diagnostics.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/store_type_mismatch.lullaby`: verify `store` value type diagnostics.
- `cargo run -p lullaby_cli -- run tests/fixtures/invalid/store_after_dealloc.lullaby`: verify invalid pointer diagnostics after deallocation.
- `cargo run -p lullaby_cli -- run tests/fixtures/invalid/read_missing_file.lullaby`: verify structured resource diagnostics for missing file reads.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/read_file_path_type.lullaby`: verify file builtin path type diagnostics.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/write_file_content_type.lullaby`: verify file builtin content type diagnostics.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/sys_args_type.lullaby`: verify system command builtin argv type diagnostics.
- `cargo run -p lullaby_cli -- check tests/fixtures/invalid/unsupported_import.lullaby`: verify `N0211` planned-syntax diagnostics.
- `cargo run -p lullaby_cli -- compile -o target/missing_main.lbc tests/fixtures/invalid/missing_main.lullaby`: verify `N0329` executable entry-point diagnostics.
- `$env:LULLABY_UPDATE_PARSER_SNAPSHOTS='1'; cargo test -p lullaby_parser --test ast_snapshots; Remove-Item Env:LULLABY_UPDATE_PARSER_SNAPSHOTS`: intentionally refresh parser AST golden snapshots after reviewing expected AST-shape changes.
- `$env:LULLABY_UPDATE_IR_MEMORY_SNAPSHOTS='1'; cargo test -p lullaby_ir --test memory_snapshots; Remove-Item Env:LULLABY_UPDATE_IR_MEMORY_SNAPSHOTS`: intentionally refresh bytecode memory metadata golden snapshots after reviewing expected backend metadata changes.
- `cargo test -p lullaby_ir native_contract`: verify the Alpha 1 native backend contract for target selection, value layouts, cleanup sequencing, and serializable JSON stability.
- `cargo test -p lullaby_ir --test native_contract_snapshots`: verify the checked-in Alpha 1 native backend contract snapshot.
- `$env:LULLABY_UPDATE_NATIVE_CONTRACT_SNAPSHOTS='1'; cargo test -p lullaby_ir --test native_contract_snapshots; Remove-Item Env:LULLABY_UPDATE_NATIVE_CONTRACT_SNAPSHOTS`: intentionally refresh the native backend contract golden snapshot after reviewing expected ABI/layout policy changes.
- `cargo test -p lullaby_ir native_object`: verify the first Alpha 1 native object emitter for minimal COFF output, stack-backed `i64` local arithmetic and assignments, and unsupported-body errors.
- `cargo test -p lullaby_ir --test native_object_snapshots`: verify the checked-in literal-return, local-arithmetic, and assignment COFF object-emission snapshots.
- `$env:LULLABY_UPDATE_NATIVE_OBJECT_SNAPSHOTS='1'; cargo test -p lullaby_ir --test native_object_snapshots; Remove-Item Env:LULLABY_UPDATE_NATIVE_OBJECT_SNAPSHOTS`: intentionally refresh the native object golden snapshot after reviewing expected object-emission changes.
- Running a malformed `.lbc` artifact verifies `N0601 [bytecode error]` diagnostics for unsupported bytecode artifacts and invalid instruction contracts such as top-level `break`/`continue`.
- `python offline_docs/verify_offline_docs.py`: verify the shipped self-contained offline browser documentation entry point, including metadata, fixture content, compile/run/inspect/examples command coverage, and `lullaby check`/`lullaby run` execution for documented `.lullaby` examples.
- `python offline_docs/generate_offline_docs.py`: generate the Markdown-sourced offline documentation bundle with package-ready quick start, CLI reference, package layout, diagnostics, limitations, shipped-doc parity sections, and fixture-backed examples to `target/offline_docs/index.html`.
- `python offline_docs/verify_offline_docs.py target/offline_docs/index.html --profile generated`: verify the generated offline docs bundle, including source-section coverage, generated package-ready user sections, shipped-doc parity requirements, local-only links, and fixture-backed executable examples.
- `powershell -ExecutionPolicy Bypass -File scripts/verify_markdown_refs.ps1`: verify file-like Markdown links, backticked `.md` references, and stale-source markers without misclassifying language syntax examples.
- `powershell -ExecutionPolicy Bypass -File scripts/package_windows_portable.ps1`: build the Windows Alpha 1 portable package, zip archive, and SHA-256 checksum under `dist/`.
- `python scripts/package_portable.py --verify`: build the host portable package with generated offline docs, create an archive plus checksum under `dist/`, and run host-compatible package smoke tests.
- `python scripts/package_portable.py --target <triple> --target-tag <os-arch>`: build a portable package for an explicit Cargo target triple and package tag; executable smoke tests run only when the package target matches the host.
- `powershell -ExecutionPolicy Bypass -File scripts/verify_release.ps1`: run the full Alpha 1 release gate and smoke-test the packaged toolchain, including Markdown reference verification, `lullaby build`, dry-run install/uninstall PATH helpers, and checksum verification.
- `powershell -ExecutionPolicy Bypass -File scripts/publish_github_release.ps1`: publish a GitHub prerelease for the clean current commit after the release gate passes.

## Planning And Tracking

- ClickUp folder: `Lullaby` under the available `general` space.
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
