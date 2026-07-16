# Lullaby — User-Defined Generic Types: Implementation-Strategy Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

**Status:** Design spike (implementation strategy, NOT implementation). Requested
for **road_to_1_0_stable A1** — owner DECIDED (2026-07-15) that user-defined
generic types ship in 1.0. This document is the buildable plan; it does not edit
any `.rs`.

**Scope.** How `struct Stack<T>` / `enum Opt<T>` should parse, type-check, lower,
and reclaim across Lullaby's five execution paths (AST tree-walker, IR
interpreter, bytecode VM, native x86-64, WASM). It references — does not restate —
the memory model in
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md), the type
rules in [lullaby_type_system.md](lullaby_type_system.md), and the indentation
rules in [core_language_rules.md](core_language_rules.md).

---

## 1. The verified gap

- **Bounded generic *functions* work** end-to-end: `fn identity<T> x T -> T`,
  `<T: Trait>` / `<T: A + B>` bounds, call-site type inference. The machinery is
  in `crates/lullaby_parser/src/lib.rs` (`parse_type_params`) and
  `crates/lullaby_semantics/src/semantics_generics.rs` (`unify_param`,
  `substitute_type`, `infer_generic_return`).
- **Built-in collections are generic**: `list<T>`, `map<K,V>`, `array<T>`,
  `option<T>`, `result<T,E>`, `rc<T>`, `ref<T>`, `ptr<T>` — all lower on every
  backend today.
- **User-defined generic *types* do not parse.** `struct Stack<T>` and
  `enum Opt<T>` both fail with **L0205** ("expected structural token"). Root
  cause is mechanical: `parse_struct` / `parse_enum`
  (`crates/lullaby_parser/src/lib.rs:434`, `:406`) read the type name with
  `expect_identifier` and then immediately `expect_newline`; the `<` after the
  name is unexpected. `StructDecl` / `EnumDecl` (`ast.rs:66`, `:86`) have **no
  `type_params` field** — unlike `Function`, which has `type_params: Vec<TypeParam>`
  (`ast.rs:125`).

This design closes that gap.

---

## 2. Surface syntax (indentation-only)

The syntax is the natural extension of the existing generic-function surface:
type parameters in `<...>` immediately after the declared name, bounds with `:`
and `+`, no braces, no semicolons, fields/variants declared with the existing
`name Type` form.

### 2.1 Generic struct

```lullaby
struct Stack<T>
    items list<T>
    count i64

struct Pair<K, V>
    key K
    value V
```

`T` is an ordinary type inside the body: as a bare field type (`value V`), nested
inside a built-in generic (`items list<T>`), or inside another user generic once
those exist (`inner Box<T>`).

### 2.2 Generic enum

```lullaby
enum Opt<T>
    present T
    absent

enum Tree<T>
    leaf T
    node rc<Tree<T>>          # recursion goes through an indirection (see 5.4)
```

Variant payloads use the existing space-separated payload-type syntax
(`parse_enum`, `ast.rs` `EnumVariant.payload: Vec<TypeRef>`); each payload type
may mention the type parameters.

### 2.3 Bounds on a generic type

Reuses the function bound grammar exactly (`parse_type_params` already parses
`T: Trait + Other`):

```lullaby
trait Compare
    fn less self, other Self -> bool

struct SortedList<T: Compare>
    items list<T>
```

A `<T: Compare>` type bound means: inside the type's methods (and any function
generic over the same `T`) a value of type `T` may call `Compare`'s methods —
same rule as bounded generic functions today
([lullaby_type_system.md](lullaby_type_system.md) "Generics and Bounds").

### 2.4 Instantiation and construction

A concrete instantiation is the type name with concrete arguments, spelled the
same way built-in generics are spelled (`list<i64>`):

```lullaby
let s Stack<i64> = Stack(items: [], count: 0)     # explicit annotation pins T = i64
let p = Pair(key: "id", value: 42)                # K,V inferred from field args: Pair<string, i64>
```

- **Annotation-directed:** when the binding (or parameter, or return) carries a
  concrete `Stack<i64>` annotation, that pins the type arguments.
