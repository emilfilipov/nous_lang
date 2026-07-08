# `?` Error-Propagation Operator Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).
Builds on [[option_result_design]] (`option<T>` / `result<T, E>`) and
[[enum_and_match_design]] (enums + statement-form `match`).

The postfix `?` operator is terse sugar for the recurring "unwrap or bail out"
pattern over the two core sum types. `expr?` yields the success payload when the
value is `ok`/`some`, and otherwise short-circuits the enclosing function with a
matching `err`/`none`. It removes the boilerplate `match` that today wraps every
fallible call while introducing **no new runtime value** — it is fully erased
before any backend runs.

The single hard constraint that shapes this whole design: in this compiler
`match` is a **statement-only** construct in the IR. A `match` may *appear* in
expression position in surface syntax, but the semantic/lowering pipeline only
admits `match` as a `Stmt`; an expression subtree can never contain a live
`match` node when it reaches a backend. Because `?` fundamentally *is* a
conditional early return, it cannot desugar to an inline `match` expression. The
[Lowering strategy](#lowering-strategy-the-crux) section solves this concretely
with a statement-level A-normal-form (ANF) hoist.

## Semantics

Let `f` be the function that lexically encloses the `?`, with declared return
type `R`.

- **`expr?` where `expr: result<T, E>`**
  - If `expr` evaluates to `ok(v)`, the whole `expr?` expression evaluates to
    `v : T`.
  - If `expr` evaluates to `err(e)`, control leaves `f` immediately, as if by
    `return err(e)`. No later code in `f` runs.
  - **Enclosing-return compatibility:** `R` must be `result<U, E>` for some `U`.
    The error arm `E` must match **exactly** (nominal type equality, the same
    check `let`/`return` already use); the success arm `U` is unconstrained by
    `?` itself (it is fixed by whatever `f` actually returns on its success
    paths). No implicit `E`-to-`E'` conversion is performed in the first
    increment — an `err(e: E)` can only propagate out of a function returning
    `result<_, E>`. (A `From`-style widening is deferred; see
    [Scope and sequencing](#scope-and-sequencing).)

- **`expr?` where `expr: option<T>`**
  - If `expr` evaluates to `some(v)`, `expr?` evaluates to `v : T`.
  - If `expr` evaluates to `none`, control leaves `f` immediately as if by
    `return none`.
  - **Enclosing-return compatibility:** `R` must be `option<U>` for some `U`.
    There is no error type to match; only the outer constructor kind
    (`option`) must agree.

- **Where `?` is allowed**
  - Only in the body of a function whose return type `R` is `option<...>` or
    `result<...>`. Outside such a function (a `void`/value-returning function,
    the top level, or any context with no compatible enclosing return) `?` is a
    compile-time error (`L0427`).
  - The operand's static type must be exactly `option<T>` or `result<T, E>`.
    Applying `?` to any other type (`i64?`, `string?`, a struct, an `array<T>`)
    is a compile-time error (`L0428`).
  - The **kind** of the operand must agree with the kind of `R`: `?` on a
    `result` inside an `option`-returning function, or on an `option` inside a
    `result`-returning function, is a compile-time error (`L0429`). This is a
    distinct, more specific diagnostic than the exact-`E` mismatch, which is
    also `L0429` with a differently worded detail (result/result but `E`
    differs). Both are "the operand cannot propagate through this function's
    return type".
  - `?` is permitted anywhere an expression is permitted *within* such a
    function body — nested in call arguments, operands of `+`, indices, `return`
    operands, `let` initializers, `match` scrutinees, and so on. The hoist in
    the next section is what makes arbitrary nesting sound.
  - `?` is **not** permitted in a position that is not dominated by a normal
    statement sequence in a way the hoist can target — specifically, it may not
    appear in a **default/constant** position that is evaluated outside function
    control flow. In the current language every expression that can contain `?`
    is reached through a function-body statement, so this restriction is
    automatically satisfied; it is stated for completeness against future
    top-level `const` initializers.

### Type rules (formal)

Given the operand type `S = typeof(expr)` and enclosing return type `R`:

```
S = result<T, E>,  R = result<U, E>      ⊢  expr? : T
S = option<T>,     R = option<U>         ⊢  expr? : T
```

Any other combination is rejected:

| Operand `S`      | Enclosing `R`        | Result       | Diagnostic |
|------------------|----------------------|--------------|------------|
| `result<T, E>`   | `result<U, E>`       | `T`          | ok         |
| `option<T>`      | `option<U>`          | `T`          | ok         |
| `result<T, E>`   | `result<U, E2>` E≠E2 | —            | `L0429`    |
| `result<T, E>`   | `option<U>`          | —            | `L0429`    |
| `option<T>`      | `result<U, E>`       | —            | `L0429`    |
| `result`/`option`| not `result`/`option`| —            | `L0427`    |
| any other type   | any                  | —            | `L0428`    |

`?` participates in ordinary type flow: `expr?` has type `T`, so it can feed any
context expecting `T` (arithmetic, a call argument, another `?` if `T` is itself
an `option`/`result`, etc.).

## Parsing

`?` is a **postfix unary operator** with the same (highest) precedence tier as
call `f(...)`, index `a[i]`, and field access `x.f`. It binds tighter than any
prefix or binary operator, so it always attaches to the smallest complete
postfix expression to its left and never captures a following binary operand.

Grammar (extending the existing postfix chain):

```
postfix   := primary ( call_suffix | index_suffix | field_suffix | "?" )*
```

Because it lives in the postfix loop, `?` chains and interleaves naturally with
the other suffixes:

- `f()?`        → apply `?` to the call result.
- `g()?.field`  → `?` first, then field access on the unwrapped value.
- `a?[i]`       → `?` on `a`, then index.
- `f(g()?)?`    → inner `g()?` unwraps before it is passed to `f`; the outer `?`
  unwraps `f(...)`.
- `x??`         → legal iff `x : result<result<T,E>, E>` (or the `option`
  analogue): the first `?` yields `result<T,E>`, the second yields `T`.

`?` is purely postfix; there is no prefix or infix form. The lexer already
produces a `?` token for other planned uses; the parser disambiguates by
position (a `?` immediately after a complete postfix expression is the
propagation operator).

### `.lby` examples

```lby
fn parse_pair line string -> result<i64, string>
    let a i64 = parse_int(field(line, 0))?      # unwrap or return err(...)
    let b i64 = parse_int(field(line, 1))?
    ok(a + b)

fn first_positive xs list<i64> -> option<i64>
    let head i64 = first(xs)?                    # returns none if xs empty
    match head > 0
        true -> some(head)
        false -> none

fn combine a string b string -> result<i64, string>
    ok(parse_int(a)? + parse_int(b)?)            # two ? inside one expression

fn deep s string -> result<i64, string>
    ok(scale(parse_int(s)?)? )                   # chained: inner then outer ?
```

Each `?` above expands to a hoisted temporary plus an early-return `match`
statement, shown next.

## Lowering strategy (the crux)

### Chosen approach: statement-level ANF hoist (option **a**)

`?` is lowered by a **statement-level desugaring** that runs after parsing and
name/type resolution, before IR lowering. For every statement `S` in a function
body, the pass rewrites `S` so that each `?` subexpression is:

1. evaluated into a fresh temporary `let`, and
2. immediately followed by an early-return `match` statement that either binds
   the success payload or returns the failure variant,

leaving in the original expression position only a plain reference to the
success temporary. This is an A-normal-form transform: `?` operands are lifted
out of expression position into named intermediate bindings, so the expression
that remains is `?`-free and the only conditional-return construct introduced is
a **statement** `match` — exactly what the statement-only-`match` IR requires.

Concretely, the pass walks each `Stmt` and, whenever it finds a `?` node in any
contained `Expr`, replaces that node with a `Variable(tmp)` reference and
records a pair of prelude statements to emit *before* the current statement:

```
# for  expr?  where expr : result<T, E>, enclosing R = result<U, E>:
let __q0 = <expr-with-inner-?-already-hoisted>
match __q0
    ok(__v0) -> __v0        # value of the hoisted position
    err(__e0) -> return err(__e0)
```

The `match` here is a real statement `match` (a `Stmt`, not an `ExprKind::Match`
left in expression position). It has two arms: the success arm evaluates to the
bound payload, the failure arm is a `return`. To keep `match` purely a
statement, the success payload is itself hoisted into a second temporary
`__v0` and the outer expression references `__v0` (not the `match`):

```
let __q0 = <expr>
let __v0 <T>
match __q0
    ok(x) -> __v0 = x       # statement match: assign, don't yield
    err(e) -> return err(e)
# ... __v0 substituted into the original expression position ...
```

This is the canonical, backend-safe shape: a `let` binding the operand, a
declared success temporary, a **statement `match`** whose arms are
`Stmt`-blocks (`Assign` in the success arm, `Return` in the failure arm), and
then the rewritten host statement that reads `__v0`. No `match` ever survives in
expression position.

### Fresh-name and ordering discipline

- Temporaries use a reserved, source-illegal prefix (`__q<N>` / `__v<N>`) with a
  per-function counter, so they cannot collide with user names.
- Multiple `?` in one statement are hoisted **left-to-right in evaluation
  order**. The pass performs a left-to-right, depth-first traversal of the host
  expression; each `?` it reaches emits its prelude *in traversal order*, so the
  emitted `let`/`match` sequence preserves exactly the evaluation order the
  source implies. An inner `?` (`f(g()?)`) is hoisted before the `?` that
  encloses it, because the inner one is reached first in a depth-first,
  operands-before-operator walk.
- Short-circuit correctness: because each failure arm is a `return`, once an
  earlier `?` fails, none of the later hoisted preludes execute — matching the
  "first failure wins, later effects skipped" guarantee a hand-written `match`
  cascade would give.

### Before / after: nested arithmetic

Source:

```lby
fn total a string b string -> result<i64, string>
    ok(parse_int(a)? + 10 + parse_int(b)?)
```

After the ANF hoist (conceptual desugared form):

```lby
fn total a string b string -> result<i64, string>
    let __q0 = parse_int(a)
    let __v0 i64
    match __q0
        ok(x) -> __v0 = x
        err(e) -> return err(e)
    let __q1 = parse_int(b)
    let __v1 i64
    match __q1
        ok(x) -> __v1 = x
        err(e) -> return err(e)
    ok(__v0 + 10 + __v1)
```

Note `parse_int(a)?` is fully evaluated (and may short-circuit) **before**
`parse_int(b)?`, preserving left-to-right evaluation. The final `ok(...)` is
`?`-free.

### Before / after: chained `f(g()?)?`

Source:

```lby
fn deep s string -> result<i64, string>
    ok(scale(parse_int(s)?)?)
```

After the hoist (inner `?` first, then outer):

```lby
fn deep s string -> result<i64, string>
    let __q0 = parse_int(s)
    let __v0 i64
    match __q0
        ok(x) -> __v0 = x
        err(e) -> return err(e)
    let __q1 = scale(__v0)
    let __v1 i64
    match __q1
        ok(x) -> __v1 = x
        err(e) -> return err(e)
    ok(__v1)
```

### `option` form

For `expr?` where `expr : option<T>` in an `option`-returning function, the
statement `match` is:

```
let __q0 = <expr>
let __v0 <T>
match __q0
    some(x) -> __v0 = x
    none -> return none
```

### Where the pass lives and how it composes

- The hoist is a **desugaring pass over the resolved AST** (a `Stmt`-vector
  rewrite), sitting between semantic analysis and IR lowering. It runs after
  type checking so that: (1) the operand type (`option`/`result` and its `T`)
  is known, letting the pass pick `ok`/`some` vs `err`/`none` arms and give
  `__v0` the correct declared type; (2) the enclosing `R` is known, so the
  `return` operand's variant is correct. The `L0427`/`L0428`/`L0429` checks are
  raised in **semantics** (before the pass); the pass assumes a well-typed
  `?` and never fails.
- Because the pass emits only constructs the pipeline *already* lowers —
  `Stmt::Let`, `Stmt::Assign`, `Stmt::Return`, and a statement-form
  `ExprKind::Match` consumed as a `Stmt` — **no new IR node and no new lowering
  rule is required**. Every existing backend lowering for `let`/`assign`/
  `return`/`match` handles the desugared output verbatim.
- Statements that contain no `?` are passed through untouched (zero cost, no
  regressions), exactly as the context-inference threading already does for
  non-target expressions.

### Why not a dedicated IR node (option **b**)

A `Try(expr)` IR node was considered and rejected. It would force **every**
backend (AST interpreter, IR interpreter, bytecode VM, and the optimizer's IR
passes) to grow a new case that internally performs the same conditional early
return — re-implementing, three-plus times, the control flow that the
statement-`match` lowering already provides. It would also reintroduce
"expression that can non-locally return", which is precisely the shape the
statement-only-`match` IR is designed to forbid. The ANF hoist keeps the
non-local control flow expressed as ordinary statements, so the invariant "no
conditional return hides inside an expression" is preserved end-to-end.

## Backend parity

`?` is **erased before lowering**: after the ANF pass, the AST handed to each
backend contains no `?` and no expression-position `match` — only `let`,
`assign`, `return`, and statement `match`. Consequently:

- **AST interpreter:** runs the desugared statements directly; the statement
  `match` uses its existing tag-compare + payload-bind path, the `return` arm
  uses its existing early-return mechanism.
- **IR lowerer / IR interpreter:** the desugared `let`/`assign`/`return`/`match`
  lower through the existing rules. The IR lowerer's type re-derivation sees a
  `?`-free tree; the only requirement is that the desugaring runs *before*
  lowering so the IR never encounters a `?`. No `call_return_type`-style special
  case is needed because `?` produced ordinary temporaries.
- **Bytecode VM:** identical — it consumes the same lowered IR.
- **Optimized IR / bytecode:** the desugared form is plain enough that existing
  optimizer passes (constant folding, dead-store, etc.) apply unchanged; there
  is no `?`-specific node to teach the optimizer about.

Parity is verified by a `run_error_propagation.lby` fixture (mixing `result`
and `option` propagation, chained `f(g()?)?`, multiple `?` in one expression,
and both success and short-circuit paths, folded into a deterministic `i64`)
run through the auto-discovering parity harness across all backends including
the optimized ones — mirroring `run_option_result.lby` and `run_generics.lby`.

## Diagnostics

Proposed new codes (registry not edited here; these extend the `L04xx`
semantic block, current highest is `L0426`):

- **`L0427`** — `?` used outside a function whose return type is a compatible
  `option`/`result`. Message points at the `?` and names the enclosing function's
  actual return type. Fix hint: change the return type to `option<...>` /
  `result<...>`, or handle the value with `match`.
- **`L0428`** — `?` applied to a non-`option`/`result` operand. Message names the
  operand's actual type. Fix hint: `?` only unwraps `option<T>` / `result<T, E>`.
- **`L0429`** — enclosing return type incompatible with the operand: either
  kind mismatch (`result` operand in an `option` function or vice versa) or a
  `result`/`result` pair whose `E` types differ. Message shows both the operand
  type and the enclosing return type and states which arm disagrees. Fix hint:
  make the function return `result<_, E>` with the same `E` (no implicit error
  conversion yet), or convert the error before propagating.

Reuse existing codes where they already fit: a malformed `?` token sequence is an
ordinary parse error; a success temporary whose inferred `T` disagrees with a
later use is caught by the existing type-mismatch code (`L0303`), not a new one.

## Scope and sequencing

First increment (this design):

1. Postfix `?` parsing in the existing postfix-suffix loop.
2. Semantic checks: operand must be `option`/`result`; enclosing `R` must be a
   compatible `option`/`result` with exact `E` match; emit
   `L0427`/`L0428`/`L0429`. Give `expr?` type `T`.
3. The ANF hoist pass (resolved-AST `Stmt` rewrite) producing `let` + statement
   `match` + success temporary.
4. All backends at parity via `run_error_propagation.lby`; no new IR node.

Deferred:

- **Error widening** (`From`-style `E → E2` on propagation) so a `?` can lift an
  inner error into a different enclosing error type. Requires a conversion trait;
  arrives with / after the trait system (see [[traits_design]]). Until then, `E`
  must match exactly.
- **`?` on user-defined `Try`-like types** (a `Try` trait). Deferred with the
  same trait work.
- **`?` in top-level `const` initializers**, if such initializers ever gain
  control-flow context.

## Why these choices

- **Statement-level ANF hoist over a dedicated node:** it *reuses* the existing
  statement-`match` lowering on every backend instead of re-implementing
  conditional early return three-plus times, and it preserves the core IR
  invariant that no expression hides a non-local return. This is the same
  "erase to existing constructs, keep backends untouched" philosophy that made
  generics free ([[generics_design]]) and enums uniform ([[enum_and_match_design]]).
- **Exact `E` match, no implicit conversion (yet):** keeps the first increment
  sound and simple, deferring the conversion-trait machinery to the trait
  ticket exactly as generics deferred trait bounds.
- **Postfix, highest precedence:** matches reader intuition (`f()?` reads as
  "call, then propagate") and composes cleanly with call/index/field in one
  postfix loop — zero new precedence tiers, and chaining (`f(g()?)?`) falls out
  for free.
- **Fully erased, no runtime value:** the dynamically-typed runtime needs no new
  `Value`; `?` is a pure front-end transform, so AST/IR/bytecode/optimized
  backends stay at parity by construction.
