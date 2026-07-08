# Lullaby Compilation Architecture Documentation

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

## Overview

Lullaby implements a novel multi-phase compilation pipeline specifically optimized for:
1. **Token efficiency** - Minimize source code token count for LLM generation
2. **Compile-time optimization** - Perform optimizations during compilation, not runtime
3. **Systems programming focus** - Direct hardware interaction with minimal abstraction overhead
4. **LLM compatibility** - Design patterns that tiny LLMs can understand and generate

## Compilation Pipeline Stages

## Current Alpha Pipeline

The current Rust workspace implements a frontend and in-process execution pipeline:

1. `lullaby_lexer` validates `.lby` paths, emits tokens, emits indentation/dedent structure, and rejects forbidden block delimiters.
2. `lullaby_parser` builds an AST for functions, typed parameters, `let`, assignment, returns, break/continue, if/elif/else, while/loop/range-for blocks, calls, literals, array literals/indexing, variables, arithmetic, comparisons, and boolean logic.
3. `lullaby_semantics` validates static types, explicit and inferred local bindings, assignments, function calls, return behavior, bool conditions, loop-control placement, arithmetic/comparison/logical operands, homogeneous non-empty arrays, array indexes, interim pointer-style memory builtins, text file I/O builtins, and safe system command builtins. Successful validation returns `CheckedProgram` metadata with function signatures and inferred expression types.
4. `lullaby_ir` lowers a `CheckedProgram` into typed semantic IR for the current alpha subset, including typed functions, parameters, statements, control flow, calls, builtins, and expressions. It also exposes memory-operation analysis for current heap-slot operations and array bounds checks so optimizers, bytecode work, and later native backends can share one side-effect and safety model.
5. `lullaby_runtime` executes the validated AST directly, including `main`, calls, scoped locals, assignment, branch result values, while/loop/range-for control flow, array literals/indexing with runtime bounds checks, arithmetic/comparisons, short-circuit boolean logic, heap-slot memory operations including `alloc`/`load`/`store`/`dealloc`, text file I/O, and safe system command builtins.
6. `lullaby_ir` provides a deterministic optimization pass framework. Implemented passes are constant folding for pure literal arithmetic, comparisons, boolean logic, string equality, and unary `not`, conservative block-local common subexpression elimination for repeated pure bindings, conservative loop-invariant motion for safe loop-body bindings, conservative block-local copy propagation for simple variable aliases, plus dead-code elimination for statements after unconditional `return`, `break`, or `continue` in the same block. Constant folding and loop-invariant motion deliberately leave potentially failing divide-by-zero expressions in place so runtime diagnostics and zero-iteration loop behavior are preserved. Optimizer barriers are conservative around calls and bounds-checked indexing.
7. `lullaby_ir` can also execute the lowered typed IR, lower it into an explicit instruction-bytecode module, and encode/decode a versioned `.lbc` bytecode artifact for the current alpha subset.
8. `lullaby_cli` exposes the current pipeline as `lullaby check <file.lby>`, `lullaby compile [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] <file.lby>`, `lullaby build [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] <file.lby>`, `lullaby inspect <file.lbc>`, `lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] <file.lby|file.lbc>`, `lullaby docs`, and `lullaby examples`. Optimization is opt-in and applies only to IR/bytecode source runs and compiled bytecode artifacts.

Additional optimization passes, native code generation, linking, and binary output remain planned architecture stages.

### Memory-Aware IR Contract

The current memory-aware IR increment keeps `alloc`, `load`, `store`, `dealloc`, and array indexing in the existing expression/statement IR shape while adding an analysis contract for backend consumers:

- `Allocate`: records the initialized value type and produced pointer type.
- `Load`: records the pointer type and loaded value type.
- `Store`: records the pointer type and stored value type.
- `Deallocate`: records the released pointer type.
- `BoundsCheck`: records the indexed target type and index type.

Each operation carries safety metadata for live-resource requirements, bounds-check requirements, memory mutation, cleanup role, and unsafe-boundary handling. Region creation/resizing, copy operations, and compiler-inserted cleanup are reserved in the roadmap but are not emitted until the language surface and runtime model support them.

### Native Backend Contract