- **Argument-directed inference:** when a constructor's arguments pin every type
  parameter (`Pair("id", 42)`), the arguments determine the type — reusing the
  same `unify_param` engine that infers generic-function type arguments today.
- **Empty / unpinnable construction needs an annotation**, exactly mirroring the
  existing empty-array rule ("an empty array … cannot provide an inferred type",
  [lullaby_type_system.md](lullaby_type_system.md)). `let s = Stack(items: [],
  count: 0)` with no annotation is rejected (a new `L03xx`, see §3.5/§4.5) because
  `T` is unresolved.

**Turbofish (explicit type args at the construction site, e.g. `Stack<i64>()`) is
an OWNER DECISION — see §8.** The recommendation is to allow the *type spelling*
`Stack<i64>` in annotations (required — it is how you name the type) and to rely
on annotation/argument inference at construction, **not** to add a distinct
turbofish call form initially. This keeps one inference story and avoids a second
angle-bracket parse context in expression position.

### 2.5 Composition with existing generics

- A user generic can hold built-in generics (`items list<T>`, `slot option<T>`,
  `cell rc<T>`) and vice-versa (`list<Stack<i64>>`).
- A generic *function* can be generic over, take, and return a user generic:
  `fn peek<T> s Stack<T> -> option<T>`. The function's `T` and the type's `T`
  unify through the same substitution map.

---

## 3. Core strategy decision — monomorphization vs erasure vs hybrid

This is the load-bearing call. The analysis is specific to Lullaby's shape:
**value semantics**, **arena-first + RC memory**, a **native x86-64 layout-driven
codegen path**, and **three dynamic-`Value` interpreters**.

### 3.1 The two representations already in the tree

The codebase already contains both ends of the spectrum, which is why a hybrid is
not a compromise but a match to reality:

- **Interpreters are already type-erased.** The runtime `Value`
  (`crates/lullaby_runtime/src/lib.rs:591`) is a dynamic tagged union:
  `Struct(Box<StructValue>)` holds field `Value`s, `Enum(Box<EnumValue>)` holds a
  tag + payload `Value`s, `List`/`Map` hold dynamic elements. A struct's fields
  carry whatever runtime values they were given; **the interpreter never needed
  static layout to run a struct.** A generic `Stack<T>` is, at runtime, just a
  `StructValue` whose `items` field is a `List` of whatever. No per-`T` copy is
  required for the interpreters to be correct.
- **Native is already layout-driven and monomorphic-by-spelling.** `NativeType`
  (`crates/lullaby_ir/src/native_object.rs`, ~line 2500+) computes a concrete
  layout per concrete type spelling: `Struct` field offsets,
  `Enum { payload_words }` sized to the widest variant, `List`/`Map` element word
  counts. Aggregates cross function boundaries **by pointer**; a heap value
  (string/list/map/heap-struct/`rc`) is a **single pointer word**, an inline
  scalar is its own width. Deep-copy and reclamation glue (`__lullaby_*` helpers
  in `.text`) are emitted against a *known* layout. Native fundamentally needs to
  know, for each field, **is this a pointer word to reclaim or an inline scalar** —
  and that is exactly what a type parameter hides.

### 3.2 Option A — Pure type erasure (uniform boxed representation everywhere)

Represent every generic field through one uniform machine representation (a
tagged word / boxed pointer + runtime type info), one compiled copy of the type
regardless of `T`.

- **Pro:** one compiled body per generic type → minimal code size, minimal
  incremental compile time, trivially matches the interpreters.
- **Con (native, disqualifying):** it destroys the zero-cost, layout-driven
  native model. A `Stack<i64>` field could no longer be an inline `i64` — it would
  become a boxed/tagged word, forcing heap traffic and runtime type dispatch on
  what is today register-resident arithmetic. It also **breaks arena/RC
  reclamation soundness**: to reclaim a `T` field the emitter must know whether
  `T` is heap-typed; erasure hides that behind runtime type info, meaning every
  generic aggregate would need a per-value RTTI walk to drop correctly — precisely
  the "hidden control flow" the freestanding tier forbids
  ([execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md)). Native
  performance regresses and the memory model gets more complex, not less.

### 3.3 Option B — Pure monomorphization (specialize every instantiation on every backend)

