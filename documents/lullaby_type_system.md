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
  inference and trait bounds `<T: Trait>`; `trait`/`impl` with receiver-type
  dispatch; and generic user structs `struct Box<T>` and enums `enum Opt<T>` with
  inference-directed construction, including **multiple type parameters**
  (`struct Pair<K, V>`, `enum Either<L, R>`) and **trait bounds on a type
  parameter** (`struct Sorted<T: Ord>`) enforced at each instantiation (see
  "Generic User Structs", "Generic User Enums", and "Multiple Type Parameters and
  Bounds").

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

### Integer overflow

**Integer arithmetic wraps by default.** The `+ - *` operators on every integer
type — `i64` and each fixed-width kind (`i8`/`i16`/`i32`/`u8`/`u16`/`u32`/`u64`/
`isize`/`usize`) — wrap modulo the type width on overflow (two's-complement),
deterministically and identically on every backend. This is a **conscious 1.0
decision**: wrapping is total (no hidden trap, no build-mode-dependent behaviour),
matches the machine, and keeps arithmetic branch-free. `/` likewise wraps the sole
signed-overflow case (`MIN / -1`), and shifts mask the amount to the operand width.

When wrapping is the wrong behaviour, handle overflow **explicitly** with the
overflow-aware stdlib builtins — `checked_add`/`checked_sub`/`checked_mul`/
`checked_div`/`checked_rem` (return `option<T>`, `none` on overflow or an undefined
division), `saturating_add`/`saturating_sub`/`saturating_mul` (clamp to `T`'s
bounds), and `wrapping_add`/`wrapping_sub`/`wrapping_mul` (the explicit form of the
default). See the standard library's [Overflow-aware integer
arithmetic](standard_library.md#overflow-aware-integer-arithmetic).

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
runtime bounds check. A fixed array may carry a compile-time extent —
`array<T, N>`, constructed with `[a, b, c]` or the fill literal `[value; N]` — see
[Const-sized arrays](#const-sized-arrays-arrayt-n).

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
trait's methods.

### Generic User Structs

A **generic `struct`** declares type parameters in angle brackets after its name,
exactly like a generic function, and uses each as an ordinary field type:

```lullaby
struct Box<T>
    value T

fn main -> i64
    let b Box<i64> = Box(5)      # T = i64 (from the annotation)
    let c = Box(true)            # T = bool (inferred from the argument)
    b.value                      # field read sees the concrete type: i64
```

A concrete instantiation is spelled `Name<Args>` (`Box<i64>`) wherever a type is
written — a binding annotation, a parameter, or a return type — and reading a
field of an instance substitutes the type arguments for the parameters, so
`b.value` on a `Box<i64>` has type `i64`. Type arguments are fixed **by inference,
not a turbofish call form**: the annotation on the binding/parameter/return pins
them, or the constructor's arguments do (`Box(5)` → `Box<i64>`). A construction
that pins no annotation and whose arguments cannot determine a parameter is
rejected (`L0455`) — supply an annotation. Spelling a generic type with the wrong
number of type arguments (or none) is `L0454`; fields that disagree on a shared
parameter are `L0395`.

The three interpreters (AST, IR, bytecode) run a generic struct by **type
erasure** — at runtime a generic struct is just a struct over dynamic values, so
`Box<i64>` and `Box<bool>` share one runtime shape and produce identical results
on every interpreter. The native and WASM backends (which need concrete field
layout) treat a function that uses a generic struct as ineligible for now and
cleanly skip it to the interpreter (`L0339`), never miscompiling; per-instantiation
monomorphization on those backends is a later stage.

A generic `struct` may declare **multiple type parameters** and **bounds** on a
parameter (`struct Pair<K, V>`, `struct Sorted<T: Ord>`); both are covered under
"Multiple Type Parameters and Bounds". Methods on generic types ship too (see
"Methods on Generic Types"). Heap-typed `T` (`string`/`list`/`map`/heap struct)
runs on the interpreters today; native/WASM monomorphization for heap `T` is a
later stage. Trait objects (`dyn`) and associated types remain on the roadmap; the
built-in generics `option`, `result`, `list`, `map`, `array`, `rc`, `ref`, and
`ptr` are available today.

### Generic User Enums

A **generic `enum`** declares type parameters in angle brackets after its name,
exactly like a generic struct, and each variant payload may mention them:

```lullaby
enum Opt<T>
    present T
    absent

fn unwrap_or o Opt<i64> fallback i64 -> i64
    match o
        present(x) -> x            # x binds as i64: T substituted from Opt<i64>
        absent -> fallback

fn main -> i64
    let a Opt<i64> = present(5)    # T = i64 (from the annotation)
    let b = present(true)          # T = bool (inferred from the payload)
    let missing Opt<i64> = absent  # unit variant: T from the annotation
    unwrap_or(a, 0)
```

A concrete instantiation is spelled `Name<Args>` (`Opt<i64>`) wherever a type is
written, and a `match` over an instance substitutes the type arguments for the
parameters, so a `present(x)` arm on `Opt<i64>` binds `x` as `i64`. Type arguments
are fixed **by inference, not a turbofish**: an annotation on the
binding/parameter/return pins them, or a payload-carrying variant's arguments do
(`present(5)` → `Opt<i64>`). A **unit** variant of a generic enum (`absent`)
carries no payload to infer from, so it needs the annotation; without one it is
`L0455`. When an annotation is present it is authoritative, so a payload argument
that disagrees with the pinned parameter is a clean payload mismatch (`L0381`).
Spelling a generic enum with the wrong number of type arguments (or none) is
`L0454`; payloads that disagree on a shared parameter are `L0395`.

**Recursion through an indirection.** A value-semantic recursive type is
infinitely sized, so a generic enum may recurse on itself only when the recursion
passes through a heap/pointer **indirection** — `rc<...>`, `ref<...>`, `ptr<...>`,
`list<...>`, `map<...>`, or `array<...>`:

```lullaby
enum Tree<T>
    leaf T
    branch list<Tree<T>>           # OK: recursion goes through list<...>
```

A **direct** self-recursion by value (`node Tree<T>` inside `enum Tree<T>`, or one
nested only through the by-value tagged unions `option`/`result`) is rejected with
`L0456`.

Like generic structs, the three interpreters run a generic enum by **type
erasure** — at runtime a generic enum is just a tagged union over dynamic values,
so every instantiation shares one runtime shape and produces identical results on
`ast`/`ir`/`bytecode`. The native and WASM backends treat a function that uses a
generic enum as ineligible for now and cleanly skip it to the interpreter
(`L0339`/`L0338`), never miscompiling; per-instantiation monomorphization is a
later stage. A generic `enum` may declare **multiple type parameters**
(`enum Either<L, R>`) and **bounds** on a parameter, plus the
recursion-through-indirection rule; see "Multiple Type Parameters and Bounds".
Heap-`T` native monomorphization is staged next.

### Methods on Generic Types

An **inherent `impl` block** attaches methods to a generic type. `impl Box<T>`
introduces the type parameter `T` over every method in the block; a method's
signature and body use `T` and the `self` receiver like any other type:

```lullaby
struct Box<T>
    value T

impl Box<T>
    fn peek self -> T                # returns the wrapped value as the concrete T
        self.value
    fn rewrap self v T -> Box<T>     # takes a T, returns a fresh Box<T>
        Box(v)

fn main -> i64
    let a Box<i64> = Box(5)
    let flag Box<bool> = Box(true)
    let bumped Box<i64> = a.rewrap(9)  # T = i64: rewrap takes i64, returns Box<i64>
    a.peek() + bumped.peek() + (1 if flag.peek() else 0)  # peek() is i64 / bool
```

An inherent `impl` has **no `for`** — that distinguishes it from a trait impl
(`impl Show for Point`). A method is called with the usual receiver syntax
(`recv.method(args)`), which resolves the method by the **receiver's concrete
instantiation**: unifying the method's `self` type (`Box<T>`) against the
receiver's type (`Box<i64>`) pins the type parameters, and those are substituted
into the method's parameter and return types. So `peek` returns `i64` on a
`Box<i64>` and `bool` on a `Box<bool>`; a parameter of type `T` accepts exactly
the receiver's `T`. A wrong-typed argument is `L0313`; calling a method the
receiver's type does not declare is `L0457`. A generic `enum` takes methods the
same way (an `impl Opt<T>` method may `match self`, binding a payload as the
concrete `T`).

Method names share the **receiver-dispatch namespace** with trait methods: a
method name must be distinct from every free-function and trait-method name
(`L0398`), and a method may be declared only once per type (`L0399`). Like the
generic types themselves, methods run by **type erasure** on the three
interpreters — dispatch on a generic type is ordinary receiver dispatch at
runtime, so a program calling generic-type methods runs identically on
`ast`/`ir`/`bytecode`. A function that calls a generic-type method is
native-ineligible for now and cleanly skips to the interpreter (`L0339`), never
miscompiling; per-instantiation monomorphization on the native/WASM backends is a
later stage. An inherent impl may itself carry multiple type parameters and
bounds (`impl Pair<K, V>`, `impl Sorted<T: Ord>`); see "Multiple Type Parameters
and Bounds".

### Multiple Type Parameters and Bounds

A generic type may declare **more than one type parameter**, and each is inferred
independently. Two parameters are written in the angle-bracket list exactly as a
generic function declares them:

```lullaby
struct Pair<K, V>
    key K
    value V

enum Either<L, R>
    left L
    right R

impl Pair<K, V>
    fn get_key self -> K              # returns the first field as the concrete K
        self.key
    fn get_value self -> V            # returns the second field as the concrete V
        self.value

fn main -> i64
    let a Pair<i64, bool> = Pair(key: 10, value: true)   # K = i64, V = bool
    let b Pair<string, i64> = Pair(key: "id", value: 32) # K = string, V = i64
    a.get_key() + b.get_value()                          # 10 + 32 = 42
```

Every parameter must be pinned — by the annotation or by the constructor's
arguments — and each field/variant substitutes its own parameter, so `a.get_key()`
is `i64` and `b.get_value()` is `i64`. Spelling a multi-parameter type with the
wrong number of arguments (`Pair<i64>`) is `L0454`. An inherent `impl` restates
the full parameter list (`impl Pair<K, V>`), and a method resolves each parameter
from the receiver's instantiation.

A **trait bound** on a type parameter constrains which types may instantiate it,
so the type's methods may call the bound trait's methods on a `T` value. The bound
is written on the declaration (`struct Sorted<T: Ord>`, `enum Holder<T: Show>`) and
propagates into the type's inherent-impl methods; it may equivalently be written on
the impl (`impl Sorted<T: Ord>`). Bounds reuse the same grammar and dispatch as
bounds on generic functions (`<T: A + B>`):

```lullaby
trait Rank
    fn rank self -> i64

struct Card
    value i64

impl Rank for Card
    fn rank self -> i64
        self.value * 10

struct Ranked<T: Rank>
    item T

impl Ranked<T>
    fn score self -> i64
        self.item.rank() + 100        # `T: Rank` lets a T value call rank()

fn main -> i64
    let c Ranked<Card> = Ranked(Card(4))
    c.score()                          # 4*10 + 100 = 140
```

**Bounds are enforced at every instantiation.** A concrete type argument must
implement each trait its parameter position requires; `Ranked<i64>` — where `i64`
does not implement `Rank` — is rejected with `L0400` (the same unsatisfied-bound
diagnostic used for bounded generic functions). A bare in-scope type variable used
as a type argument is not checked concretely (its bound is enforced where that
outer variable is pinned).

Like the rest of the generics surface, multi-parameter and bounded generic types
run by **type erasure** on the three interpreters, so a program using them runs
byte-for-byte identically on `ast`/`ir`/`bytecode`. A function that uses one is
native-ineligible for now and cleanly skips to the interpreter (`L0339`), never
miscompiling.

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
otherwise all-`i64` function stays native-eligible.

### Const-sized arrays `array<T, N>`

A **const-sized array** `array<T, N>` is a fixed-extent array: the same
length-carrying `array<T>` value, annotated with a compile-time extent `N`. It
gives typed fixed-extent buffers a name in a signature or a struct — `fn blit(buf
array<u8, 512>)`, `struct Frame { pixels array<u32, 1024> }` — which kernel and
systems code needs.

```lullaby
const WIDTH i64 = 512

fn blit buf array<u8, WIDTH> -> i64
    len(buf)

fn main -> i64
    let row array<i64, 4> = [10, 20, 30, 40]   # literal extent
    let zero array<u8, WIDTH> = [0u8; WIDTH]    # fill literal, named extent
    row[0] + len(zero)
```

- **Extent `N`.** `N` is a *constant expression* that folds to a positive
  integer: an integer literal (`array<u8, 512>`) or a named integer constant
  (`array<u8, WIDTH>`, reusing the same fold as `const`). A non-constant extent is
  `L0463`; a zero or negative extent is `L0464`. `N` is allowed in every type
  position — `let`, parameters, fields, return types, and nested
  (`option<array<u8, 16>>`, `array<array<i64, 3>, 2>`).
- **Fill literal `[value; count]`.** The terse constructor for a fixed-extent
  array: `value`, repeated `count` times (`count` is a constant expression, like
  an extent). `[0; 512]` is a 512-element zeroed buffer without spelling 512
  values. The `value` is materialized once per element; it composes with the
  ordinary `[a, b, c]` list literal (the `;` separator is lexed only inside
  brackets — a `;` elsewhere is still the forbidden statement terminator `L0103`).
- **Construction-count check.** A statically-counted literal (`[a, b, c]` or
  `[value; k]`) initializing an `array<T, N>` slot must have exactly `N` elements
  (`L0465`), checked at `let` annotations, `return`/trailing expressions,
  struct-literal fields, and free-function call arguments, at every nesting level.
- **Decay.** The extent exists *only* as this construction-count assertion.
  `array<T, N>` otherwise **decays to `array<T>`** for every typing purpose — it
  shares the element type and the runtime representation — so a fixed-extent array
  interoperates freely with the length-agnostic surface (`len`, `a[i]`, `for x in
  a`, passing to an `array<T>` helper), and an already-`array<T>` value (whose
  length is dynamic) is accepted where `array<T, N>` is expected.

Like `const`, const-sized arrays are **frontend-only**. The array-extent pass
resolves and validates every extent, checks construction counts, then **erases**
the extent (`array<T, N>` becomes `array<T>`) and expands every fill literal to an
ordinary array literal, all before the type checker runs. Every backend therefore
sees only the existing `array<T>` representation: `array<T, N>` compiles and runs
exactly wherever the same-length `array<T>` already does, on every tier
(AST/IR/bytecode interpreters, native, WASM), with no new miscompile surface. A
dedicated *inline, by-value, no-heap* native storage layout for fixed-extent
arrays is a separate later increment; today the extent is a checked annotation
over the heap `array<T>`.

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
