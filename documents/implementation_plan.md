# Nous Lang (nlang) Compiler & Installer Implementation Plan

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

**Goal:** Create a complete toolchain and installer for Nous Lang (nlang) that allows users to compile and run `.nl` source files easily via the command line.

**Key Dependencies:** All 8 core language documents in this directory must be fully implemented and integrated into this plan.

## Current Implementation Checkpoint

The repository now contains the first executable alpha slice:

- `nous_lexer`: validates `.nl` source paths, tokenizes source text, emits indentation/dedent tokens, recognizes the current core keywords, and rejects curly braces and semicolon terminators.
- `nous_parser`: parses function declarations, typed parameters, return types, indentation-based bodies, `let` bindings, assignment, `return`, `break`, `continue`, expression statements, calls, array literals/indexing, arithmetic/comparison/logical expressions, `if`/`elif`/`else`, `while`, `loop`, and range `for` blocks into a structured AST.
- `nous_semantics`: performs static checks for duplicate declarations, binding initializer types, assignment target/type validity, function call argument counts/types, return values, bool conditions, loop-control placement, arithmetic/comparison/logical operands, homogeneous non-empty arrays, array indexes, pointer-style memory builtins, text file I/O builtins, and conservative system command builtins.
- `nous_runtime`: executes the validated AST in-process, including `main`, function calls, scoped locals, assignment, if/else branch values, while/infinite loops, range for loops, break/continue, array literals/indexing, arithmetic/comparison/logical expressions, `alloc`/`load`/`store`/`dealloc` heap slots, text file I/O, structured resource errors, and direct program-plus-argv system command calls.
- `nous_cli`: exposes `nlang check <file.nl>` and `nlang run <file.nl>` through Cargo.
- `nous_ir`: placeholder crate for the future semantic IR schema and lowering pipeline.
- `tests/fixtures`: valid and invalid `.nl` fixtures for frontend, semantic, runtime, and CLI smoke checks.
- `crates/nous_cli/tests`: integration tests that run the compiled CLI binary end to end.
- `offline_docs`: first self-contained offline browser documentation entry point plus a verifier for required alpha sections and local-only links/assets.

## Epic 1: Core Toolchain Implementation (Compiler & Runtime)
*Objective: Implement the core components defined by our design docs to parse, analyze, and execute nlang code.*

| Story | Description | Dependencies | Estimated Effort | Status |
| :--- | :--- | :--- | :--- | :--- |
| **1.1** | **Lexer & Parser Implementation:** Implement the core components to read raw nlang source code and convert it into an Abstract Syntax Tree (AST). | `nous_lang_syntax_design.md` | High | Alpha functions/expressions/arrays/conditionals/loops/range-for done; ongoing for full language |
| **1.2** | **Type System Integration:** Implement the type checker based on `nous_lang_type_system.md`. Ensure all AST nodes are correctly typed and checked before execution. | `nous_lang_type_system.md` | High | Alpha scalar/array/call/assignment/control-flow/logical checks done; ongoing for full language |
| **1.3** | **Memory System Implementation:** Implement the memory allocator/deallocator based on `nous_lang_memory_management.md`. Ensure ARC and explicit allocation work correctly in the runtime environment. | `nous_lang_memory_management.md` | High | Initial heap-slot `alloc`/`load`/`store`/`dealloc` builtins done; ARC/regions pending |
| **1.4** | **Runtime Execution Engine:** Implement the core execution loop that traverses the AST, manages memory, resolves types, and executes nlang instructions. | All previous steps | Critical | Alpha AST runtime with loops, range-for, arrays, and indexing done; native backend pending |

## Epic 1.5: Offline Browser Documentation
*Objective: Build and maintain a self-contained browser documentation bundle from ground zero so users can open a local HTML file and browse Nous Lang documentation without a server or internet connection. This artifact must later be bundle-ready for the language toolchain installer.*

