# Standard-Library Module Layout and the Primitive/Module Boundary

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This note is the **1.0 honesty pass** for Lullaby's standard library. Today every
built-in type and function in [standard_library.md](standard_library.md) is
*always in scope* — a single compiler-provided prelude that includes not only the
scalars, collections, memory intrinsics, and core I/O that a systems language
must expose, but also full "batteries" like an HTTP/1.1 client and server helper,
the string-convenience library, and math beyond core arithmetic. That prelude is
honest today, but at 1.0 it over-claims: it says "these are the
primitives" while shipping a web client among them.

The design here draws the line between **true language primitives** (which stay in
the always-in-scope prelude) and **stdlib modules** (opt-in batteries imported on
top of the existing flat `import NAME` + `pub` system from
[modules_design.md](modules_design.md)). It then specifies the module mechanics,
a non-breaking migration for the HTTP builtins, the on-disk and installed-bundle
layout, diagnostics, and a production-complete first increment that establishes
the layout and moves **one** battery as the proof.

Related design: [modules_design.md](modules_design.md) (import/`pub`/manifest
mechanics this builds on), [standard_library.md](standard_library.md) (the current
flat prelude catalog), [stdlib_io_boundary.md](stdlib_io_boundary.md) (the
existing intrinsic-vs-runtime I/O boundary this note generalizes), and
[roadmap.md](roadmap.md) (packaging/installer sequencing to
coordinate with). ClickUp: list **"11 Standard Library and Collections"** owns the
prelude/module classification and the string/math/collection work; list
**"12 Modules and Packaging"** owns the module-resolution, namespacing, and
bundle-layout work.

## The principle: "most primitive core, batteries as modules"

Lullaby's core promise is a small, honest, always-available primitive surface a
systems programmer can rely on without ceremony — and everything larger reachable
by one `import`. The test for whether something stays in the prelude:

1. **Irreducible.** It cannot be written in Lullaby on top of a smaller
   primitive (e.g. `alloc`, `tcp_read`, `+`, `len`), OR it is a scalar/type the
   type system itself is defined in terms of.
2. **Universal.** Essentially every non-trivial program needs it, so importing it
   would be pure ceremony (e.g. `println`, `to_string`, `list`/`map`).
3. **Backend-defining.** Its representation is part of the value model the three
   backends and the WASM/native lowerers already agree on (scalars, `struct`,
   `enum`, `option`/`result`, references), so it cannot live "outside" the core.

A "battery" fails (1) or (2): it is **implementable in Lullaby over the
primitives** and is **not needed by every program**. HTTP is the archetype — it
is already demonstrably pure-Lullaby-over-TCP (`examples/valid/http_server/http.lby`
is a `pub` module written entirely on the socket builtins). The string-convenience
library and transcendental/extended math are the next clearest batteries: a
program that never touches text or trigonometry pays nothing to keep them out of
its namespace.

## Classification: primitives vs. stdlib modules

The table below reclassifies the entire current prelude. "Prelude" = stays
always-in-scope. A module name in the last column = moves to an opt-in stdlib
module (`import NAME`). Rows are grouped by the current
[standard_library.md](standard_library.md) sections.

