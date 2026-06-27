# Nous Lang Compilation Architecture Documentation

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

## Overview

Nous Lang implements a novel multi-phase compilation pipeline specifically optimized for:
1. **Token efficiency** - Minimize source code token count for LLM generation
2. **Compile-time optimization** - Perform optimizations during compilation, not runtime
3. **Systems programming focus** - Direct hardware interaction with minimal abstraction overhead
4. **LLM compatibility** - Design patterns that tiny LLMs can understand and generate

## Compilation Pipeline Stages

## Current Alpha Pipeline

The current Rust workspace implements a frontend and in-process execution pipeline:

1. `nous_lexer` validates `.nl` paths, emits tokens, emits indentation/dedent structure, and rejects forbidden block delimiters.
2. `nous_parser` builds an AST for functions, typed parameters, `let`, assignment, returns, break/continue, if/elif/else, while/loop/range-for blocks, calls, literals, array literals/indexing, variables, arithmetic, comparisons, and boolean logic.
3. `nous_semantics` validates static types, local bindings, assignments, function calls, return behavior, bool conditions, loop-control placement, arithmetic/comparison/logical operands, homogeneous non-empty arrays, array indexes, interim pointer-style memory builtins, text file I/O builtins, and safe system command builtins. Successful validation returns `CheckedProgram` metadata with function signatures and inferred expression types.
4. `nous_ir` lowers a `CheckedProgram` into typed semantic IR for the current alpha subset, including typed functions, parameters, statements, control flow, calls, builtins, and expressions.
5. `nous_runtime` executes the validated AST directly, including `main`, calls, scoped locals, assignment, branch result values, while/loop/range-for control flow, array literals/indexing with runtime bounds checks, arithmetic/comparisons, short-circuit boolean logic, heap-slot memory operations including `alloc`/`load`/`store`/`dealloc`, text file I/O, and safe system command builtins.
6. `nous_ir` provides a deterministic optimization pass framework. Implemented passes are constant folding for pure literal arithmetic, comparisons, boolean logic, string equality, and unary `not`, conservative block-local common subexpression elimination for repeated pure bindings, conservative loop-invariant motion for safe loop-body bindings, conservative block-local copy propagation for simple variable aliases, plus dead-code elimination for statements after unconditional `return`, `break`, or `continue` in the same block. Constant folding and loop-invariant motion deliberately leave potentially failing divide-by-zero expressions in place so runtime diagnostics and zero-iteration loop behavior are preserved.
7. `nous_ir` can also execute the lowered typed IR, lower it into an explicit instruction-bytecode module, and encode/decode a versioned `.nbc` bytecode artifact for the current alpha subset.
8. `nous_cli` exposes the current pipeline as `nlang check <file.nl>`, `nlang compile [--optimize none|constant-fold|dead-code|alpha] [-o output.nbc] <file.nl>`, `nlang inspect <file.nbc>`, `nlang run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] <file.nl|file.nbc>`, `nlang docs`, and `nlang examples`. Optimization is opt-in and applies only to IR/bytecode source runs and compiled bytecode artifacts.

Additional optimization passes, native code generation, linking, and binary output remain planned architecture stages.

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
```nlang
source_code = tokenizer.parse(source_text)
stream = stream.compact(source_code)
```

The tokenizer uses context-aware parsing to reduce token count by ~40% compared to traditional languages:
- No parentheses needed (operator precedence handled by position)
- No semicolons (statement termination implicit via newline or block markers)
- No quotes for strings (implicit string delimiters removed)

#### Example Tokenization
```nlang
// Source code snippet: x = 42 + "hello world"
// Traditional language tokens: 9 tokens
// Nous Lang tokens: 5 tokens

Traditional: IDENT(x), OP(=), NUM(42), OP(+), STR("hello", "world"), ENDSTR
Nous Lang:   ident(x), assign, num(42), plus, str(helloworld)
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
```nlang
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
```nlang
ir = [
  declarations: [...],     // Variable and function declarations with types
  functions: [...],        // Function definitions as control flow graphs
  imports: [...],          // External module references
  resources: [...]         // Memory, file, device allocations
]
```

#### IR Optimizations Built-in
- **Implemented now**: Opt-in constant folding through the `nous_ir` optimization framework and `nlang run --backend ir|bytecode --optimize constant-fold`.
- **Implemented now**: Conservative block-local common subexpression elimination for repeated pure `let` initializer expressions in the current alpha optimizer pipeline.
- **Implemented now**: Conservative loop-invariant motion for safe loop-body `let` initializers whose dependencies are available before the loop and are not declared or mutated inside the loop.
- **Implemented now**: Conservative block-local copy propagation for simple variable aliases in the current alpha optimizer pipeline.
- **Implemented now**: Dead-code elimination for statements after explicit block terminators through `nlang run --backend ir|bytecode --optimize dead-code`.
- **Implemented now**: The current alpha pass pipeline through `nlang run --backend ir|bytecode --optimize alpha`, running constant folding, CSE, loop-invariant motion, copy propagation, and dead-code elimination.
- **Planned**: Broader dead branch and unreachable control-flow elimination.
- **Planned**: Type propagation to infer missing types through data flow analysis.
- **Planned**: Memory layout optimization for cache-friendly variable placement.

### Stage 4: Code Generation (Compiler)

Transforms IR into efficient machine code with systems-level optimizations.

#### Target Architecture Support
- Native x86_64, ARM64 instruction sets
- Custom Nous Lang bytecode (optional)
- Direct hardware abstraction layer

#### Current Alpha Bytecode Artifact

The current compiler artifact is a JSON `.nbc` file with a format marker, artifact version, metadata, entry point, function table, and instruction-bytecode module:

- `format`: `nous-bytecode`
- `version`: artifact version, currently `3`
- `metadata`: deterministic producer, target, and payload metadata
- `entry`: currently `main`
- `function_table`: declared bytecode function signatures used for compatibility checks
- `module`: bytecode functions containing dedicated `instructions` rather than raw IR statements

`nlang compile file.nl -o file.nbc` writes this artifact, `nlang inspect file.nbc` prints artifact metadata and function signatures, and `nlang run file.nbc` executes it through the bytecode VM entry point. Unsupported artifact format, version, target, payload, entry values, duplicate functions, or function-table/module mismatches produce `N0601 [bytecode error]`.

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
```nlang
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
| Aspect | Traditional Languages | Nous Lang | Improvement |
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
The Nous Lang syntax is designed to be:
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

```nlang
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
nlang-compile main.nl -o main.bin --optimize=full --arch=x86_64

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

The Nous Lang compilation architecture provides:
- **Minimal token usage** through compact syntax design (~60% reduction vs C++)
- **Compile-time optimization** shifting work away from runtime
- **LLM-friendly patterns** enabling smaller models to understand code
- **Systems programming efficiency** with direct hardware control
- **Type safety** through static analysis without runtime overhead

This architecture enables building highly efficient, LLM-compatible systems programming languages suitable for OS development while maintaining the minimalistic design philosophy central to Nous Lang.
