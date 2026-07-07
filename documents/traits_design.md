# Traits / Interfaces Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).
Builds on [generics_design.md](generics_design.md) (generic functions) and the
UFCS method-call sugar (`x.m(a)` desugars to `m(x, a)`).

A trait names a set of methods a type can implement. Traits give Lullaby
polymorphism two ways: **bounded generics** (write one generic function that
works for every type implementing a trait) and **direct method calls** (call a
trait method on a concrete value). Unlike modules/generics, traits touch the
backends: method calls dispatch on the receiver's concrete type at run time.

## Declaration

```lby
trait Show
    fn show self -> string

trait Ord
    fn less self other Self -> bool
```

- `trait Name` introduces an indentation-only list of method signatures.
- The first parameter is the receiver, named `self`; its type is the
  implementing type. `Self` may be used as a type in later parameters/returns to
  mean "the implementing type".
- Signatures have no body. Default method bodies are deferred.

## Implementation

```lby
struct Point
    x i64
    y i64

impl Show for Point
    fn show self -> string
        "(" + to_string(self.x) + ", " + to_string(self.y) + ")"
```

- `impl Trait for Type` provides a body for every method the trait requires.
- Missing or extra methods, or a signature that does not match the trait
  (accounting for `Self` = `Type`), are errors (`L0398`).
- A type may implement several traits; a trait may be implemented for many
  types. Duplicate `impl Trait for Type` is an error (`L0399`).

## Calling trait methods

Trait methods are called with the existing UFCS spelling:

```lby
fn label p Point -> string
    p.show()
```

`p.show()` desugars (as today) to `show(p)`. Resolution: when a called name is a
trait method (not a free function), it dispatches to the `impl` of that method
for the **receiver's concrete type**. At run time the receiver value carries its
type (a `struct`/`enum` name), so dispatch is a lookup of
`(concrete_type, method) -> impl function`.

## Bounded generics

A generic type parameter may require traits, enabling bounded operations on it:

```lby
fn max_by<T: Ord> a T b T -> T
    if a.less(b)
        return b
    a

fn show_all<T: Show> xs list<T> -> string
    ...
```

- `<T: Ord>` (or `<T: Ord + Show>`) constrains `T`. Inside the function, a value
  of type `T` may call the methods of its bound traits (`a.less(b)`), which the
  bare-`T` rules in the generics increment otherwise forbid.
- At a **call site**, the inferred concrete type for `T` must implement every
  bound trait, or it is an error (`L0400`). This is checked during the same
  call-site unification generics already performs.

## Representation and backends

- No new value kind: a receiver is an ordinary `Value` (`Struct`/`Enum`/scalar)
  that already carries its type name.
- A shared **impl table** `(type_name, method_name) -> function` is built once
  from all `impl` blocks and consulted by the AST interpreter, IR interpreter,
  and bytecode VM. A call whose name is a trait method dispatches through this
  table on the receiver's runtime type; a call whose name is a free function
  uses the existing path. This is the one backend-visible change, and it is
  applied identically on all three backends so results stay at parity.
- Bounded-generic dispatch is the same lookup: because generics are erased, a
  `T: Ord` value is just its concrete value at run time, and `a.less(b)`
  dispatches on that concrete type.

## Diagnostics

- `L0398` — an `impl` does not satisfy the trait (missing/extra/mismatched
  method).
- `L0399` — duplicate `impl Trait for Type`.
- `L0400` — a type used where a trait bound is required does not implement it.

## Scope and sequencing

First increment: trait declarations, `impl Trait for Type`, concrete trait-method
calls dispatched by receiver type, and single/multiple trait bounds on generic
functions. Deferred: default method bodies, trait objects (`dyn Trait` as a
runtime-polymorphic value in a collection), associated types/constants,
supertraits, and blanket impls.

## Why these choices

- **Reuse UFCS method calls**: `x.m()` already reads as a method; trait dispatch
  slots in behind it with no new call syntax.
- **`self` receiver + `Self` type**: the minimal spelling for "the implementing
  type", matching the parameter shape the reader knows.
- **One shared impl table**: keeps the three backends identical and dispatch
  trivial (a name+type lookup), avoiding vtable machinery until trait objects
  are actually needed.
- **Bounds reuse generic inference**: the bound check rides on the call-site
  unification generics already does, so bounded generics add checking, not a new
  mechanism.
