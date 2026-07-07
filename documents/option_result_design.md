# option<T> / result<T, E> Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).
Builds on [enum_and_match_design.md](enum_and_match_design.md) (enums + `match`).

`option<T>` and `result<T, E>` are the two core sum types for absence and
fallible results. They ship as **built-in generic enums** — the compiler knows
their variants — because user-defined generics do not exist yet. Delivering
them also lays the small generics foundation (multi-argument generic type
spelling and context-directed type inference) that user generics, `list<T>`,
and `map<K, V>` will reuse.

## Types and variants

- `option<T>` has variants `some(value T)` and `none`.
- `result<T, E>` has variants `ok(value T)` and `err(error E)`.

Type spelling reuses the existing generic form and extends it to two arguments:
`option<i64>`, `result<i64, string>`, `option<array<i64>>`. These are nominal,
built-in generic types (like `array<T>`), not user declarations.

Variant names `some`, `none`, `ok`, `err` are reserved globally (they may not be
declared as user enum variants — reuse the `L0382` collision check).

## Construction and type inference

Construction reuses call/bare-name spelling exactly like user enum variants:

```lby
let a option<i64> = some(3)
let b option<i64> = none
let r result<i64, string> = ok(3)
let e result<i64, string> = err("boom")
```

Some type parameters cannot be inferred from the payload alone, so construction
is **checked against an expected type** supplied by context. The expected type
flows from exactly these sites (this is the generics foundation):

1. a `let name TYPE = init` annotation → `TYPE` is expected for `init`;
2. a function's declared return type → expected for `return EXPR` and for the
   function's final expression;
3. a `match` scrutinee's type → used to type variant payload bindings.

Inference rules (given optional expected type `X`):

- `some(v)` synthesizes `option<typeof v>`. If `X = option<U>`, require
  `typeof v == U`.
- `ok(v)` requires `X = result<T, E>`; require `typeof v == T`; result type is
  `X`. Without an expected `result<...>`, report `L0386`.
- `err(v)` requires `X = result<T, E>`; require `typeof v == E`; result type is
  `X`. Without an expected `result<...>`, report `L0386`.
- `none` requires `X = option<U>`; result type is `X`. Without an expected
  `option<...>`, report `L0386`.

`L0386` = "cannot infer the type of `none`/`ok`/`err`; add a type annotation or
return type". This keeps the first increment honest: where the type is
determinable it just works; where it is not, the user adds an annotation.

Argument-position and other expected-type sites are deferred; annotate the
`let`/return when needed.

## Pattern matching

`match` already dispatches on variant name. Extend its variant resolution so an
`option<U>` scrutinee knows `some(x)` binds `x: U` and `none` binds nothing, and
a `result<T, E>` scrutinee knows `ok(x)` binds `x: T` and `err(x)` binds `x: E`.
Exhaustiveness: `option` is covered by `some` + `none` (or `_`); `result` by
`ok` + `err` (or `_`).

```lby
fn unwrap_or o option<i64> fallback i64 -> i64
    match o
        some(v) -> v
        none -> fallback

fn describe r result<i64, string> -> string
    match r
        ok(v) -> "ok " + to_string(v)
        err(m) -> "err " + m
```

## Representation and backends

- Runtime value reuses `Value::Enum` with `enum_name` `"option"`/`"result"` and
  `variant` `some`/`none`/`ok`/`err`. The runtime is dynamically typed, so
  construction and `match` need no special runtime code beyond recognizing these
  built-in variant names during construction (mirror how user variants build
  `Value::Enum`).
- All three backends (AST, IR, bytecode) and the optimized IR/bytecode stay at
  parity, verified by a `run_option_result.lby` fixture in the parity harness.

## Generics foundation added here (reused later)

- Parser `expect_type`: accept `option<T>` (one arg) and `result<T, E>` (two
  comma-separated args). Generalize to parse a comma-separated generic argument
  list so `result<...>` and future multi-arg generics work; store the canonical
  string form `result<i64, string>`.
- `TypeRef`: add `generic_args(ctor) -> Option<Vec<TypeRef>>` that splits the
  top-level (nesting-aware) comma-separated argument list, so `result<T, E>`
  yields `[T, E]`. Keep the existing single-arg `generic_arg` working.
- Semantics: a `check_expr` path that carries an optional expected `TypeRef`,
  threaded from `let`-annotation and `return`/final-expression sites. Non-target
  expressions ignore it and behave exactly as before (no regressions).

## Diagnostics

- `L0386` — cannot infer `none`/`ok`/`err` type without a `result`/`option`
  expected type or annotation.
- Reuse `L0303` for a `some`/`ok`/`err` payload whose type disagrees with the
  annotation, and `L0384`/`L0385` for non-exhaustive or invalid `match` arms.

## Scope

First increment: the two types, construction with context inference at
`let`/return, `match` support, all backends at parity. Deferred: expected-type
inference in argument position, `?`-style propagation sugar, and generic
`map`/`list` (separate tickets), all of which reuse this foundation.
