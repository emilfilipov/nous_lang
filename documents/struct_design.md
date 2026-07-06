# Struct / Record Type Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This note designs Lullaby's struct (record) surface. The guiding constraint is
the language aesthetic: token-minimalist, indentation-only, no braces, no
semicolons, strongly typed. The struct surface reuses the shapes the language
already has (function-parameter spelling for fields, call spelling for
construction, `.` for access) so nothing new needs to be learned.

## Declaration

A struct is declared at the top level with the `struct` keyword and an
indentation-only field list. Each field is `name type` — exactly the spelling
of a function parameter, with no braces or commas:

```lby
struct Point
    x i64
    y i64

struct Player
    name string
    score i64
    alive bool
```

Rules:
- Fields are ordered; a struct type is nominal (identified by its name).
- Duplicate struct names and duplicate field names are errors.
- Field types may be any existing type, including other structs and `array<T>`.

## Construction

Construction reuses call spelling — positional arguments in declaration order.
This is the most token-minimal option and reads like any other call:

```lby
let origin Point = Point(0, 0)
let p1 Point = Point(3, 4)
let hero Player = Player("Ada", 100, true)
```

Semantics resolves `Name(...)` as a struct construction when `Name` is a
declared struct, otherwise as a function call, so there is no new syntax. Arity
and per-field types are checked against the declaration.

## Field access

Field access uses `.`, parsed as a postfix like array indexing:

```lby
fn distance_squared p Point -> i64
    p.x * p.x + p.y * p.y

fn greet hero Player -> string
    "player " + hero.name + " has " + to_string(hero.score)
```

An unknown field name is a compile-time error.

## Scope of the first increment

Implemented first (this ticket's build subtasks):
- Declaration, positional construction, and field **read** access.
- Structs as function parameters, return values, locals, and inside `array<T>`.
- Full type checking with clear diagnostics, running on the AST, IR, and
  bytecode backends at parity.

Implemented since:
- Field **mutation** (`p.x = 5`, `p.y += 1`, nested `a.b.c = e`) across all
  backends. The assignment target is a variable plus an optional field path;
  the root variable is what optimizers track, so a field write correctly
  invalidates expressions over that variable.
- **Named-field construction** (`Point(x: 1, y: 2)`) alongside positional
  construction. Fields may appear in any order; each declared field must be
  provided exactly once. Named construction parses to a distinct
  `StructLiteral` node and is reordered into declared field order during IR
  lowering and AST evaluation, so it produces the identical value on all three
  backends. Duplicate, missing, or unknown fields report `L0372`.

Deferred (follow-up tickets, kept out to stay minimal):
- Methods / associated functions, generics over structs, and struct update
  syntax.

## Why these choices

- **Fields as `name type`**: identical to parameters, so the reader already
  knows the shape; zero new punctuation.
- **Positional call construction**: fewest tokens, reuses the call parser, and
  matches how every other value is produced.
- **`.` access**: universal and terse; one token per field hop.
- **Nominal typing**: simplest strong-typing story and avoids accidental
  structural coercion between unrelated records.