Generate a distinct concrete type (and distinct code) for every distinct
`(GenericDecl, type-args)` reached in the program, on **all five** backends.

- **Pro:** uniform pipeline; every backend sees only concrete
  `struct Stack$i64` / `struct Stack$string` defs, so no backend needs a notion of
  a type variable past the front-end. Best native performance and the only sound
  basis for per-instantiation reclamation (§5).
- **Con:** the interpreters gain nothing from it and pay for it — they would
  compile and hold N near-identical specialized defs where one erased def already
  runs correctly on dynamic `Value`s. More importantly, doing full monomorphized
  codegen on every backend risks the **~7 ms fast-compile edge** if instantiation
  counts grow, for zero interpreter benefit. It over-specializes the cheap path to
  serve the expensive one.

### 3.4 Option C — Hybrid (RECOMMENDED): shared generic front-end, erasure on interpreters, monomorphization on native/WASM

One front-end validates the generic type once; the back half splits by what each
target actually needs.

- **Parser + semantics** are generic-aware **once**: they carry `type_params`,
  scope the parameters, substitute, check bounds, and infer instantiation type
  arguments — all at the type level, producing a fully-typed AST/IR in which every
  *use site* records its concrete instantiation spelling (`Stack<i64>`, already
  the natural `TypeRef` string form, §4.2).
- **Interpreters (AST, IR, bytecode) use erasure.** They keep a **single**
  `StructDef`/`EnumDef` per generic type and run it against dynamic `Value`s.
  `Stack<i64>` and `Stack<string>` share one runtime shape; the static checker
  already guaranteed each use is well-typed, so the interpreter needs no per-`T`
  copy. **Near-zero interpreter work** beyond threading the decl through.
- **Native + WASM use monomorphization.** A **collection pass** walks the typed
  program, gathers the finite set of concrete instantiations actually reachable,
  and emits one specialized concrete `struct`/`enum` def per unique instantiation
  (mangled, §4.4). The **existing** `NativeType`/WASM layout machinery then
  consumes each specialized def as an ordinary concrete type — including exact
  field offsets, enum payload sizing, deep-copy, and arena/RC drop glue. Native
  stays zero-cost; reclamation stays sound (§5).

**Why hybrid is the right call for Lullaby (one paragraph).** The codebase already
*is* hybrid: the interpreters are dynamically typed (`Value` tags carry all the
structure they need, so erasure is literally free there), while native is a
layout-and-reclamation machine that cannot be sound or zero-cost without knowing
each field's concrete type. Forcing one strategy onto both either wrecks native
(erasure — boxing, RTTI drops, hidden control flow the kernel tier bans) or taxes
the interpreters and the fast-compile budget for nothing (monomorphize
everywhere). Hybrid pays the monomorphization cost **only where layout matters and
only for reachable instantiations**, keeps the interpreters on the free erased
path, and reuses two mechanisms that already exist — `unify_param`/`substitute_type`
for the type math and `NativeType` layout for the specialized defs. It is the
lowest-complexity plan that keeps native zero-cost, keeps reclamation provably
sound, and protects the compile-speed moat. **Recommendation: adopt Option C.**
This is an implementation-strategy call, not an identity fork — treated as decided
unless the owner overrides.

### 3.5 Cost control (protecting the ~7 ms edge)

Monomorphization blow-up is the one real compile-time risk. Controls:

- **Reachable-only, deduplicated.** Specialize only instantiations that actually
  appear; key by the canonical concrete spelling (`Stack<i64>`), which is already a
  plain string, so dedup is a hash-set insert. This is exactly how the built-in
  generics already avoid re-emitting `list<i64>` layouts.
- **Native/WASM only.** Interpreter builds (the common `lby run` inner loop) never
  pay monomorphization at all.
- **Bounded depth diagnostic.** A recursion/instantiation-depth guard emits a
  clear `L03xx` if a generic instantiation set fails to close (pathological
  `Foo<Foo<Foo<...>>>` growth), rather than looping the compiler.

---

## 4. Semantic model

### 4.1 Type-parameter scoping