| Story | Description | Dependencies | Estimated Effort | Status |
| :--- | :--- | :--- | :--- | :--- |
| **1.5.1** | **Documentation Information Architecture:** Define the offline documentation structure, navigation, entry page, and content ownership for overview, quick start, installation/setup, syntax reference, type system, memory model, control flow, examples, CLI usage, diagnostics, current limitations, and roadmap. | `language_specification.md`, `repository_map.md` | Medium | Initial local entry structure done; ongoing |
| **1.5.2** | **Static HTML Documentation Generator:** Create a local build path that turns canonical Markdown/source documentation into a self-contained HTML bundle with no required network access, CDN, external fonts, or development server. | 1.5.1 | High | Manual self-contained HTML v0 done; generator pending |
| **1.5.3** | **Offline Documentation Content Pass:** Bring the offline docs up to parity with the current language alpha: `.nl`, indentation-only blocks, functions, returns, `let`, assignment, control flow, memory builtins, CLI `check`/`run`, examples, and diagnostics. | 1.5.1, 1.5.2, Epic 1 | High | Alpha content v0 done; keep synchronized with Epic 1 |
| **1.5.4** | **Executable Example Sync:** Ensure examples shown in the offline docs are backed by fixtures or tests when they claim to work, and clearly mark future/planned syntax that is not yet accepted by the compiler. | 1.5.3, Epic 5 | High | To Do |
| **1.5.5** | **Offline Docs Verification:** Add checks that verify the generated HTML entry point exists, local links resolve, required sections are present, and the bundle can be opened from disk without missing assets. | 1.5.2, 1.5.3 | Medium | Initial verifier done |
| **1.5.6** | **Installer Bundle Integration:** Prepare the offline docs output for packaging with the future toolchain installer and document where the installer should place the local entry page. | Epic 3, Epic 4 | Medium | To Do |

## Epic 2: System Integration & I/O Layer
*Objective: Connect the runtime to the operating system and enable basic interaction.*

| Story | Description | Dependencies | Estimated Effort | Status |
| :--- | :--- | :--- | :--- | :--- |
| **2.1** | **I/O System Integration:** Implement the I/O layer based on `nous_lang_input_output.md`. Focus on file system access (reading/writing) for standard library operations. | `nous_lang_input_output.md` | High | Alpha text file builtins done: `read_file`, `write_file`, `append_file`, `file_exists`; streams/binary/mmap planned |
| **2.2** | **Error Handling Integration:** Integrate the error handling system (`nous_lang_error_handling.md`) into the runtime to ensure all compilation and runtime errors are gracefully reported in a structured format. | `nous_lang_error_handling.md` | Medium | Alpha structured runtime/resource error formatting done; language-level try/catch planned |
| **2.3** | **System Call Abstraction:** Define the interface for executing low-level OS commands (e.g., system calls) that nlang code can invoke safely. | N/A (New Design) | High | Alpha direct program-plus-argv abstraction done: `sys_status`, `sys_output`; no shell invocation |

## Epic 3: Build System & Distribution
*Objective: Create the infrastructure to compile the source code into an executable and package it for distribution.*

| Story | Description | Dependencies | Estimated Effort | Status |
| :--- | :--- | :--- | :--- | :--- |
| **3.1** | **Compiler Toolchain:** Implement the full compiler pipeline defined in `nous_lang_compilation_architecture.md` to handle source code compilation into machine-readable bytecode or an intermediate representation. | All Runtime Components | High | To Do |
| **3.2** | **Build Script Generation:** Create a robust, platform-agnostic build script (e.g., using CMake or a custom script) that orchestrates the compilation of the compiler and runtime into a single binary. | 3.1 | Medium | To Do |
| **3.3** | **Installer Creation:** Develop the installer logic to bundle the compiled nlang executable, necessary libraries, and documentation into a single user-friendly package (e.g., .exe or system package). | 3.2 | High | To Do |

