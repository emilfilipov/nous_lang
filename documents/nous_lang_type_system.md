# Nous Lang (nlang) - Type System

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

## Current Alpha Type Checker

The Rust alpha currently validates `i64`, `bool`, `string`, `void`, interim pointer types such as `ptr_i64`, and homogeneous arrays spelled `array<T>`.

Implemented local binding inference:
- Explicit local annotations use `let name Type = expression`.
- Omitted local annotations use `let name = expression` and infer the binding type from the initializer.
- Literal, function-call, array-literal, index, unary, binary, and builtin-call expressions can provide inferred binding types when semantic validation succeeds.
- Empty arrays and `void` initializers cannot provide a usable inferred local type in the current alpha.

Implemented array rules:
- Array literals use bracket syntax, for example `[1, 2, 3]`.
- Array literals must be non-empty in the current alpha because empty-array inference is not implemented yet.
- All array literal values must have the same static type.
- Index expressions use `values[index]`.
- Index expressions require an `array<T>` target and an `i64` index.
- Runtime execution bounds-checks array indexes.
- Equality comparisons require matching operand types; ordering comparisons currently require `i64` operands.

## Overview
Nous Lang employs a **hybrid typing system** combining static type safety with automatic type inference, optimized for tiny LLM comprehension while maintaining strong guarantees for systems programming in OS development.

## Typing Philosophy

### Core Principles
1. **Explicit is Better Than Implicit**: Types are declared where needed, inferred only when unambiguous
2. **Monomorphic Design**: Strong typing without complex type hierarchies (simplifies tiny model inference)
3. **Structural Coercion**: Type compatibility checked at assignment points, not declaration sites
4. **Minimal Metadata**: Type information embedded directly in code where it aids compilation

### Type System Goals
- Compile-time safety: Catch errors before runtime execution
- LLM-friendliness: Clear type signatures enable tiny model (<1B params) understanding
- Performance: Minimal overhead through explicit declarations and inference
- Systems suitability: Support low-level memory operations with precise type knowledge

## Type Categories

### Primitive Types
Core scalar types inferred automatically or explicitly declared:

```nlang
// Numeric primitives (signed/unsigned variants):
int8, int16, int32, int64       // signed integers
uint8, uint16, uint32, uint64   // unsigned integers
float32, float64                // IEEE 754 floating point

// Other primitives:
bool                            // boolean (true/false)
char                            // single character literal
string                          // byte sequence

// Example usage:
let x: int32 = 42               // explicit type declaration
var pi: float64 = 3.14159       // explicit for precision control
let text: string = "hello"      // inferred from literal
```

### Composite Types

**Structs (Record Layout):**
Fixed-size collections with named fields, stored contiguously in memory:

```nlang
// Struct definition
struct Point
    x: float64
    y: float64
    label: string

// Struct instance (field access without brackets)
let p: struct Point
    x: 3.14, y: 2.71, label: "origin"

// Field access
p.x          // dot notation (no brackets)
p.y

// Type-aware field access
point.x       // type checking ensures 'x' exists in Point struct
```

**Unions:**
Disjoint alternative values with explicit variant tags:

```nlang
union Status
    success: int32,
    error_code: uint16,
    timeout: bool

var state: Status = union
    variant: error_code = 404

// Variant access (explicit naming)
state.success   // returns None if not success variant
state.error_code    // returns value if error_code is active
```

**Arrays:**
Fixed-size homogeneous collections:

```nlang
// Array type declaration
array<int32>              // array of 32-bit integers

// Array instantiation
let nums = array(5) init [1, 2, 3, 4, 5]

// Indexed access (bracket notation retained for arrays only)
nums[0]                  // first element
nums[i where i < len(nums)]   // bounds-checked access
```

**Strings:**
Byte sequences with optional encoding metadata:

```nlang
type Text = array<uint8>  # byte-oriented strings

let msg: Text = "Hello" as utf8
```

### Reference Types

**Typed Pointers:**
Explicitly typed memory references with allocation tracking:

```nlang
// Pointer declaration and typing
ptr<int32>*              // pointer to int32
ref<array<uint8>>        // reference (alias) to byte array

// Allocation with type inference
let num_ptr = alloc(int32*, 42)      # value stored at address
var buf_ref: ref<Text> = alloc(Text, capacity=1024)

// Access through pointers/references
num_ptr.load()         // dereference and access
buf_ref.data[i]        # bracket notation for array contents
```

**Smart References:**
Reference counting wrappers (for heap objects):

```nlang
rc<array<uint8>> create_buffer(size)   # reference-counted pointer
shared<Point> clone(p)                 # increments refcount
release(rc_obj)                        # decrements refcount, frees if 0

// Usage patterns:
var data = rc_create(buffer_ptr)      # explicit type + allocation
use data.data                         # dereference for access
dec_ref(data)                         # manual cleanup when needed
```

### Function Types

Functions treated as first-class types with explicit signatures:

```nlang
# Basic function type signature

fn<int32, int32> -> int32             # takes two ints, returns int
fn(string) -> bool                   # string input, boolean output

# With named parameters

fn ProcessImage(
    image: ref<Text>,
    quality: float64 = 0.8,
    output_format: string = "png"
) -> bool

// Type-checked call
fn_call(ProcessImage, img_ref, 0.95, png_fmt): bool result
```

## Type Inference Rules

### Declaration-Based Inference
Types inferred from explicit declarations and initial values:

```nlang
let x = 42                    # inferred as int32 (literal type)
var y: int64 = 1000           # declared type overrides inference
z = "hello"                  # inferred as string (text literal)
w: array<uint8> = [1, 2, 3]   # type from both declaration and values

// Type consistency check:
let mismatched: int32 = "text"   # ERROR: type incompatibility
```

### Literal-Based Inference
Types inferred from literals when no declaration present:

```nlang
auto v1 = 42          # inferred as integer (context determines precision)
auto v2 = 3.14        # inferred as float64 (decimal point indicates float)
auto s = "text"       # inferred as string
auto b = true         # inferred as bool

// Type coercion (with explicit conversion):
large_num: int64 = small_val   # automatic promotion
small_int = large_num          # implicit truncation with warning flag
```

### Expression-Based Inference
Types inferred from computed values:

```nlang
let result = a + b           # inferred as common supertype of a and b
sum_val = sum(numbers)       # returns type of elements (or their supertype)
computed = process(x)        # inferred from function return type annotation
```

## Type System Features

### 1. Structural Coercion
Types compatible if they have matching structure, even without identical names:

```nlang
struct A  x: int32; y: string
struct B
    x: int32
    label: string   # renamed field (still structurally compatible)

// These are compatible for assignment:
let a: A = create_struct_B()  # coercion applied automatically
```

### 2. Type Bounds Checking
Compile-time validation of array and pointer operations:

```nlang
safe_access arr[i]    # compile error if i's type doesn't match index range
bounds_checked(safe_read(arr, i))

unchecked
    unsafe_arr[raw_index]  # runtime check disabled (programmer risk)
```

### 3. Type Aliasing
Type synonyms without memory overhead:

```nlang
type struct pixel r: uint8, g: uint8, b: uint8
# 'pixel' becomes alias for the full Point RGB structure


use_pixel(p)          # treated as use_Point() semantically
```

### 4. Type Parameters (Generics-Simple)
Parameterized types with explicit constraints:

```nlang
list<T> of [e1, e2] where T is int32 or float64
map<K,V> of "key": val where K is string, V is number type

// Constrained usage:
items: list<int32> = create_list(10, 20, 30)
mixed: list<string> = ["a", "b"]   # ERROR: T must be int32 or float64
```

## Type Safety Mechanisms

### Compile-Time Checks
Errors detected before runtime execution:

```nlang
// Type mismatch error
let x: int32 = "text"
# ERROR: Cannot assign string to integer type


// Uninitialized reference error
var ptr: ptr<int32>*
use ptr.load()
# ERROR: ptr not allocated (null pointer)


// Struct field access error
struct Point  x: float64; y: float64
p.invalid_field = 1.0
# ERROR: Field 'invalid_field' not in struct 'Point'


// Array bounds violation
arr: array(5)
unsafe_read(arr, 10)
# ERROR: Index 10 >= array length 5 (in unchecked mode only)

```

### Runtime Safety
Checks performed during execution for edge cases:

```nlang
// Optional type handling
var opt_ref = try_alloc()
if opt_ref.is_valid then
    process(opt_ref.data)
else
    handle_null_pointer()
end_if

// Result-based error propagation
fn_divide(a: float64, b: float64) -> result: Option<float64>
let quot = divide(10.0, 2.0)
if quot.is_some then
    use quot.unwrap()
else
    handle_error("division by zero")
end_if
```

## Type System for OS Development

### Memory Layout Types
Types optimized for kernel data structures:

```nlang
// Process descriptor structure
struct process
    pid: int32
    memory_start: ptr<void>*
    memory_end: ptr<void>*
    refcount: uint16  # reference counting for process objects
    state: enum<process_state>

// Page table entry (kernel virtual memory)
struct pte
    address: ptr<void>*
    permissions: uint8  # read/write/execute bits
    present: bool

// File system inode
struct inode
    file_id: int32
    refcount: uint16
    data_offset: int64
    type_tag: enum<file_type>
```

### Kernel Type Optimizations
Special types for performance-critical systems code:

```nlang
// Zero-copy buffers (avoid memcpy)
struct zero_copy_buffer
    ptr: ptr<void>*
    length: uint32
    offset: int64  # memory region within larger buffer

// Ring buffers for inter-process communication
struct ring_buffer<T>
    head: uint16, tail: uint16
    data: array<T, capacity>,
    read_count: uint16, write_count: uint16
```

## Type System Comparison to Existing Languages

| Feature | Nous Lang | Rust | C++ | Go/Java |
|---------|-----------|------|-----|---------|
| **Type declaration** | Explicit `type X = ...` or inferred | Explicit with generics | Templates (compile-time) | Interfaces + explicit |
| **Struct field access** | Dot notation `.field` | Dot notation `.field` | Dot/bracket syntax | Dot notation .Field |
| **Array access** | `arr[i]` (with bounds check) | `arr[i]` with checks | `arr[i]` optional checks | `arr[i]` unchecked |
| **Pointers** | Explicit typed ptr/references | Smart pointers Box/Rc/Arc | Raw ptr*, references & | Pointers only * |
| **Functions as types** | Yes: `fn<int32> -> int64` | Yes (traits) | Yes with templates | No |
| **Type inference** | Hybrid (explicit or auto) | Extensive inference | Limited inference | Type inference limited |

---
**Document Purpose:** Define the complete type system for Nous Lang, covering all type categories, inference rules, safety mechanisms, and OS-specific adaptations. This complements the syntax and memory documentation to provide a comprehensive understanding of how types work in the language.