- `StructDecl` and `EnumDecl` gain `type_params: Vec<TypeParam>` (same type the
  `Function` node already uses; `TypeParam { name, bounds }`, `ast.rs:178`). Serde
  default to an empty vec so existing single-file artifacts / AST snapshots stay
  valid (the same backward-compat pattern already used for `is_public`).
- During checking of a generic type's body and its methods, the type parameters
  are in scope as opaque types, registered the same way a generic function
  registers its `type_params`. The existing bare-`T` rule holds: a `T` value
  supports only universal operations (bind/pass/return/`==`) unless a bound
  supplies more ([lullaby_type_system.md](lullaby_type_system.md), `L0327`).

### 4.2 Instantiation spelling (no new type representation)

`TypeRef` is a string with `ctor<args>` conventions and nesting-aware
`generic_args` / `split_generic_args` helpers (`ast.rs:225+`). A user
instantiation `Stack<i64>` is **already representable** as
`TypeRef::new("Stack<i64>")` — no new data type. This is the key leverage point:
user generics reuse the exact spelling machinery the built-ins use.

`decompose_generic` in `semantics_generics.rs:33` currently recognizes only the
built-in constructors (`array`, `list`, `option`, `result`, `map`, `ptr`, `ref`,
`rc`, and function types). It gains a case: **if the head name resolves to a
declared generic type, decompose it into `(name, args)`** so `unify_param` /
`substitute_type` / `first_unresolved_type_var` recurse through user generics for
free.

### 4.3 Substitution and type-checking a use

- **Field/variant type resolution:** to type-check `s.value` for `s Pair<string,
  i64>`, substitute the declaration's type parameters (`[K, V]`) with the
  instantiation's arguments (`[string, i64]`) using the existing `substitute_type`,
  yielding the concrete field type. This is the same substitution `substitute_self`
  already does for `Self` in trait impls (`semantics_generics.rs:169`).
- **Construction checking:** unify each supplied field/payload argument type
  against the declared (parameterized) field/payload type via `unify_param`,
  building the type-argument map; then verify no parameter is left unresolved
  (`first_unresolved_type_var`) → otherwise the empty/unpinnable error (§2.4).
  Conflicting bindings reuse the existing conflict diagnostic path (`L0395`
  analog).
- **Method dispatch:** methods on a generic type (`impl` blocks / UFCS) resolve
  with the receiver's concrete instantiation substituted in, reusing receiver-type
  dispatch that traits already use.
  - **DELIVERED on native (A1).** Inherent-method dispatch is now compiled to
    native x86-64 (`crates/lullaby_ir/src/native_object_method.rs`). A source
    `recv.method(args)` reaches the backend as an ordinary UFCS `Call { name,
    args: [recv, ...] }` (the method bodies live in `BytecodeModule::impls`, keyed
    by `(base_type, method)`; the receiver-dispatched names are
    `BytecodeModule::trait_methods`). A native pre-pass expands each call whose
    receiver resolves to a concrete user struct/enum — including a monomorphized
    generic instantiation (`Box<i64>`, `Opt<i64>`) — into a direct call to a
    synthesized instance function appended to `functions`, with the method body
    monomorphized by `substitute_type` (`{T: i64}`) exactly as generic-type field
    layouts already are. `self` is passed by the existing aggregate ABI (hidden
    pointer / copy-in), so the interpreters' by-value `self` (each call clones the
    receiver `Value`) is matched bit-for-bit — mutating `self` cannot affect the
    caller. **Default-deny:** a receiver that is a bare type parameter `T`, a
    dynamic/trait-object receiver, or an instance whose monomorphized
    receiver/param/return layout is outside the native subset (a deeper-than-one-
    level heap receiver, an out-of-subset param/return) is left untouched and skips
    cleanly through the fixpoint (`L0339`) — never miscompiled. Proven by the
    `native_object_method` unit tests, the `generics/methods.lby` link-and-run
    (exit 151), and the `fuzz_method_*` differential fuzzers (2000 interpreter +
    120 native link-and-run programs).

### 4.4 Name mangling for monomorphized defs (native/WASM only)

