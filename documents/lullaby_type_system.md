# Lullaby (lullaby) - Type System

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

## Current Type Checker

> **Authoritative surface:** the concise, current list of implemented types lives
> in [language_surface.md](language_surface.md) and the "Currently implemented"
> section of [language_specification.md](language_specification.md). The design
> material further below uses illustrative names for some constructs; where it
> differs, those two documents and the canonical spellings here win.

The compiler validates, with real (space-separated, colon-free) syntax:

- **Scalars:** `i64`; the fixed-width integers `i8`/`i16`/`i32`/`u8`/`u16`/`u32`/`u64`
  and `isize`/`usize`; `f64` and `f32`; `bool`, `char`, `byte`, `string`, and
  `void`. Fixed-width integers and `f32` take typed literal suffixes (`5i32`,
  `1.5f32`) and convert via `to_<T>`/`to_i64`/`to_f32`/`to_f64`.
- **Composites:** nominal `struct` (fields declared `name Type`, positional and
  named construction, `.field` access/mutation, UFCS methods); `enum` tagged
  unions with exhaustive `match` (there is **no** `union` keyword — `union` is
  reserved and rejected with `L0211`); the built-in generic enums `option<T>` and
  `result<T, E>`; fixed `array<T>`, growable `list<T>`, and `map<K, V>`.
- **Reference/function:** `rc<T>`/`ref<T>`/`ptr<T>` and function values `fn(T) -> R`.
- **Generics and traits:** generic functions `fn name<T> ...` with call-site
  inference and trait bounds `<T: Trait>`; `trait`/`impl` with receiver-type dispatch.

Local binding inference:
- Explicit local annotations use `let name Type = expression` (no colon).
- Omitted annotations use `let name = expression` and infer from the initializer;
  an empty array or a `void` initializer cannot provide an inferred type.

Array rules:
- Array literals use bracket syntax `[1, 2, 3]` and must be non-empty (empty-array
  inference is not implemented); all elements share one static type.
- Index expressions `values[index]` require an `array<T>`/`list<T>` target and an
  `i64` index, and are bounds-checked at runtime.

## Overview
Lullaby employs a **hybrid typing system** combining static type safety with automatic type inference, optimized for tiny LLM comprehension while maintaining strong guarantees for systems programming in OS development.

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

```lullaby
# Numeric primitives (signed/unsigned variants):
i8  i16  i32  i64   isize       # signed integers (isize is pointer-width)
u8  u16  u32  u64   usize       # unsigned integers (usize is pointer-width)
f32  f64                        # IEEE 754 floating point

# Other primitives:
bool                            # boolean (true / false)
char                            # single Unicode scalar
byte                            # a raw 8-bit byte
string                          # UTF-8 text

# Example usage (annotations are space-separated, no colon; no `var` keyword):
let x i32 = 42i32               # explicit type + typed literal suffix
let pi f64 = 3.14159            # f64 literal
let text = "hello"              # inferred from the literal
```

### Composite Types

**Structs (Record Layout):**
Fixed-size collections with named fields, stored contiguously in memory:

```lullaby
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

```lullaby
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

```lullaby
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

```lullaby
type Text = array<uint8>  # byte-oriented strings

let msg: Text = "Hello" as utf8
```

### Reference Types

**Typed Pointers:**
Explicitly typed memory references with allocation tracking:

```lullaby
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

```lullaby
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

```lullaby
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

```lullaby
let x = 42                    # inferred as int32 (literal type)
var y: int64 = 1000           # declared type overrides inference
z = "hello"                  # inferred as string (text literal)
w: array<uint8> = [1, 2, 3]   # type from both declaration and values

// Type consistency check:
let mismatched: int32 = "text"   # ERROR: type incompatibility
```

### Literal-Based Inference
Types inferred from literals when no declaration present:

```lullaby
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

```lullaby
let result = a + b           # inferred as common supertype of a and b
sum_val = sum(numbers)       # returns type of elements (or their supertype)
computed = process(x)        # inferred from function return type annotation
```

## Type System Features

### 1. Structural Coercion
Types compatible if they have matching structure, even without identical names:

```lullaby
struct A  x: int32; y: string
struct B
    x: int32
    label: string   # renamed field (still structurally compatible)

// These are compatible for assignment:
let a: A = create_struct_B()  # coercion applied automatically
```

### 2. Type Bounds Checking
Compile-time validation of array and pointer operations:

```lullaby
safe_access arr[i]    # compile error if i's type doesn't match index range
bounds_checked(safe_read(arr, i))

unchecked
    unsafe_arr[raw_index]  # runtime check disabled (programmer risk)
```

### 3. Type Aliasing
Type synonyms without memory overhead:

```lullaby
type struct pixel r: uint8, g: uint8, b: uint8
# 'pixel' becomes alias for the full Point RGB structure


use_pixel(p)          # treated as use_Point() semantically
```

### 4. Type Parameters (Generics-Simple)
Parameterized types with explicit constraints:

```lullaby
list<T> of [e1, e2] where T is int32 or float64
map<K,V> of "key": val where K is string, V is number type

// Constrained usage:
items: list<int32> = create_list(10, 20, 30)
mixed: list<string> = ["a", "b"]   # ERROR: T must be int32 or float64
```

## Type Safety Mechanisms

### Compile-Time Checks
Errors detected before runtime execution:

```lullaby
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

```lullaby
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

```lullaby
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

```lullaby
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

| Feature | Lullaby | Rust | C++ | Go/Java |
|---------|-----------|------|-----|---------|
| **Type declaration** | Explicit `type X = ...` or inferred | Explicit with generics | Templates (compile-time) | Interfaces + explicit |
| **Struct field access** | Dot notation `.field` | Dot notation `.field` | Dot/bracket syntax | Dot notation .Field |
| **Array access** | `arr[i]` (with bounds check) | `arr[i]` with checks | `arr[i]` optional checks | `arr[i]` unchecked |
| **Pointers** | Explicit typed ptr/references | Smart pointers Box/Rc/Arc | Raw ptr*, references & | Pointers only * |
| **Functions as types** | Yes: `fn<int32> -> int64` | Yes (traits) | Yes with templates | No |
| **Type inference** | Hybrid (explicit or auto) | Extensive inference | Limited inference | Type inference limited |

---
**Document Purpose:** Define the complete type system for Lullaby, covering all type categories, inference rules, safety mechanisms, and OS-specific adaptations. This complements the syntax and memory documentation to provide a comprehensive understanding of how types work in the language.