`lullaby_ir::native_contract` records the first Alpha 1 native backend contract before machine-code output exists. It defines the first prototype target, supported 64-bit target family, internal calling convention, stack-frame slot classes, current value layouts, pointer and array lowering rules, cleanup sequencing, and native diagnostic requirements.

The contract is serializable and unit-tested so object-emission work can consume stable data instead of embedding target policy directly into lowering code. See [native_backend_contract.md](native_backend_contract.md).

`lullaby_ir::native_object` now provides the first object-emission prototype for `x86_64-pc-windows-msvc`. It emits a deterministic COFF object for a zero-argument `main` that returns a literal `i64`, literal `bool`, `void`, a stack-backed `i64` local arithmetic expression, or straight-line `i64` local assignment arithmetic, after the source has already passed semantic validation, typed IR lowering, and bytecode lowering. Broader instruction lowering, linker orchestration, and native runtime packaging remain planned work.

Within otherwise-`i64` functions, the native backend also compiles the fixed-width integer types (`i8`/`i16`/`i32`/`u8`/`u16`/`u32`/`u64`/`isize`/`usize`). Each fixed-width value is held in a 64-bit register as the same normalized `i64` cell the interpreters use (signed kinds sign-extended, unsigned zero-extended, the 64-bit kinds filling the cell). The backend emits wrapping arithmetic (`+ - * /`) re-normalized to the width, signedness-correct comparison (unsigned condition codes for unsigned kinds, signed for signed) and division (`div`/`idiv`), bitwise `& | ^ ~`, shifts with the count masked to the width and right shift logical-for-unsigned / arithmetic-for-signed, and the `to_<T>`/`to_i64` conversions emitted inline as a width-normalize. This matches the AST/IR/bytecode interpreters bit-for-bit (verified by the native link-and-run parity tests).