## Epic 4: User Experience & Final Delivery
*Objective: Create the final, easy-to-use installation method.*

| Story | Description | Dependencies | Estimated Effort | Status |
| :--- | :--- | :--- | :--- | :--- |
| **4.1** | **CLI Tool Implementation:** Implement the command-line interface (CLI) tool that allows users to invoke the compiled nlang executable (`nlang run script.nl`). | 3.3 | Medium | Initial `check` and `run` commands done |
| **4.2** | **Installation & Setup:** Finalize the installation process, ensuring minimal user interaction and clear setup instructions are provided upon first launch. | 3.3, 4.1 | High | To Do |
| **4.3** | **Documentation Finalization:** Review all documentation to ensure they align with the final installed product's usage patterns. | All previous steps | Low | To Do |

## Epic 5: Testing & Verification (The Regression Shield)
*Objective: Establish a continuous feedback loop to ensure correctness, prevent regressions, and verify that all components interact as designed.*

| Story | Description | Dependencies | Estimated Effort | Status |
| :--- | :--- | :--- | :--- | :--- |
| **5.1** | **Unit Test Framework Setup:** Define the structure for unit tests (e.g., using a Python-based harness or custom nlang testing runner). This must be lightweight and fast, aligning with our minimalistic philosophy. | All previous components | Medium | In Progress |
| **5.2** | **Component Unit Testing:** Implement unit tests for each major component: Lexer (tokenization), Parser (AST generation), Memory Manager (allocation/deallocation), and Type Checker. | Stories 1.1, 1.2, 1.3 | High | In Progress |
| **5.3** | **Integration Test Suite:** Develop end-to-end integration tests that verify the entire pipeline: `Source Code` -> `AST` -> `Runtime Execution`. This ensures the compiler and runtime work together correctly. | All previous steps | Critical | In Progress |
| **5.4** | **Regression Test Protocol:** Establish a protocol for running the full suite (Unit + Integration) before any major feature addition or refactoring is committed to the codebase. | All previous steps | Medium | To Do |

## Epic 6: Diagnostics UX And Root-Cause Tracebacks
*Objective: Make every compiler, runtime, and host-resource failure clear enough for both humans and LLM agents to identify the root cause, understand the relevant source context, and apply a likely fix.*

| Story | Description | Dependencies | Estimated Effort | Status |
| :--- | :--- | :--- | :--- | :--- |
| **6.1** | **Unified Diagnostic Model:** Define a shared diagnostic structure with code, phase/category, severity, source path, span/range, function/context, primary message, explanation, root cause, suggested fix, and related notes. | Current lexer/parser/semantic/runtime diagnostic structs | High | To Do |
| **6.2** | **Verbose CLI Diagnostics:** Add `--verbose` diagnostics for `check` and `run` with source excerpts, caret markers, root-cause explanation, and fix guidance. | 6.1 | High | To Do |
| **6.3** | **Machine-Readable Diagnostics:** Add deterministic JSON output for `check` and `run`, suitable for editors, CI, and LLM agents. | 6.1 | Medium | To Do |
| **6.4** | **Semantic Source Spans:** Extend semantic diagnostics so type, name, control-flow, and builtin argument errors point to the exact offending AST node instead of only the containing function. | 6.1, `nous_parser` spans | High | To Do |
| **6.5** | **Runtime Tracebacks:** Track source context and lightweight call stacks through user-defined functions and failing builtins/expressions. | 6.1, `nous_runtime` | High | To Do |
| **6.6** | **Diagnostic Registry:** Document every stable `N####` code with meaning, likely cause, example, and suggested fix; keep Markdown and offline browser docs synchronized. | 6.1 | Medium | To Do |
| **6.7** | **Diagnostic Snapshot Tests:** Add representative CLI tests for lexer, parser, semantic, runtime, and resource diagnostics in concise, verbose, and JSON modes. | 6.2, 6.3 | High | To Do |