A specialized def gets a deterministic mangled name derived from the canonical
instantiation spelling, e.g. `Stack<i64>` → `Stack$i64`, `Pair<string,i64>` →
`Pair$string$i64`, nested `Stack<list<i64>>` → a stable encoding of the nested
spelling. Mangling is confined to the native/WASM lowering; source, diagnostics,
and interpreters always show/handle the human spelling `Stack<i64>`. The delivered
native **method** instances follow the same principle with a `$mth$` prefix that
cannot collide with any source identifier: `Box<i64>::peek` →
`$mth$Box_i64_$peek`, `Counter::bump` → `$mth$Counter$bump` (non-identifier
characters in the receiver spelling are sanitized to `_`, keeping distinct
instantiations distinct).

### 4.5 New diagnostics (numbers assigned in `diagnostic_registry.md` at build time)

- Unresolved type parameter at construction (empty/unpinnable) — a new `L03xx`.
- Wrong type-argument arity (`Stack<i64, i64>` for a one-parameter `Stack`) — a
  new `L03xx`.
- Bound not satisfied by a type argument (`SortedList<T>` where `T` lacks
  `Compare`) — reuse/extend the existing bound-check family used for function
  bounds.
- Instantiation-depth/closure guard (§3.5) — a new `L03xx`.
- Illegal non-indirected recursive generic (§5.4) — a new `L03xx`.

Existing generic-function codes (`L0394` type-param decl errors, `L0395`
conflict, `L0396` unresolved, `L0327` bare-`T` operation) are reused unchanged
where they apply.

---

## 5. Memory-model interaction (the reclamation-soundness core)

This is where monomorphization on native earns its place, and it ties directly
into the arena-first + RC model
([execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md)) and the
native-aggregate work.

### 5.1 The problem a type parameter creates

Reclaiming a value means: for each field, if it owns heap memory, run the right
drop/reset; if it is an inline scalar, do nothing. Native already distinguishes
these — a heap field is a single pointer word, a scalar is inline. But a field of
type `T` is **neither until `T` is known**: `Stack<i64>.items` (a `list<i64>`) is a
heap word to reclaim; `Pair<i64,i64>` fields are inline scalars needing no drop.
Under erasure the emitter cannot tell them apart without per-value RTTI — a
runtime walk on every drop, and *hidden control flow* the freestanding tier
forbids.

### 5.2 How monomorphization makes it sound and static

At each concrete instantiation the type arguments are fully known, so the
specialized `NativeType::Struct` / `NativeType::Enum` records, **per field/variant
payload, the concrete `NativeType`** — hence statically whether it is a pointer
word or inline scalar. Native then emits **per-instantiation drop/deep-copy glue**
exactly as it already does for concrete structs:

- `Stack$i64`: `items` is a `List{i64}` pointer word → its drop glue reclaims the
  list block; `count` is inline `i64` → skipped.
- `Pair$i64$i64`: both fields inline → the whole value is a flat, trivially-copied,
  trivially-dropped aggregate (no heap traffic at all).
- `Stack$string`: `items` is a `List{string}` → its element drop recurses into
  the string words, all statically emitted.

No RTTI, no hidden branches, no per-value type test — each specialized type has
its own straight-line reclamation code, which is the invariant the native
aggregate + arena work already relies on.

### 5.3 Arena vs RC for generic fields

Placement is unchanged by genericity because it is decided *after*
monomorphization, on the concrete field types:

- **Arena (default/primary):** a monomorphized generic aggregate that provably
  does not escape its region is bump-allocated and bulk-reset with the enclosing
  scope, like any concrete aggregate. Its heap `T` fields' backing blocks live in
  the same region and are reclaimed by the same bulk reset — no per-object
  refcount traffic.
- **RC (`ref`, secondary/opt-in):** when a generic value escapes to shared/dynamic
  ownership, the `T` field that is itself heap-typed participates in the same
  refcount drop-glue the concrete case uses. Because the instantiation is known,
  the emitter knows *whether* there is anything to refcount.
- **Freestanding tier:** since monomorphization removes all type variables before
  codegen and injects no RTTI/hidden allocation, generic types with scalar/arena
  `T` are usable in `no-runtime` code; `ref`/RC-bearing instantiations remain
  unavailable there exactly as non-generic `ref` is.

