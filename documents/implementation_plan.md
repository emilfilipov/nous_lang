# Nous Lang (nlang) Compiler & Installer Implementation Plan

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

**Goal:** Create a complete toolchain and installer for Nous Lang (nlang) that allows users to compile and run `.nl` source files easily via the command line.

**Key Dependencies:** All 8 core language documents in this directory must be fully implemented and integrated into this plan.

## Current Implementation Checkpoint

The repository now contains the first executable alpha slice:

- `nous_lexer`: validates `.nl` source paths, tokenizes source text, emits indentation/dedent tokens, recognizes the current core keywords, and rejects curly braces and semicolon terminators.
- `nous_parser`: parses function declarations, typed parameters, return types, indentation-based bodies, `let` bindings, assignment, `return`, `break`, `continue`, expression statements, calls, arithmetic/comparison expressions, `if`/`elif`/`else`, `while`, and `loop` blocks into a structured AST.
- `nous_semantics`: performs static checks for duplicate declarations, binding initializer types, assignment target/type validity, function call argument counts/types, return values, bool conditions, loop-control placement, arithmetic/comparison operands, and the first pointer-style memory builtins.
- `nous_runtime`: executes the validated AST in-process, including `main`, function calls, scoped locals, assignment, if/else branch values, while/infinite loops, break/continue, arithmetic/comparison expressions, and `alloc`/`load`/`dealloc` heap slots.
- `nous_cli`: exposes `nlang check <file.nl>` and `nlang run <file.nl>` through Cargo.
- `nous_ir`: placeholder crate for the future semantic IR schema and lowering pipeline.
- `tests/fixtures`: valid and invalid `.nl` fixtures for frontend, semantic, runtime, and CLI smoke checks.
- `crates/nous_cli/tests`: integration tests that run the compiled CLI binary end to end.

## Epic 1: Core Toolchain Implementation (Compiler & Runtime)
*Objective: Implement the core components defined by our design docs to parse, analyze, and execute nlang code.*

| Story | Description | Dependencies | Estimated Effort | Status |
| :--- | :--- | :--- | :--- | :--- |
| **1.1** | **Lexer & Parser Implementation:** Implement the core components to read raw nlang source code and convert it into an Abstract Syntax Tree (AST). | `nous_lang_syntax_design.md` | High | Alpha functions/expressions/conditionals/loops done; ongoing for full language |
| **1.2** | **Type System Integration:** Implement the type checker based on `nous_lang_type_system.md`. Ensure all AST nodes are correctly typed and checked before execution. | `nous_lang_type_system.md` | High | Alpha scalar/call/assignment/control-flow checks done; ongoing for full language |
| **1.3** | **Memory System Implementation:** Implement the memory allocator/deallocator based on `nous_lang_memory_management.md`. Ensure ARC and explicit allocation work correctly in the runtime environment. | `nous_lang_memory_management.md` | High | Initial heap-slot builtins done; ARC/regions pending |
| **1.4** | **Runtime Execution Engine:** Implement the core execution loop that traverses the AST, manages memory, resolves types, and executes nlang instructions. | All previous steps | Critical | Alpha AST runtime with loops done; native backend pending |

## Epic 2: System Integration & I/O Layer
*Objective: Connect the runtime to the operating system and enable basic interaction.*

| Story | Description | Dependencies | Estimated Effort | Status |
| :--- | :--- | :--- | :--- | :--- |
| **2.1** | **I/O System Integration:** Implement the I/O layer based on `nous_lang_input_output.md`. Focus on file system access (reading/writing) for standard library operations. | `nous_lang_input_output.md` | High | To Do |
| **2.2** | **Error Handling Integration:** Integrate the error handling system (`nous_lang_error_handling.md`) into the runtime to ensure all compilation and runtime errors are gracefully reported in a structured format. | `nous_lang_error_handling.md` | Medium | To Do |
| **2.3** | **System Call Abstraction:** Define the interface for executing low-level OS commands (e.g., system calls) that nlang code can invoke safely. | N/A (New Design) | High | To Do |

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
