# Modules, Imports, and Visibility Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This note designs multi-file Lullaby programs: how a file pulls in declarations
from another file (`import`), and how a file controls what it exposes (`pub`).
The design keeps the token-minimal, indentation-only aesthetic and — crucially —
is a **frontend-only** change: imports are resolved and all files are merged
into a single `Program` before semantic analysis, so the type checker, all three
backends (AST, IR, bytecode), and the optimizer are unchanged.

## Files are modules

Each `.lby` file is a module. A module's top-level declarations (`fn`, `struct`,
`enum`, `alias`) are **file-private by default** and become importable only when
marked `pub`:

```lby
# geometry.lby
pub struct Point
    x i64
    y i64

pub fn dot a Point b Point -> i64
    a.x * b.x + a.y * b.y

fn helper n i64 -> i64      # private: not importable
    n * n
```

## Import

A file imports another module by name at the top of the file, before any
declaration:

```lby
# main.lby
import geometry

fn main -> i64
    let p Point = Point(3, 4)
    dot(p, p)
```

Rules:
- `import NAME` loads `NAME.lby` from the **same directory** as the importing
  file. (A dotted `import pkg.sub` maps to `pkg/sub.lby` — deferred; first
  increment resolves flat single-name imports in the entry file's directory.)
- An import makes the imported module's `pub` items available **unqualified** in
  the importing file. This is the most token-minimal option and avoids a new
  qualifier token (a qualified `Module.item` would collide with field/method
  access, and `Module::item` would add a new token; flat import keeps the
  surface unchanged).
- A name that is `pub` in two different imported modules, or that collides with
  a local declaration, is a compile-time error (`L0391`) — flat namespacing
  trades brevity for a no-shadowing rule.
- Imports are transitive for **loading** (importing `a` which imports `b` loads
  `b`), but names are **not** re-exported: `main` importing `geometry` does not
  automatically see what `geometry` imported. Each file sees only its own
  imports plus its own declarations.

## Visibility

- `pub` may prefix a top-level `fn`, `struct`, `enum`, or `alias`. Only `pub`
  items are importable; unmarked items are private to their file.
- Struct fields and enum variants inherit their type's visibility in the first
  increment (a `pub struct` exposes all its fields). Per-field visibility is
  deferred.
- Using a non-`pub` item from another module is a compile-time error
  (`L0392`) — reported at resolution time.

## Loading pipeline (frontend only)

A new module-loader stage runs before semantic analysis:

1. Start from the entry file. Lex+parse it, collect its `import` names.
2. For each import, resolve `NAME.lby` in the entry file's directory, lex+parse
   it, and recurse over its imports. Detect and reject import cycles (`L0393`)
   and missing module files (a resolver diagnostic).
3. Merge every loaded module's declarations into one `Program`, tagging each
   declaration with its source module and visibility, and record per-file import
   sets so name resolution can enforce visibility and the no-shadowing rule.
4. Hand the merged `Program` to the existing semantic analyzer and backends
   unchanged. Because everything is one `Program`, execution, optimization, and
   bytecode artifacts need no changes.

The parser already reports `L0211` for `import`/`module` as planned syntax;
that rejection is removed for `import` and `pub` exactly as `struct`/`enum`/
`match` were un-rejected.

## AST and CLI

- Parser: accept a leading `import NAME` line list, and an optional `pub`
  modifier on top-level declarations. Add `imports: Vec<String>` and a `pub`
  flag (or a `visibility` field) to the relevant AST nodes (serde-defaulted so
  single-file artifacts stay compatible).
- CLI: `lullaby run/check/compile <entry.lby>` invokes the loader to build the
  merged program. Single-file programs with no imports behave exactly as today.

## Diagnostics

- `L0391` — name collision across imports / with a local declaration.
- `L0392` — use of a non-`pub` item from another module.
- `L0393` — import cycle.
- Plus a resource-style diagnostic for a missing module file.

## Projects and the `lullaby.json` manifest

A single file (`.lby`) still builds exactly as before. A **project** adds a
`lullaby.json` manifest at its root so a build can span multiple source
directories and depend on other local Lullaby projects:

```json
{
  "name": "myapp",
  "version": "0.1.0",
  "entry": "src/main.lby",
  "src": ["src"],
  "dependencies": { "mathx": "../mathx" }
}
```

- `name` (string) — the project name.
- `version` (string, optional) — the project's own version, a semver-shaped
  `MAJOR.MINOR.PATCH` with an optional `-<prerelease>` suffix (e.g. `0.1.0`,
  `1.2.0`, `1.0.0-preview`). This mirrors the toolchain's `MAJOR.PATCH-STATUS`
  display scheme mapped onto semver (see [versioning.md](versioning.md)). It is
  **optional and backward-compatible**: a manifest with no `version` loads
  unchanged. When present it must be well-formed — exactly three `.`-separated
  non-negative integers (no leading zeros except the single digit `0`), plus an
  optional `-<prerelease>` of `.`-separated ASCII-alphanumeric/`-` identifiers —
  or the manifest is rejected with `L0343`. `lullaby new` scaffolds `"0.1.0"`.
- `entry` (path, optional) — the executable `.lby`, relative to the manifest
  directory. Library projects omit it.
- `src` (array of paths, default `["."]`) — source directories, relative to the
  manifest directory.
- `dependencies` (object, default `{}`) — dependency name → path to another
  project root that contains its own `lullaby.json`. **Local path dependencies
  only**; remote/registry *fetching* is deferred.

### The manifest is a stable 1.0 contract

`lullaby.json` is part of the 1.0-stable public surface: the field names above
(`name`, `version`, `entry`, `src`, `dependencies`), their meanings, the module
resolution order below, and the `L0343` diagnostic are a **stable contract**. The
format is designed to stay **forward-compatible** — later toolchain versions add
capability by introducing *new optional* fields (e.g. a package registry source,
per-dependency version *constraints*, a lockfile reference) without changing or
removing any existing field, so a `lullaby.json` written today keeps loading on
future toolchains. The `version` field is reserved now precisely so that a future
registry/dependency-versioning layer is a purely additive change. Manifests
authored before `version` existed (no `version` key) remain valid indefinitely.

### Module resolution across a project

When a project is built, `import NAME` resolves `NAME.lby` by searching, in
order: (1) the importing file's own directory, (2) every `src` directory of the
current project, then (3) every `src` directory of each transitively resolved
dependency project. The existing merge-into-one-`Program`, `pub` visibility,
no-shadowing (`L0391`), private-cross-module (`L0392`), and cycle (`L0393`) rules
apply unchanged across the whole set — so a dependency's private item is not
usable from the consuming project, and a name that collides across any two loaded
modules is still an error.

### CLI

`lullaby run|check|build|test` accept **either** a `.lby` file (unchanged) **or**
a directory / a `lullaby.json` path. Given a directory, the CLI looks for
`lullaby.json` in it; given `lullaby.json`, it uses it. `run`/`build`/`compile`/
`wasm`/`native` require an `entry`; `check`/`test` can operate on a library
project without an `entry`, validating every module across the `src` set.

### Diagnostics and errors

A missing `lullaby.json`, malformed JSON, a missing/invalid `src` directory, a
missing dependency directory or its missing `lullaby.json`, and a
`run`/`build`/`compile` target without an `entry` all report `L0343` (loader).

## Scope and sequencing

First increment: flat single-name imports in the entry directory, `pub`
visibility on top-level declarations, unqualified merged namespace with a
no-shadowing rule, cycle/missing detection, plus the `lullaby.json` project
manifest with multiple `src` directories and **local path** dependencies.
Deferred: dotted module paths, qualified access, per-field visibility,
re-exports, and remote/registry dependency fetching.

## Why these choices

- **File = module, `pub` to export**: the simplest mental model; matches how
  most systems languages scope visibility.
- **Frontend-only merge**: keeps the three backends and the optimizer untouched,
  so multi-file support carries zero backend risk and stays at parity for free.
- **Flat unqualified import**: no new tokens, no collision with `.` access;
  the no-shadowing rule keeps it unambiguous.
