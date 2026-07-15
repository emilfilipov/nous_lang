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

**Structs (record layout):** nominal types with named fields declared `name Type`
(space-separated, no colon). Construction is positional or named; fields are read
and mutated with dot notation.

```lullaby
struct Point
    x f64
    y f64
    label string

let p Point = Point(3.14, 2.71, "origin")   # positional
let q = Point(x: 1.0, y: 2.0, label: "q")   # named; type inferred
p.x                                         # field read
p.x = 5.0                                   # field mutation
```

**Enums (tagged unions):** disjoint variants, optionally carrying payloads,
consumed by exhaustive `match`. (There is no `union` keyword — `union` is reserved
and rejected with `L0211`.)

```lullaby
enum Status
    active
    failed i64
    timeout

let s = failed(404)           # bare variant construction
match s
    active -> 0
    failed(code) -> code      # payload bound with parentheses
    timeout -> -1
```

**Built-in generic enums:** `option<T>` (`some(v)`/`none`) and `result<T, E>`
(`ok(v)`/`err(e)`), with the postfix `?` propagation operator.

**Collections:** fixed `array<T>` (`[1, 2, 3]` literals, or `array_fill(n, v)` for a
sized buffer), growable `list<T>`, and `map<K, V>`. Indexing is `xs[i]` with a
runtime bounds check.

```lullaby
let nums array<i64> = [1, 2, 3, 4, 5]
let buf array<i64> = array_fill(64, 0)     # 64-element zeroed buffer
nums[0]                                     # first element (bounds-checked)
```

**Strings:** `string` is UTF-8 text (not a byte array). Decompose to bytes with
`to_bytes`/`from_bytes` or to characters with `chars`/`string_from_chars`.

### Reference Types

Explicit reference and pointer types for heap values and low-level memory.

```lullaby
rc<i64>       # reference-counted shared pointer (rc_new/rc_clone/rc_release/rc_get)
ref<i64>      # a borrow/alias to a value (ref_get)
ptr<i64>      # a raw pointer (unsafe read/write)

let cell rc<i64> = rc_new(42)
let value = rc_get(cell)          # read through the rc
rc_release(cell)                  # drop a reference
```

Heap slots use the `alloc`/`load`/`store`/`dealloc` builtins; raw pointer
dereferencing is gated behind `unsafe`.

### Function Types

Functions are first-class values. A function type is spelled `fn(T1, T2) -> R`
(zero or more parameter types, an arrow, a return type). A bare top-level function
name is a value of its type, so functions can be stored, passed, and returned.

```lullaby
fn(i64, i64) -> i64        # two i64 in, one i64 out
fn(string) -> bool
fn() -> void

fn add a, b i64 -> i64
    a + b

let op fn(i64, i64) -> i64 = add    # name-as-value
op(2, 3)                            # call through the local -> 5
```

Parameters are always explicitly typed (no defaults or named-argument syntax at
the call site). Capturing closures / lambda literals are on the roadmap.

## Type Inference

Inference is local and initializer-directed — it never crosses function
boundaries (parameter types are always explicit; return types infer from the body).

```lullaby
let x = 42            # i64 (integer literal default)
let y i32 = 1000i32   # explicit annotation + typed suffix
let pi = 3.14         # f64 (a decimal literal is f64)
let s = "text"        # string
let b = true          # bool
let sum = a + b       # the operand type (both operands must already agree)
```

A binding whose annotation and initializer disagree is a compile error
(`L0303`); an empty array or a `void` initializer cannot provide an inferred type.
There is **no implicit numeric coercion** — mixing widths or int/float in an
operator is rejected (`L0307`); convert explicitly with `to_<T>`/`to_i64`/`to_f32`/
`to_f64`.

## Type Safety

- **Nominal typing.** Two structs with identical fields but different names are
  **not** interchangeable — there is no structural coercion. Assignment and
  argument passing require matching declared types.
- **Compile-time checks.** Type mismatches (`L0303`/`L0313`), unknown struct fields,
  non-exhaustive `match`, and operand-type errors (`L0307`) are caught before
  execution. Diagnostics carry stable `L####` codes (see
  [diagnostic_registry.md](diagnostic_registry.md)).
- **Runtime checks.** Array/list indexing is bounds-checked at runtime; absence and
  failure are modeled in the type system with `option<T>`/`result<T, E>` and the
  `?` operator rather than null.