| Surface (current prelude) | Classification | Rationale | Target |
|---|---|---|---|
| `i64` `f64` `bool` `string` `char` `byte` `void` | **Primitive** | The type system is defined over these; scalars are backend-defining. | prelude |
| `array<T>` `struct` `enum` `fn(...) -> R` | **Primitive** | Core value/type constructors; lowered by WASM/native. | prelude |
| `option<T>` `result<T, E>` | **Primitive** | Built-in generic enums with context inference; the error/absence model. | prelude |
| `list<T>` `map<K,V>` and their builtins | **Primitive** | Universal data structures; runtime-backed, not writable in-language. | prelude |
| `rc<T>` `ref<T>` `ptr<T>` + rc/ref/ptr builtins | **Primitive** | Memory model; irreducible + backend-defining. | prelude |
| `alloc` `load` `store` `dealloc` | **Primitive** | Heap intrinsics; irreducible. | prelude |
| `to_string` `char_code` `char_from` `byte` `byte_val` | **Primitive** | Irreducible conversions across scalar reprs; near-universal. | prelude |
| `len` | **Primitive** | Intrinsic over `string`/`array`/`list`. | prelude |
| `print` `println` `warn` `flush` | **Primitive (core I/O)** | Irreducible stream intrinsics; near-universal. | prelude |
| `read_file` `write_file` `append_file` `file_exists` and the byte/line/dir FS builtins | **Primitive (core I/O)** | Irreducible OS intrinsics (cannot be written in-language). See [stdlib_io_boundary.md](stdlib_io_boundary.md). | prelude |
| `env` `args` `sys_status` `sys_output` | **Primitive (core I/O)** | Irreducible process/OS intrinsics. | prelude |
| `wasm_log` `console_log` `dom_set_text` | **Primitive (host interop)** | Backend-lowered host imports; not expressible in-language. | prelude |
| `assert` + `throw`/`try`/`catch` | **Primitive** | Error-control forms wired into the runtime. | prelude |
| TCP builtins (`tcp_connect`/`listen`/`accept`/`read`/`write`/`shutdown`/`close`) | **Primitive (I/O intrinsic)** | Irreducible socket syscalls; the substrate HTTP is built on. | prelude |
| UDP builtins (`udp_bind`/`send_to`/`recv`) | **Primitive (I/O intrinsic)** | Same as TCP — the OS datagram substrate. | prelude |
| `chan_new`/`send`/`recv`/`try_recv`/`spawn`/`task_join`/`mutex_*`/`parallel_map` | **Primitive** | Concurrency intrinsics wired to OS threads/`mpsc`; irreducible. | prelude |
| Core arithmetic on `i64`/`f64` (`+ - * /`, comparisons) and `abs` `min` `max` `pow` `sqrt` `floor` `ceil` `round` | **Primitive (core math)** | Operators are syntax; the eight functions are the arithmetic floor a systems core is expected to have. | prelude |
| **HTTP client** `http_get` `http_post` | **Battery → module** | Pure-Lullaby-over-TCP; not universal. | **`http`** |
| **String conveniences** `substring` `find` `contains` `split` `join` `trim` `replace` `upper` `lower` `starts_with` `ends_with` `repeat` | **Battery → module** | Implementable over `char`/`byte`/`+`/`len`; not every program needs them. `+` string concat stays a prelude operator. | **`strings`** |
| **Extended math** `sin` `cos` `tan` `atan` `atan2` `exp` `ln` `log10` | **Battery → module** | Transcendentals beyond the arithmetic floor; not universal. | **`math`** |

Notes on the boundary calls:

- **TCP/UDP stay primitives.** They are irreducible syscall wrappers — you cannot
  write `tcp_read` in Lullaby. "Higher-level networking" (connection pools,
  URL/host helpers, a request abstraction) is exactly what belongs in modules and
  is where `http` lives. So the honesty pass reclassifies **HTTP**, not the
  sockets under it.
- **String `+` concatenation stays** a prelude operator (it is core syntax over
  the `string` primitive); only the *named convenience functions* move to
  `strings`.
- **The eight core-math functions stay.** `abs`/`min`/`max`/`pow` and
  `sqrt`/`floor`/`ceil`/`round` are the arithmetic floor; only the eight
  transcendentals move to `math`. This keeps numeric code that never calls `sin`
  free of an import while still shrinking the prelude.
- **Concurrency stays primitive** for 1.0: it is thread/`mpsc`-backed intrinsics,
  not in-language code. A future `sync`/`threads` module could re-export
  higher-level patterns, but that is out of scope here.

## Module mechanics for the standard library

Stdlib modules reuse the **existing** flat `import NAME` + `pub` system from
[modules_design.md](modules_design.md) verbatim — no new import token, no new
visibility rule. What is new is *where the compiler looks* for a stdlib name and
*how* those modules ship.

### Naming and import

A stdlib module is imported exactly like a user module:

```lby
import strings
import math
import http

fn main -> i64
    let parts array<string> = split("a,b,c", ",")   # from strings
    let angle f64 = sin(1.5708)                       # from math
    let body result<string, string> = http_get("http://example.com")  # from http
    len(parts)
```

The names stay **unqualified** after import (matching the current flat-namespace
choice), so a migrated program adds one `import` line and is otherwise byte-for-
byte the same source. `http_get` remains `http_get`; `split` remains `split`.

### How stdlib modules ship: source-first, with an intrinsic escape hatch

Two kinds of stdlib module exist, distinguished by whether their bodies are
expressible in Lullaby:

1. **Source modules (`.lby`, shipped in the toolchain).** `http`, `strings` (once
   its primitives exist), and any future pure-Lullaby battery are ordinary `.lby`
   files with `pub` functions, shipped in the toolchain bundle and loaded through
   the *same* loader path as user modules. `http` is the flagship: it is already
   written (`examples/valid/http_server/http.lby`) and only needs to move into the
   shipped stdlib tree. Source modules are the default and the honest ones — their
   implementation is readable Lullaby.
2. **Intrinsic modules (compiler-provided facade).** Where a battery's body is
   *not yet* expressible in Lullaby but should still be import-gated for honesty
   (extended `math` transcendentals lower to host `f64` intrinsics; the initial
   `strings` conveniences are runtime-backed), the module is a thin
   compiler-provided **facade**: importing it makes a fixed set of builtin names
   resolvable, but *not importing it* leaves those names unresolved (an ordinary
   `L0309` unknown-function). The facade is a compiler manifest — a list of which
   builtins each stdlib module "owns" — not new runtime code. As the language
   grows (`byte`/`char` string ops, an `f64` polynomial core), an intrinsic module
   can be reimplemented as a source module with no user-visible change.

Both kinds resolve through one **stdlib search root** appended after the user's
own search directories (below), so a user module named `strings.lby` in the
program's own tree still shadows the stdlib per the existing resolution order —
and a duplicate is caught by the existing no-shadowing rule (`L0391`).

### The prelude, defined explicitly

Today "the prelude" is implicit — every builtin the semantic analyzer knows. At
1.0 the prelude becomes an **explicit compiler manifest**: the set of builtin
names and types that are in scope with no import. Reclassifying a battery is then
a single change — moving a name from the prelude manifest to a stdlib module's
owned-name set. The manifest is the machine-checkable source of truth behind the
[standard_library.md](standard_library.md) catalog, and the `strings`/`math`/`http`
owned-name sets are its module counterparts.

Opting *into* a non-prelude module is exactly `import NAME`. Opting *out* is the
default. There is no "opt out of the prelude" — the prelude is, by construction,
the minimal set no program should have to name.

### Namespacing at scale

The current flat unqualified namespace is fine for a handful of user modules but
scales poorly as dozens of stdlib names land. The honesty pass keeps the flat
default (so migrations are one-line) but adds the *foundation* for qualified
access without breaking it:

- **First increment (this design):** flat unqualified names, exactly as today.
  Collisions between two imported modules, or a module and a local, stay a
  compile-time error (`L0391`). Because stdlib names are curated, the shipped
  modules are guaranteed collision-free with each other.
- **Deferred (recorded here so the syntax is not painted into a corner):** an
  optional **qualified import** form, `import math as m` giving `m.sin(x)`, using a
  dotted access that does not collide with field access because the left side is a
  known module alias, not a value. This is the escape valve for large programs and
  for user/stdlib name clashes, and it is why `math`/`strings` own *disjoint*
  name sets today — so a later qualifier is purely additive. Per-item selective
  import (`import strings (split, join)`) is likewise deferred but compatible.

## Migration plan: move HTTP into a stdlib `http` module

HTTP is the proof-of-concept battery. The migration must be **behavior-preserving
for existing programs** and **backend-parity-preserving** (the AST/IR/bytecode
interpreters must stay identical, since HTTP is interpreter-only network I/O).

### Current state

`http_get`/`http_post` are **runtime builtins** (see
[standard_library.md](standard_library.md) Networking, and `L0336` in
[diagnostic_registry.md](diagnostic_registry.md)) implemented in
`crates/lullaby_runtime` over `TcpStream`, always in scope. The pure-Lullaby HTTP
*server* framework already lives in `examples/valid/http_server/http.lby` as a
`pub` module over the TCP builtins — proof the layer is expressible in-language.

### Target state

`http` becomes a **source stdlib module** (`stdlib/http.lby`) whose `pub`
functions `http_get`/`http_post` are written in Lullaby over the TCP socket
primitives (which stay in the prelude). It is imported, not global:

```lby
import http

fn main -> i64
    match http_get("http://example.com/")
        ok body -> len(body)
        err e -> -1
```

### Sequenced, non-breaking migration

The move is staged so no existing program breaks at any step:

1. **Author the source module.** Rewrite `http_get`/`http_post` as `pub` Lullaby
   functions in `stdlib/http.lby` over `tcp_connect`/`tcp_write`/`tcp_read`/
   `tcp_shutdown`/`tcp_close`, preserving the exact contract in
   [standard_library.md](standard_library.md): `http` scheme only (HTTPS →
   `err("https not supported")`), read-to-EOF via `Connection: close`, `err("http
   {code}: {first-body-line}")` for 4xx/5xx, and a read timeout. The existing
   `http_server/http.lby` server logic and `parse_i64` helper are the starting
   point.
2. **Wire the stdlib search root** so `import http` resolves the shipped module
   (see On-disk layout). At this point `import http` works.
3. **Deprecation window (compat shim).** Keep the *runtime* `http_get`/`http_post`
   builtins registered but emit a **deprecation diagnostic** (`L0345`, warning
   severity) when they are called without `import http` in scope — "`http_get` is
   moving to the `http` module; add `import http`." Existing programs keep running
   and building; they just get a warning. This is the only window in which the
   name resolves both ways.
4. **Flip resolution.** Once the window closes (a tagged release boundary), remove
   the prelude registration: `http_get`/`http_post` become **module-owned**.
   Calling them without `import http` is now a plain `L0309` unknown-function with
   a note "did you mean to `import http`?". The runtime implementation is deleted;
   the source module is authoritative.

### Parity-harness and CLI-test implications

- **Parity harness.** The cross-backend harness auto-discovers top-level
  `.lby` fixtures. Two things must hold: (a) the shipped `stdlib/http.lby` must
  **not** be auto-discovered as a standalone fixture (it has no `main`); place it
  under the stdlib tree the harness ignores, exactly as the multi-file module
  fixtures live in subdirectories the top-level harness skips. (b) An HTTP round-
  trip fixture must now `import http`, and the harness must build it through the
  loader with the stdlib search root enabled so all three interpreter backends
  resolve the module identically. Because HTTP is pure Lullaby over TCP and the
  loader merges modules into one `Program` *before* the backends, parity is
  preserved by construction — the backends never see "http", only merged
  functions.
- **CLI tests.** `http_server_round_trip_on_all_backends` (and the
  `fullstack_shared_logic_round_trip` HTTP client test) must switch their client
  source to `import http` and run through the project/loader path. During the
  deprecation window, add one test that the un-imported call still runs and emits
  the `L0345` warning; after the flip, replace it with a test that the un-imported
  call is `L0309`. The offline-docs example verifier must move the HTTP example
  under the `http`-importing form.
- **Manifest option.** Programs that already use a `lullaby.json`
  ([modules_design.md](modules_design.md)) need no change — the stdlib root is a
  compiler-level search root appended after project/dependency `src` dirs, so
  `import http` resolves without a manifest entry.

## On-disk and packaging layout

Coordinate with the packaging sequence in
[roadmap.md](roadmap.md) (§4 Installer Packaging), which
already fixes `bin/`, `docs/`, `examples/` in the portable bundle.

### In the repository

```
stdlib/
    http.lby          # pub http_get / http_post over the TCP primitives
    strings.lby       # (later) pub string conveniences
    math.lby          # (later, or an intrinsic-facade manifest) transcendentals
    README.md         # what each module is, and the primitive/module boundary
```

`stdlib/` sits at the repo root beside `examples/` and `tests/`. It is **not** a
Cargo crate; it is data the CLI knows how to find (like `examples/`). A new
CLI/loader constant resolves the stdlib root: in a dev checkout, relative to the
repo/binary; in an installed bundle, relative to the executable (below).

### In the installed toolchain bundle

Extend the bundle layout from [roadmap.md](roadmap.md):

```
<package-root>/
    bin/lullaby(.exe)
    stdlib/http.lby ...      # NEW: shipped stdlib modules, resolved relative to bin/
    docs/index.html
    examples/
    MANIFEST.json            # records the stdlib module list + version
```

The CLI locates `stdlib/` relative to its own executable path (`bin/../stdlib`),
so a portable, relocatable install needs no environment variable. The packaging
scripts (`scripts/package_portable.py`,
`scripts/package_windows_portable.ps1`) copy `stdlib/` into the package next to
`bin/` and record the module list in `MANIFEST.json`; the release verifier
(`scripts/verify_release.ps1`) smoke-tests an `import http` program against the
packaged CLI. This keeps the offline, no-network install requirement intact — the
stdlib is files on disk, never fetched.