The same otherwise-`i64` functions may now also compute with **floating-point values** (`f64` and `f32`) using SSE scalar (XMM) registers. Float values live in XMM as a `double` (`f64`) or a `single` (`f32`, kept rounded to single precision after every operation to match the interpreter's real `f32` storage), spilled to 8-byte frame slots when a temporary must be held across another evaluation. The backend emits IEEE-754 arithmetic `+ - * /` (`addsd`/`subsd`/`mulsd`/`divsd` for `f64`; `addss`/`subss`/`mulss`/`divss` for `f32`, so division by zero yields inf/NaN rather than trapping, exactly like the interpreters); ordered comparisons `< <= > >= == !=` (`ucomisd`/`ucomiss` with unordered-aware condition codes, so a NaN operand makes every relational compare and `==` false and `!=` true); and the conversions `to_f32(x f64) -> f32` (`cvtsd2ss`, round to single) and `to_f64(x f32) -> f64` (`cvtss2sd`, widen), recognized and inlined as builtin calls. Float literals are materialized without a data-section constant: the IEEE-754 bit pattern is moved through a GPR into an XMM register (`movq`/`movd`). The signature constraint is unchanged — a function's parameters and return type must all be `i64` — so functions with a **float parameter or return type**, float values on the **heap**, and the transcendental/math builtins (`sqrt`/`sin`/`floor`/…) remain deferred and skip gracefully to the interpreters. Native float results match the AST/IR/bytecode interpreters bit-for-bit (verified by the native link-and-run parity tests).

Heap values, the option-returning `checked_*` builtins, and the `saturating_*`/`wrapping_*` builtins remain deferred: functions touching them are skipped and still run on the interpreters.

### Stage 1: Lexical Analysis (Tokenizer)

Converts raw source text into a stream of tokens optimized for compact representation.

#### Token Types
- **Identifier Tokens**: `ident` (alphanumeric names without underscores)
- **Keyword Tokens**: Reserved words (`func`, `if`, `then`, `end`, etc.)
- **Operator Tokens**: Mathematical and logical operators as single symbols
- **Literal Tokens**:
  - Numbers: `num` (decimal, hex, binary formats)
  - Strings: `str` (without quotes - implicit string literals)
  - Booleans: `bool` (`true`, `false`)

#### Token Stream Optimization
```lullaby
source_code = tokenizer.parse(source_text)
stream = stream.compact(source_code)
```

The tokenizer uses context-aware parsing to reduce token count by ~40% compared to traditional languages:
- No parentheses needed (operator precedence handled by position)
- No semicolons (statement termination implicit via newline or block markers)
- No quotes for strings (implicit string delimiters removed)

#### Example Tokenization
```lullaby
// Source code snippet: x = 42 + "hello world"
// Traditional language tokens: 9 tokens
// Lullaby tokens: 5 tokens

Traditional: IDENT(x), OP(=), NUM(42), OP(+), STR("hello", "world"), ENDSTR
Lullaby:   ident(x), assign, num(42), plus, str(helloworld)
```

### Stage 2: Semantic Analysis (Type Checker)

Performs static type checking and generates intermediate representation (IR).

#### Type System Features
- **Static Typing**: Types inferred or explicitly declared
- **Type Safety**: Compile-time validation of type operations
- **Minimal Type Declarations**: Compact type representations

#### Type Inference Algorithm
Uses simplified Hindley-Milner style inference optimized for:
- Function argument types
- Return type deduction
- Expression result types
- Array element types

#### Semantic Analysis Output
```lullaby
ir = semantic_analyzer.analyze(source_code)
type_errors = checker.validate(ir)
```

The analyzer generates a simplified intermediate representation with:
- Type annotations attached to expressions
- Function signatures with parameter/return types
- Control flow graph without verbose type information
- Memory allocation metadata

#### Error Detection
Catches type-related errors before code generation:
- Type mismatches in operations
- Invalid function calls (wrong argument count/types)
- Uninitialized variable usage
- Incompatible pointer accesses

### Stage 3: Intermediate Representation Generation

Converts semantic analysis output into optimization-friendly IR format.

#### IR Structure
```lullaby
ir = [
  declarations: [...],     // Variable and function declarations with types
  functions: [...],        // Function definitions as control flow graphs
  imports: [...],          // External module references
  resources: [...]         // Memory, file, device allocations
]
```

#### IR Optimizations Built-in
- **Implemented now**: Opt-in constant folding through the `lullaby_ir` optimization framework and `lullaby run --backend ir|bytecode --optimize constant-fold`.
- **Implemented now**: Conservative block-local common subexpression elimination for repeated pure `let` initializer expressions in the current alpha optimizer pipeline.
- **Implemented now**: Conservative loop-invariant motion for safe loop-body `let` initializers whose dependencies are available before the loop and are not declared or mutated inside the loop.
- **Implemented now**: Conservative block-local copy propagation for simple variable aliases in the current alpha optimizer pipeline.
- **Implemented now**: Dead-code elimination for statements after explicit block terminators through `lullaby run --backend ir|bytecode --optimize dead-code`.
- **Implemented now**: The current alpha pass pipeline through `lullaby run --backend ir|bytecode --optimize alpha`, running constant folding, CSE, loop-invariant motion, copy propagation, and dead-code elimination.
- **Planned**: Broader dead branch and unreachable control-flow elimination.
- **Planned**: Type propagation to infer missing types through data flow analysis.
- **Planned**: Memory layout optimization for cache-friendly variable placement.

### Stage 4: Code Generation (Compiler)

Transforms IR into efficient machine code with systems-level optimizations.

#### Target Architecture Support
- Native x86_64, ARM64 instruction sets
- Custom Lullaby bytecode (optional)
- Direct hardware abstraction layer

#### Current Alpha Bytecode Artifact

The current compiler artifact is a JSON `.lbc` file with a format marker, artifact version, metadata, entry point, function table, memory operation metadata, and instruction-bytecode module:

- `format`: `lullaby-bytecode`
- `version`: artifact version, currently `4`
- `metadata`: deterministic producer, target, and payload metadata
- `entry`: currently `main`
- `function_table`: declared bytecode function signatures used for compatibility checks
- `memory_operations`: analyzed allocation, load, store, deallocation, and bounds-check metadata used for backend and native-codegen preparation, including a stable artifact-order sequence for cleanup/lowering order
- `module`: bytecode functions containing dedicated `instructions` rather than raw IR statements

`lullaby compile file.lby -o file.lbc` writes this artifact. `lullaby build file.lby -o file.lbc` is the same artifact-generation path under a build-oriented command name. `lullaby inspect file.lbc` prints artifact metadata, function signatures, and memory operation counts, while verbose/JSON inspect output includes each memory operation's sequence number. `lullaby run file.lbc` executes it through the bytecode VM entry point. Unsupported artifact format, version, target, payload, entry values, duplicate functions, function-table/module mismatches, or memory-operation/module mismatches produce `L0601 [bytecode error]`.

#### Optimization Passes
1. **Algebraic Simplification**
   - `x + 0` -> `x`
   - `x * 1` -> `x`
   - `a + a + b` -> `2*a + b`

2. **Control Flow Optimization**
   - Loop invariant code motion
   - Dead branch elimination
   - Branch prediction hints

3. **Memory Optimization**
   - Stack allocation for local variables
   - Heap allocation for dynamic structures
   - Register assignment optimization

4. **Instruction Selection**
   - Match IR operations to optimal machine instructions
   - Fuse multiple operations when beneficial
   - Use vector instructions where applicable

#### Code Generation Output
```lullaby
machine_code = codegen.generate(ir, target_arch)
optimization_report = codegen.get_stats()
```

### Stage 5: Symbol Resolution and Linking

Resolves cross-module references and produces final executable.

#### Symbol Table Management
- Local scope resolution (function-level variables)
- Module-level resolution (global variables, functions)
- External reference resolution (library imports)

#### Linking Strategy
1. **Static Compilation**: All dependencies included in final binary
2. **Dynamic Compilation**: Separate compilation units linked together
3. **Incremental Compilation**: Update only changed sections

### Stage 6: Binary Output and Verification

Produces the final executable and performs final validation.

#### Binary Format
- ELF (Linux), Mach-O (macOS), PE (Windows) formats supported
- Optimized section layout for performance
- Minimal binary size through advanced compression

#### Final Verification
- Symbol consistency check
- Memory safety verification
- Optimization correctness validation

## Compilation Performance Characteristics

### Token Efficiency Metrics
| Aspect | Traditional Languages | Lullaby | Improvement |
|--------|----------------------|-----------|-------------|
| Function definitions | ~15 tokens | ~4 tokens | 73% reduction |
| Variable declarations | ~2 tokens each | ~1 token each | 50% reduction |
| Control structures | ~6-8 tokens per block | ~2-3 tokens per block | 60% reduction |
| Expressions | Variable based on complexity | Fixed minimal format | 40-60% reduction |

### Compilation Speed Optimization
- Parallel semantic analysis for large codebases
- Incremental compilation for development workflows
- Caching of IR and intermediate results
- Specialized passes for common patterns

### Memory Efficiency
- Minimal runtime overhead (< 5% vs interpreted languages)
- Efficient memory representation in IR
- Optimized garbage collection (generation-based, incremental)

## LLM Integration Considerations

### Training Data Design
The Lullaby syntax is designed to be:
1. **Pattern-consistent**: Regular structure enables pattern learning
2. **Symbol-simple**: Few unique tokens needed for vocabulary
3. **Context-clear**: Explicit relationships reduce ambiguity
4. **Size-efficient**: Short sequences fit within model contexts

### LLM-Friendly Features
- Consistent indentation-based scoping (no brace nesting)
- Linear statement flow (reduced parsing complexity)
- Predictable token patterns (easier sequence modeling)
- Type annotations at declaration points (simplifies inference)

## Example Compilation Flow

```lullaby
// Source code
func main(): void
  if input_count > max_limit then
    !0x41 Error: Input too large
    limit_to_max()
  end

  for i from 0 to input_count do
    process(input[i])
  end

  output_results()

// Compilation command
lullaby compile main.lby -o main.bin --optimize full --arch x86_64

// Compilation output
[INFO] Tokenizing source (52 tokens)...
[INFO] Semantic analysis complete (3 type errors detected, fixed)
[INFO] IR generation optimized (dead code removed: 12 statements)
[INFO] Code generation complete (x86_64 target)
[INFO] Binary output: main.bin (2.3 MB)
[SUCCESS] Compilation successful

// Execution
./main.bin < input_data.txt
```

## Conclusion

The Lullaby compilation architecture provides:
- **Minimal token usage** through compact syntax design (~60% reduction vs C++)
- **Compile-time optimization** shifting work away from runtime
- **LLM-friendly patterns** enabling smaller models to understand code
- **Systems programming efficiency** with direct hardware control
- **Type safety** through static analysis without runtime overhead

This architecture enables building highly efficient, LLM-compatible systems programming languages suitable for OS development while maintaining the minimalistic design philosophy central to Lullaby.
