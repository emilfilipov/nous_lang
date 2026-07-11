# Standard Library I/O Module Boundary

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This document defines the boundary between **compiler built-ins** (names the
compiler knows and type-checks directly) and **runtime standard-library
functions** (host-backed operations the runtime provides) for I/O. It is the
design deliverable for the I/O module surface; the broader planned surface lives
in [lullaby_input_output.md](lullaby_input_output.md).

Examples below use the current `.lby` extension and the flat builtin surface
that is actually implemented. The eventual dotted `io.*` module spelling and the
project rename (see the naming ticket) are tracked separately; when the rename
lands, only the spelling of these names changes, not the boundary defined here.

## Boundary Rule

- A **compiler intrinsic** is a name the semantic checker recognizes, type-checks
  with fixed signatures, and lowers through IR. It has no user-visible
  definition and cannot be shadowed by a user function. All current I/O names are
  intrinsics.
- A **runtime stdlib function** is a normal Lullaby function (eventually shipped
  as `.lby` source or a host shim) that may call one or more intrinsics. The
  the compiler ships no stdlib functions yet; every operation below is currently an
  intrinsic so the surface stays small and each op has one authoritative
  implementation.

The rule for classifying a new operation: if it needs a host syscall or
privileged runtime state, it is an **intrinsic**; if it can be expressed purely
in terms of existing intrinsics, it belongs in the **runtime stdlib**.

## API Surface

| Operation | Signature | Kind | Status | Error behavior |
| :-- | :-- | :-- | :-- | :-- |
| `read_file(path)` | `string -> string` | intrinsic | implemented | `L0414` resource error if missing/unreadable |
| `write_file(path, content)` | `string, string -> void` | intrinsic | implemented | `L0415` resource error if unwritable |
| `append_file(path, content)` | `string, string -> void` | intrinsic | implemented | `L0415` resource error if unwritable |
| `file_exists(path)` | `string -> bool` | intrinsic | implemented | never errors; returns `false` on stat failure |
| `println(text)` / `print(text)` | `string -> void` | intrinsic | implemented | `L0419` if stdout write fails |
| `warn(text)` | `string -> void` | intrinsic | implemented | `L0419` if stderr write fails |
| `flush()` | `-> void` | intrinsic | implemented | `L0419` if stdout flush fails |
| `read_line()` (stdin) | `-> string` | intrinsic | planned | `L0419`-class on read failure; empty string at EOF |
| `open(path, mode)` | `string, string -> file` | intrinsic | planned | opens a handle; resource error on failure |
| `stat(path)` | `string -> filemeta` | intrinsic | planned | resource error if the path cannot be stat-ed |
| `list_dir(path)` | `string -> array<string>` | intrinsic | planned | resource error if not a readable directory |

`stdin`, `stdout`, and `stderr` are modeled today as the fixed intrinsics
`read_line`/`print`/`println`/`warn` rather than first-class stream handles. A
`file` handle type and `open`/`close`/read-write-on-handle are introduced only
when buffered and binary I/O land; until then the whole-file and standard-stream
intrinsics cover the current use cases.

## Examples

```lullaby
fn main -> void
    write_file("notes.txt", "first line\n")
    append_file("notes.txt", "second line\n")
    if file_exists("notes.txt")
        let body string = read_file("notes.txt")
        println(body)
    else
        warn("notes.txt missing")
    flush()
```

## Error Model

- **Programming errors** (wrong arity or argument type) are compile-time semantic
  diagnostics (`L0312` arity, `L0313` argument type), so bad I/O calls never
  reach the runtime.
- **Host/resource failures** (missing file, unwritable path, broken pipe) are
  categorized `resource` runtime diagnostics (`L0414`, `L0415`, `L0419`) carrying
  a source span and traceback.
- **Predicates do not throw:** `file_exists` returns a `bool` rather than
  erroring, so callers branch instead of catching.

## Non-Goals For This Boundary

- Buffered readers/writers and binary I/O (needs the `file` handle type).
- Memory-mapped files, sockets, and IPC (separate subsystems).
- A dotted `io.*` namespace and user-importable modules (needs the module system).