## Diagnostics

Reuse the existing loader/semantic diagnostic families; add two codes scoped to
the stdlib boundary (next free codes after `L0344`):

- **`L0345`** (loader, *warning* during the deprecation window; see the HTTP
  migration) — "a prelude builtin is moving to a stdlib module; add `import
  NAME`." Emitted when a soon-to-be-module builtin (initially `http_get`/
  `http_post`) is called without its module imported. Downgrades cleanly to a
  post-flip `L0309` with a module-suggestion note.
- **`L0346`** (loader) — "unknown stdlib module." Emitted when `import NAME` names
  a curated-but-unavailable stdlib module (e.g. a typo of a real stdlib name, or a
  module missing from a corrupted bundle), distinguished from the user-module
  `L0397` (missing `NAME.lby`) by resolving against the stdlib manifest first.
  The message lists the available stdlib modules.

Existing codes carry the rest: a name that is module-owned but used without the
import is `L0309` (unknown function) with a suggestion note; a stdlib/user name
collision is `L0391`; a wrong-arity/type call into a stdlib builtin keeps its
current code (`L0336` for HTTP until it becomes source, then ordinary
argument-checking on the `pub` function).

## Scope and sequencing

The production-complete **first increment** establishes the layout and moves
exactly one battery, end to end:

1. **Prelude manifest.** Make the in-scope builtin/type set an explicit compiler
   manifest (the machine-checkable backing for
   [standard_library.md](standard_library.md)), so reclassification is a data
   change.
2. **Stdlib search root + `stdlib/` tree.** Add the stdlib resolution root to the
   loader (appended after user/project/dependency dirs) and create `stdlib/` in
   the repo and the packaged bundle, with `MANIFEST.json` recording the module
   list.
3. **Move `http` (the proof).** Author `stdlib/http.lby` as `pub`
   `http_get`/`http_post` over the TCP primitives; run the deprecation window with
   `L0345`; update the parity harness, `http_server_round_trip_on_all_backends`,
   the full-stack HTTP round-trip test, and the offline-docs HTTP example to the
   `import http` form; then flip to module-owned and delete the runtime builtins.
4. **Docs.** Update [standard_library.md](standard_library.md) to mark HTTP as a
   module (moving it out of the always-in-scope catalog), add a "stdlib modules"
   section pointing here, and update `documents/repository_map.md` and the offline
   docs. Register `L0345`/`L0346` in
   [diagnostic_registry.md](diagnostic_registry.md).

**Deferred** (recorded so the increment does not over-reach): moving `strings` and
`math` (a fast follow using the same pattern once HTTP proves it); qualified/
aliased imports (`import math as m`) and selective imports; a package registry or
remote stdlib fetching (the local-only model in
[modules_design.md](modules_design.md) stands); and a `sync`/`threads`
higher-level concurrency module.

## Why these choices

- **Primitive/battery test over a taste call.** The three-part test
  (irreducible, universal, backend-defining) gives a repeatable rule for every
  future builtin, so the prelude stops accreting batteries. HTTP fails it plainly;
  TCP passes it plainly.
- **Reuse the existing module system unchanged.** Stdlib modules are `import
  NAME` + `pub`, so there is nothing new for a user to learn and a migration is
  one `import` line. The loader already merges modules into one `Program` before
  the backends, so moving a battery carries **zero backend-parity risk** — the
  same reason the module system itself was frontend-only.
- **Source-first, intrinsic-facade only where forced.** Shipping batteries as
  readable `.lby` (starting with `http`, which already exists) keeps the stdlib
  honest and inspectable; the intrinsic facade is a bounded escape hatch that can
  be retired module-by-module as the language gains the primitives to express its
  batteries.
- **Deprecation window, not a hard break.** `L0345` lets every existing HTTP
  program keep building and running through a release boundary before the name
  becomes module-owned, honoring "preserve user work."
- **Stdlib on disk, resolved relative to the binary.** Keeps the offline,
  no-network, relocatable install guarantee — the stdlib is files beside `bin/`,
  never a fetch — and matches the packaging layout already planned in
  [roadmap.md](roadmap.md).
- **Flat now, qualified-ready later.** Keeping the flat namespace preserves
  one-line migrations while curated, disjoint module name sets guarantee a later
  `import math as m` is purely additive and never a breaking change.