### 5.4 Recursive generic types

`enum Tree<T>` with a `node Tree<T>` payload is infinite-size by value. As with
any value-semantic recursive type, recursion must pass through an **indirection**
(`rc<Tree<T>>`, `list<Tree<T>>`, `ptr<Tree<T>>`, or a heap struct). A direct
non-indirected recursive payload is rejected with a clear `L03xx` (rather than
diverging during layout/monomorphization). This also bounds the instantiation set
so the collection pass terminates (§3.5).

---

## 6. Per-backend lowering sketch

### 6.1 Parser (`crates/lullaby_parser`)

- Add `type_params: Vec<TypeParam>` to `StructDecl` and `EnumDecl` (`ast.rs`,
  serde-defaulted empty).
- In `parse_struct` / `parse_enum`, after `expect_identifier(name)` and **before**
  `expect_newline`, call the **existing** `parse_type_params(span)` helper (it
  already parses `<T>`, `<T: Trait + Other>`, duplicate/shadow checks, `L0394`).
  This single insertion is what turns today's `L0205` into a parsed generic decl.
- No new expression grammar (annotations already accept `Stack<i64>` spellings via
  `expect_type`). Turbofish-in-expression is explicitly *not* added (§2.4/§8).

### 6.2 Semantics (`crates/lullaby_semantics`)

- Register generic type declarations with their `type_params`; scope parameters
  over fields/variants/methods.
- Extend `decompose_generic` (`semantics_generics.rs`) with the user-generic-type
  case so `unify_param`/`substitute_type`/`first_unresolved_type_var` recurse
  through user instantiations.
- Construction, field access, `match`, and method dispatch resolve concrete field/
  payload types via `substitute_type`. Enforce arity, bounds, and
  unresolved-parameter diagnostics (§4.5).

### 6.3 IR (`crates/lullaby_ir`)

- Carry `type_params` on the IR struct/enum defs and the concrete instantiation
  spelling on every use site (already string-encoded).
- Add the **monomorphization collection pass** (native/WASM lowering input only):
  gather reachable `(def, type-args)`, dedup by canonical spelling, emit one
  specialized concrete `IrStructDef`/`IrEnumDef` per instantiation with substituted
  concrete field/payload types and a mangled name (§4.4). Interpreters bypass this
  pass.

### 6.4 Three interpreters (AST tree-walker, `ir_interpreter.rs`, `bytecode_vm.rs`)

- **Erasure:** keep one decl per generic type. `StructValue`/`EnumValue` already
  store dynamic field/payload `Value`s, so construction, `.field`, `match`, and
  methods work with the generic decl unchanged. The main work is threading the
  decl (and its already-checked types) through; no new runtime representation.

### 6.5 Native (`native_object.rs`, `native_object_stmt.rs`) and WASM (`wasm_lowering.rs`)

- Consume the **specialized** concrete defs from §6.3. Each specialized def flows
  through the existing `NativeType` resolver (`Struct` offsets,
  `Enum { payload_words }`, `List`/`Map` element widths), the existing
  aggregate-by-pointer ABI, and the existing deep-copy / arena / RC drop-glue
  emitters. Because every specialized def is fully concrete, **no native/WASM code
  needs a concept of a type variable.**
- `enum_ctor_name` / `is_enum_type_name` (which already split on `<`) generalize to
  user generic enums via the mangled concrete names.

---

## 7. Staged, production-complete increment plan

Each increment is independently shippable, fully tested (positive + negative +
differential fuzzer + the affected fixtures), and documented — never a shallow
slice of the whole. Ordering follows the memory-model staging philosophy in
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md): scalar/arena
first, heap `T` next, then breadth.

1. **Single-parameter generic `struct` with a scalar `T`.** Parser
   `type_params` + `parse_type_params` insertion; semantics scoping/substitution/
   inference; interpreter erasure; native/WASM monomorphization for the flat,
   all-inline case (`Pair<i64,i64>`, `Box<i64>`). Proves the whole pipeline on the
   case with **no** reclamation subtlety (trivially copied/dropped). Verified by a
   value-neutral layout fixture + fuzzer.