```lullaby
fn safe_div n, d i64 -> result<i64, string>
    if d == 0
        return err("division by zero")
    ok(n / d)

let q = safe_div(10, 2)      # result<i64, string>
match q
    ok v -> v
    err message -> -1
```

## Generics and Bounds

Generic *functions* declare type parameters in angle brackets after the name and
use each as an ordinary type; type arguments are inferred at the call site.
Constraints are trait **bounds** (`<T: Trait>` / `<T: A + B>`), not `where`-style
value predicates.

```lullaby
fn identity<T> x T -> T
    x

identity(41)        # T = i64
identity("hi")      # T = string

trait Show
    fn show self -> string

# Inside a bounded generic, a value of type T may call the bound trait's methods:
fn describe<T: Show> value T -> string
    value.show()
```

A bare `T` value supports only universal operations — binding, passing, returning,
`==`/`!=` between two same-`T` values, and use as a payload to a built-in generic
(`some(x)`). Arithmetic and ordering (`+`, `<`, …) on a bare `T` are rejected
(`L0327`) until a trait provides them; a `<T: Trait>` bound lets the body call that
trait's methods. Generic *user types* (parameterized `struct`/`enum` you declare),
trait objects (`dyn`), and associated types are on the roadmap; the built-in
generics `option`, `result`, `list`, `map`, `array`, `rc`, `ref`, and `ptr` are
available today.

## Type Aliases

`alias Name = Type` introduces a synonym with no runtime cost.

```lullaby
alias Bytes = array<i64>
let data Bytes = [1, 2, 3]
```

## Named Compile-Time Constants

A top-level `const NAME type = <expr>` declaration binds a name to a value that is
computed **at compile time**. Unlike an inferred `let`, the type annotation is
mandatory. A constant may be exported with `pub const` and referenced from any
function; a local binding, parameter, loop variable, `match` binding, or closure
parameter of the same name shadows it as usual.

```lullaby
const MAX_LEN i64 = 128
const GREETING string = "hi"
const DOUBLED i64 = MAX_LEN * 2   # a constant expression over another constant
```

The initializer must be a **constant expression**: a literal (`i64`, `f64`,
`bool`, `string`, `char`), or an arithmetic/logical/bitwise/comparison/unary
operator applied to literals and other already-defined constants. String `+`
concatenation of constant strings/chars is included. Anything that reads a runtime
value — a call to a non-`const` function, an array/index/field/struct/enum/match
value, a closure — is **not** a constant expression and is rejected (`L0450`).
Integer arithmetic wraps like the rest of the language, but a constant division or
remainder by zero is a compile error (`L0450`).

Each constant is type-checked against its declared type (`L0451`; there is no
implicit `i64`→`f64` widening, so an `f64` constant needs a float literal such as
`3.0`). Cyclic references (`A = B + 1`, `B = A + 1`) are rejected (`L0452`), as are
duplicate constant names or a name that collides with another top-level
declaration (`L0453`).

Constants are a **frontend-only** feature: semantic analysis evaluates every
constant once and folds each reference into its literal value before the type
checker validates function bodies. Every backend (the AST/IR/bytecode
interpreters, the native x86-64/AArch64 backends, and WASM) therefore only ever
sees ordinary literals and needs no `const` awareness — a folded `const` in an
otherwise all-`i64` function stays native-eligible. Const-sized arrays
(`array<T, N>` where `N` is a constant) are a planned follow-up and are not part of
this increment.

## Types for Systems / OS Development

The scalar and struct types map directly onto kernel data structures — fixed-width
integers for hardware registers and bit fields, `struct` for records laid out in
memory, and the raw-memory builtins (`alloc`/`load`/`store`, `sizeof`/`alignof`/
`offsetof`, `unsafe` pointer access) for direct manipulation.

```lullaby
struct Pte
    address i64        # physical page address
    permissions u8     # read/write/execute bits
    present bool

struct Inode
    file_id i32
    refcount u16
    data_offset i64
```

---
**Document Purpose:** Define the complete type system for Lullaby, covering all type categories, inference rules, safety mechanisms, and OS-specific adaptations. This complements the syntax and memory documentation to provide a comprehensive understanding of how types work in the language.
