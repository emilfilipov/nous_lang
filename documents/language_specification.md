# Nous Lang (nlang) - Complete Language Specification

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

## Overview

Nous Lang is a next-generation compiled systems programming language designed with three fundamental goals:

1. **Minimalistic Syntax**: No squiggly brackets, semicolons, or other noise. Clean, readable syntax optimized for human understanding.
2. **Token Efficiency**: Minimal token expenditure during generation by LLMs, enabling smaller models to generate correct code.
3. **LLM-Friendly Design**: Simple enough that even tiny language models (<1B parameters) can understand and write in the language without issues.

**Target Use Case**: Systems programming for operating system development and other low-level applications requiring type safety, performance optimization, and memory efficiency.

## Current Alpha Implementation

The current Rust toolchain implements a small executable subset while the wider systems-language design remains in progress:

- Source files use the `.nl` extension.
- Blocks are indentation-only. Curly braces and semicolon terminators are compile errors.
- Functions use `fn name param Type -> ReturnType` and return the last reachable expression unless an explicit `return` exits earlier.
- Void functions use `-> void` and may use bare `return`.
- Local bindings use `let name Type = expression`.
- Existing local bindings can be updated with `name = expression` or numeric compound assignments `+=`, `-=`, `*=`, and `/=`.
- Implemented scalar types are `i64`, `bool`, `string`, and `void`.
- The current pointer spelling is an interim concrete type name such as `ptr_i64`.
- Implemented expressions include literals, variables, function calls with parentheses, arithmetic, comparisons, and grouped expressions.
- Implemented control flow is `if`/`elif`/`else`, `while`, `loop`, `break`, and `continue` with indentation-only bodies.
- Implemented memory builtins are `alloc(value)`, `load(ptr)`, and `dealloc(ptr)`.
- CLI commands are `nlang check <file.nl>` and `nlang run <file.nl>` through the Rust workspace.

## Language Philosophy

Nous Lang rejects traditional design patterns that prioritize compiler convenience over code clarity:

- **No Braces**: Control blocks defined through indentation only (Python-inspired but simpler)
- **No Semicolons**: Line-based statements without terminator requirements
- **Flat Structure**: Single-level control flow instead of deep nesting
- **Type Inference**: Automatic type detection reducing explicit annotations
- **Reference Counting**: Automatic memory management without garbage collection pauses

## Core Language Components

### 1. Syntax Design (See: `nous_lang_syntax_design.md`)

The syntax is intentionally minimal and predictable:

**Variables**:
```nlang
type name = value   // Type prefix required for clarity
name               // Type inferred from context
```

**Functions**:
```nlang
fn add x i64 y i64 -> i64
    x + y

fn log message string -> void
    io.println message
```

**Indentation-based Scoping**: Blocks defined by indentation levels only, no braces needed.

**Return Rule**: A non-void function returns the last reachable expression unless an explicit `return` exits earlier. A `void` function returns no value and may use bare `return` for early exit.

### 2. Memory Management (See: `nous_lang_memory_management.md`)

Reference-counted memory with automatic lifetime management:

- No garbage collection pauses
- Deterministic object cleanup
- Region-based organization
- Type-safe operations

**Key Types**:
- `num` - Unified integers and floats
- `str` - Strings
- `bool` - Boolean values
- `array<T>` - Homogeneous arrays
- `map<K,V>` - Key-value mappings
- `ptr<T>`, `ref<T>` - Pointers and references

### 3. Control Structures (See: `nous_lang_control_structures.md`)

Flat control flow without nesting complexity:

**Conditionals**:
```nlang
if condition
    then_statements
elif other_condition
    else_statements
else
    final_statements
```

**Loops**:
```nlang
for variable from start to end [by step]:
    loop_body

while condition:
    loop_body

loop:
    unconditionally_executed_statements

break  // Exit loop
continue  // Skip remaining statements in iteration
```

### 4. Type System (See: `nous_lang_type_system.md`)

Pragmatic type system for systems programming:

- **Zero-cost abstractions**: Types compile away at runtime
- **Union types**: Single type can hold multiple representations
- **Pattern matching**: Declarative value decomposition
- **Type inference**: Automatic type detection from usage

**Core Types**:
```nlang
type Integer is num  int, uint
type Float is num  float, double
type Bool is bool  false, true
type Text is str  string, char
```

### 5. I/O and Concurrency (See: `nous_lang_input_output.md`)

Efficient systems programming primitives:

**File Operations**:
- Read/write with automatic buffering
- Type-aware file operations
- Memory-mapped access for large files
- Stream-based processing

**Concurrency Patterns**:
- Simple thread/async models
- Automatic synchronization
- I/O multiplexing
- Message queues and shared memory

## Complete Syntax Reference

### Primitives
```nlang
# Boolean values

false, true

# Numeric literals

integer: 0, -123, +456
float: 3.14159, 2.71828

# String literals

"hello world"
'single quotes also supported'
"""multi-line strings"""

# None/null (no pointer dereferencing needed)

none  // Represents absence of value
```

### Operators

**Arithmetic**: `+ - * / % ^ //`
**Comparison**: `== != < > <= >= is_none is_defined`
**Logical**: `and or not xor`
**Assignment**: `+= -= *= /= ^= +=` (compound operators)
**Bitwise**: `& | ^ << >> inv(x)`
**Functional**: `map reduce fold select min max avg sum mode`

### Control Flow
```nlang
if condition:
    statements

switch value:
    case pattern1:
        code_block
    case pattern2 when additional_condition:
        alternative_code

end_switch
```

### Functions
```nlang
fn function_name param Type -> ReturnType
    # final expression is returned
    result_value

fn function_with_early_return value i64 -> i64
    if value < 0
        return 0

    value

fn side_effect message string -> void
    io.println message
```