2. **Heap `T` (the reclamation increment).** `T` may be `string`/`list`/`map`/heap
   struct/`rc`. Exercises §5.2 per-instantiation drop/deep-copy glue and §5.3
   arena vs RC placement. Verified by bounded-heap reclaim fixtures under both
   arena and RC paths + fuzzer (no leak, no double-free, value semantics
   preserved).

3. **Generic `enum`.** Parameterized variants, exhaustive `match` with payload
   binding, `payload_words` sizing across variants for the monomorphized layout.
   Includes the recursive-generic indirection rule + its diagnostic (§5.4).

4. **Methods / `impl` on generic types.** `impl Stack<T>` methods (push/pop/peek)
   so containers are actually usable; receiver-type dispatch with the instantiation
   substituted. This is what makes the feature earn its 1.0 place (a data-structure
   library).

5. **Multi-parameter + bounds.** `Pair<K,V>`, `map`-like types, and `<T: Trait>`
   type bounds enforced (bound-satisfaction diagnostic), including bound-method
   calls inside generic-type methods.

(Each stage updates `lullaby_type_system.md`, `language_specification.md` /
`language_surface.md`, `diagnostic_registry.md`, and `repository_map.md` in the
same change — per the doc rules — but **that documentation work is out of scope
for this design spike**, which creates only this file.)

---

## 8. Open questions / risks

- **Monomorphization compile-time budget (risk).** The ~7 ms edge is the thing to
  protect. Mitigations in §3.5 (native/WASM-only, reachable-only, dedup by
  spelling, depth guard). Must be measured against the benchmark harness as stage
  2+ lands; if a pathological instantiation fan-out appears in real code, consider
  a shared-layout fallback for all-pointer-word instantiations (they share a
  layout) as a later optimization — **not** in the initial plan.
- **Type-argument inference completeness.** The `unify_param` engine infers from
  arguments today; constructor inference reuses it, but the empty/unpinnable case
  needs the annotation rule (§2.4). Confirm no surprising inference gaps vs.
  named-vs-positional construction.
- **Nested-instantiation mangling collisions.** The mangled-name scheme (§4.4)
  must be injective over nested spellings (`Stack<list<i64>>` vs a hypothetical
  user `Stack<list$i64>`); use a length-prefixed or delimiter-escaped encoding.
- **Interaction with the not-yet-built freestanding/actor tiers.** Generic types
  with scalar/arena `T` should be freestanding-clean (no RTTI/hidden alloc — a
  property monomorphization gives us); confirm when the `no-runtime` gate lands.
- **`ref`/RC drop-glue reuse.** Stage 2 leans on the RC drop-insertion machinery;
  confirm the generic field case needs no new drop-edge logic beyond substituting
  the concrete field type.

### OWNER DECISION NEEDED (identity-shaping forks)

1. **Construction type-argument syntax (turbofish).** Do we ever want an explicit
   `Stack<i64>(...)` / `Stack::<i64>(...)` call form, or is annotation +
   argument-inference (with an annotation required for empty/unpinnable
   construction) the *only* way to fix type arguments? **Recommendation:**
   inference-only initially (no turbofish); annotations already carry the
   `Stack<i64>` spelling. This is a small, reversible surface-syntax identity call
   — flagged because it shapes how every container is constructed.

2. **Recursive-generic policy.** Confirm the rule "recursion must pass through an
   indirection (`rc`/`list`/`ptr`/heap struct), else a hard diagnostic" (§5.4).
   **Recommendation:** adopt it — it matches value semantics and bounds
   monomorphization. Flagged because it constrains how users express tree/graph
   types.

All other choices in this document (hybrid strategy, mangling, staging, diagnostic
placement) are treated as implementation-strategy calls and do not need owner
sign-off.

---

**Bottom line:** hybrid — **erasure on the three interpreters, monomorphization on
native/WASM**, over one generic-aware front-end — is the plan. It reuses the type
math (`unify_param`/`substitute_type`) and the native layout/reclamation machinery
already in the tree, keeps native zero-cost and its reclamation provably sound,
leaves the interpreters on the free erased path, and confines the only real cost
(monomorphized codegen) to reachable instantiations on the backends that need
layout — protecting the fast-compile edge.
