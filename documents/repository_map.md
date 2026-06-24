# Repository Map

This file maps the repository layout and explains where to find core information. Keep it up to date whenever source files, directories, commands, tests, docs, or responsibilities change.

## Root

- [AGENTS.md](../AGENTS.md): operating guide for agents and contributors. Defines workflow, documentation rules, MCP/ClickUp/GitHub usage, testing expectations, Git rules, and where to find core language information.
- `Cargo.toml`: Rust workspace manifest for the Nous Lang compiler/toolchain crates.
- `.gitignore`: ignores local build outputs, caches, editor state, and generated artifacts once implementation begins.
- `.gitattributes`: normalizes repository text files to LF line endings.
- `crates/`: Rust implementation crates.
- `tests/`: shared `.nl` fixtures used by crate and CLI tests.
- `documents/`: core language documents and planning material.

## Documents

- `documents/core_language_rules.md`: canonical global rules for `.nl` source files, indentation-only scope, forbidden block delimiters, and no semicolon terminators.
- `documents/language_specification.md`: top-level language specification and overview. Use this first for language behavior, philosophy, syntax reference, examples, and roadmap.
- `documents/implementation_plan.md`: implementation plan for the compiler, runtime, CLI, installer, tests, and release workflow.
- `documents/nous_lang_syntax_design.md`: syntax design details for declarations, functions, data structures, operators, naming, comments, and examples.
- `documents/nous_lang_type_system.md`: type-system details for primitives, composites, references, functions, inference, safety, aliases, generics, and OS-specific types.
- `documents/nous_lang_memory_management.md`: memory model covering regions, stack allocation, heap allocation, lifetime tracking, GC hooks, safety checks, runtime memory APIs, and kernel memory examples.
- `documents/nous_lang_control_structures.md`: control flow and operators, including the current alpha if/while/loop rules plus planned switches, try/catch, coroutines, arithmetic/logical/bitwise operators, collections, conversions, and utility operations.
- `documents/nous_lang_input_output.md`: I/O and concurrency model, including files, streams, memory-mapped files, threads, processes, async, multiplexing, IPC, sockets, and performance strategies.
- `documents/nous_lang_error_handling.md`: error model, compact error tokens, compile-time and runtime categories, throw/catch/recovery behavior, diagnostics, and compiler integration.
- `documents/nous_lang_compilation_architecture.md`: compiler architecture from tokenization through semantic analysis, IR, optimization, code generation, linking, and binary verification.
- `documents/repository_map.md`: this file. Use it as the first navigation aid and update it with repository changes.

## Source Layout

The implementation is a Rust workspace. Unless changed by an explicit architecture decision, keep this layout:

- `crates/nous_lexer/`: source extension validation, tokenization, indentation scanning, forbidden brace/semicolon diagnostics, core keyword recognition, and lexical tests.
- `crates/nous_parser/`: AST model and parser for function declarations, typed parameters, return types, indentation blocks, `let`, assignment, `return`, `break`, `continue`, if/elif/else, while/loop, calls, literals, variables, arithmetic, and comparison expressions.
- `crates/nous_semantics/`: static validation for duplicate declarations, local binding types, assignment targets/types, function call arity/types, return behavior, bool conditions, loop-control placement, expression operand types, and interim pointer-style memory builtins.
- `crates/nous_ir/`: placeholder IR lowering crate for the future semantic IR schema.
- `crates/nous_runtime/`: in-process AST runtime for the current alpha subset, including `main`, function calls, scoped locals, assignment, branch results, while/loop execution, break/continue, arithmetic/comparison expressions, and `alloc`/`load`/`dealloc` heap-slot memory builtins.
- `crates/nous_cli/`: `nlang` command-line interface. Current commands: `check <file.nl>` and `run <file.nl>`.
- `crates/nous_cli/tests/`: binary-level integration tests for the CLI pipeline, including valid checks, runtime execution, lexical errors, and semantic errors.
- `tests/fixtures/valid/`: valid `.nl` smoke fixtures used by the frontend and CLI.
- `tests/fixtures/invalid/`: invalid source fixtures for diagnostics and negative tests.

## Current Commands

- `cargo fmt --check`: formatting check.
- `cargo test --all`: unit tests for all crates.
- `cargo clippy --all-targets --all-features -- -D warnings`: lint all crates and integration tests.
- `cargo run -p nous_cli -- check tests/fixtures/valid/add.nl`: check a valid fixture through source validation, lexing, parsing, and semantic validation.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_arithmetic.nl`: run a valid fixture through source validation, lexing, parsing, semantic validation, runtime execution, and stdout output.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_memory.nl`: run the current memory builtin fixture through `alloc`, `load`, and `dealloc`.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_while.nl`: run assignment plus while-loop execution.
- `cargo run -p nous_cli -- run tests/fixtures/valid/run_loop.nl`: run infinite-loop execution with break/continue.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/brace.nl`: verify forbidden block delimiter diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/type_mismatch.nl`: verify semantic type mismatch diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/assignment_type_mismatch.nl`: verify assignment type mismatch diagnostics.
- `cargo run -p nous_cli -- check tests/fixtures/invalid/break_outside_loop.nl`: verify loop-control placement diagnostics.

## Planning And Tracking

- ClickUp folder: `Nous Lang` under the available `general` space.
- Current ClickUp lists:
  - `01 Project Foundation`
  - `02 Lexer Parser AST`
  - `03 Type System`
  - `04 Memory Runtime`
  - `05 IO Errors Syscalls`
  - `06 IR Optimization Codegen`
  - `07 CLI Build Installer`
  - `08 Tests Docs Release`

## Update Rules

- Update this map when adding, moving, renaming, or deleting files or directories.
- Update this map when build, test, lint, fixture, release, or documentation commands change.
- Update this map when a document becomes canonical for a concept or stops being canonical.
- Keep this file factual. Do not use it for speculative design notes.