### Structs and Objects
```nlang
struct Type
    field1: type1  // Explicit typing
    field2        // Inferred typing
    method(params): return_type:
        code

ptr_type = new Type()   // Pointer creation
ref_ref = ref(ptr_type) // Reference copy

type_instance = Type(field1: value1, field2: value2)  // Direct construction
```

### Collections
```nlang
array[10] = [v1, v2, ..., v10]
map[key] = [key_value_pairs]

len(collection)      // Length check
index(collection, i) // Element access
slice(arr, start, end)  // Range extraction
contains(coll, item)  // Membership test
sort(coll)           // Ordering
filter(coll, pred)   // Selective collection
```

### Memory Operations
```nlang
alloc(size/type)      // Allocate memory
dealloc(ptr)          // Free allocated memory
ref(ptr)             // Create reference copy
ptr(type)            // Create pointer
swap(a, b)           // Exchange values
duplicate(value)     // Deep copy
```

Current alpha memory form:
```nlang
fn main -> i64
    let ptr ptr_i64 = alloc(41)
    let value i64 = load(ptr)
    dealloc(ptr)
    value + 1
```

### I/O Operations
```nlang
io.read(path)         // Read entire file
io.readlines(path, max_lines=N)  // Read limited lines
io.open(path, mode)   // Open stream for reading/writing
io.write(path, data)  // Write to file

# Memory-mapped files

io.memory_map(path, size)
mm_data = mm_file.data_pointer
```

### Concurrency Primitives
```nlang
thread func = spawn_thread(func, args)
result = wait(thread)

mutex sync = create_mutex()
lock(sync)
    protected_code
end_lock

async def async_func(params):
    result = await operation()
    return result

tasks = []
for param in params:
    task = spawn_task(async_func, param)
    tasks.append(task)

results = await_all(tasks)
```

## Design Principles Summary

### 1. Minimalism
- Remove unnecessary syntax (no braces, no semicolons, no parentheses where not needed)
- Each line contains one clear operation
- Single keywords for common operations instead of verbose alternatives

### 2. Type Safety
- Compile-time type checking prevents errors before runtime
- Automatic inference reduces annotation burden
- Zero-cost abstractions maintain performance

### 3. Memory Efficiency
- Reference counting eliminates GC overhead
- Region-based organization improves cache locality
- Explicit lifetime management ensures determinism

### 4. LLM Optimization
- Predictable structure enables pattern-based generation
- Flat syntax reduces complexity for model understanding
- Limited keyword set (approx. 50 core keywords)

### 5. Systems Focus
- Designed specifically for systems programming needs
- Direct hardware abstraction without hidden layers
- Explicit memory and resource management

## Comparison with Existing Languages

| Feature | C/C++/Java | Python | Nous Lang |
|---------|------------|--------|-----------|
| Block syntax | Brace-delimited blocks | Indentation | **Indentation only** |
| Statement terminator | Semicolon `;` | None (new line) | **No terminators** |
| Memory model | Manual/GC | GC (pause risks) | **Reference counting** |
| Type system | Static/Static | Dynamic | **Static+Inference hybrid** |
| Control flow | Nested blocks | Indentation | **Flat indentation** |
| Keyword count | 50-80 | 30-40 | **~25 core keywords** |

## Implementation Roadmap

1. **Phase 1**: Core language specification and compiler design
   - Grammar definition
   - Type system specification
   - Compiler frontend (parsing, semantic analysis)

2. **Phase 2**: Runtime infrastructure development
   - Memory allocator implementation
   - Virtual machine design
   - JIT/AOT compilation strategies

3. **Phase 3**: Standard library creation
   - Basic I/O operations
   - Concurrency primitives
   - Data structure implementations

4. **Phase 4**: Systems programming toolkit
   - OS development templates
   - Hardware abstraction layer
   - Performance optimization guides

## Getting Started Examples

### Hello World
```nlang
fn main -> string
    "Hello, Nous Lang!"
```

### Simple Calculator
```nlang
fn add x i64 y i64 -> i64
    x + y

fn main -> i64
    let value i64 = add(40, 2)
    value
```

### Branching
```nlang
fn main -> i64
    if true
        42
    else
        0
```

### Loops
```nlang
fn main -> i64
    let value i64 = 0
    while value < 4
        value += 1
    value
```

```nlang
fn main -> i64
    let value i64 = 0
    loop
        value += 1
        if value < 5
            continue
        break
    value
```

## Future Extensions (Planned Features)

- **Generative AI Integration**: Built-in model training and inference APIs
- **WebAssembly Target**: Native WASM compilation for browser deployment
- **SQL Query Language**: Embedded database query syntax
- **DSL Support**: Domain-specific language embedding capabilities
- **Testing Framework**: Assertion-based testing primitives
- **Profiling Tools**: Performance analysis and optimization hints

## Conclusion

Nous Lang represents a fresh approach to systems programming, combining the performance and type safety of compiled languages with the simplicity and LLM-friendliness of modern design. By eliminating traditional sources of complexity (braces, semicolons, deep nesting) while maintaining rigorous type checking and memory safety, Nous Lang enables developers to write clear, efficient code that can be reliably generated by smaller language models.

This specification provides the foundation for both human developers building complex systems programs and AI models generating correct, optimized code. The minimalist design philosophy ensures that as LLM capabilities improve, Nous Lang will continue to benefit from more sophisticated generation while maintaining its core advantages of simplicity and efficiency.

---
**Language Version**: 1.0 (Alpha Specification)
**Design Goals Achieved**: Minimalism yes | Token Efficiency yes | LLM-Friendly yes | Type Safety yes | Uniqueness yes | Systems Programming yes
