# Linker And Binary-Output Validation Plan

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This document is the design/plan deliverable for symbol resolution, module
linking, output sections, and output verification. It sequences the work from
the current validated bytecode-output milestone toward native binaries. The
executable native contract lives in
[native_backend_contract.md](native_backend_contract.md) and
`crates/lullaby_ir/src/native_contract.rs`; the first object emitter lives in
`crates/lullaby_ir/src/native_object.rs`.

## Alpha Milestone: Validated Output Exists Today

The Alpha 1 milestone deliberately validates a **bytecode** output rather than a
linked native binary, so the toolchain has an end-to-end, checkable output
contract before native linking is attempted:

- Versioned `.lbc` artifacts (format/version/entry/target/payload) are encoded
  and decoded with compatibility checks; malformed or unsupported artifacts fail
  with the `L0601` bytecode diagnostic.
- `lullaby inspect` reports artifact metadata, the function table, and ordered
  memory-operation metadata in human-readable, `--verbose`, and `--format json`
  forms — the machine-readable and human-readable verification reports required
  by this ticket.
- Object-emission snapshots (`alpha1_return_42.coff.json`,
  `alpha1_locals_add.coff.json`, `alpha1_assignments.coff.json`) pin the
  `source -> typed IR -> bytecode -> COFF object` path deterministically.

This satisfies "the alpha milestone has explicit binary-output or
bytecode-output validation" and "verification reports are machine-readable and
human-readable" against the current subset.

## Symbol Resolution

- **Symbol table.** Each compiled module publishes a symbol table keyed by a
  mangled name. Alpha uses a flat, non-namespaced scheme: `main` exports as the
  platform entry symbol; user functions export as `nl$<function>`; builtins are
  not emitted as symbols (they lower inline or resolve to a small runtime).
- **Binding classes.** `GLOBAL` for exported user functions and `main`, `LOCAL`
  for compiler-internal helpers, `UNDEF` for references resolved at link time
  (initially only the C runtime entry/exit and any native runtime helpers).
- **Resolution order.** Intra-module references resolve first from the module
  symbol table; unresolved externals are collected as relocations and resolved
  against (1) other Lullaby object modules, then (2) the platform C runtime.
- **Duplicate/missing symbols.** A duplicate `GLOBAL` definition or an
  unresolved `UNDEF` at final link is a hard error surfaced through the shared
  `N####` diagnostic model (reserved: `L0610` duplicate symbol, `L0611`
  unresolved symbol), never a silent link.

## Relocation Assumptions

- Alpha targets 64-bit only. Code is emitted position-independent-friendly with
  RIP-relative references on x86-64.
- Relocation kinds needed for the first milestone:
  - PC-relative 32-bit for `call`/`jmp` to other functions
    (COFF `IMAGE_REL_AMD64_REL32`, ELF `R_X86_64_PLT32`/`R_X86_64_PC32`).
  - Absolute 64-bit for data/address-of (COFF `ADDR64`, ELF `R_X86_64_64`).
- String and array literals live in a read-only data section and are referenced
  by relocation, not by absolute immediates baked into code.
- ARM64 relocations (`CALL26`, `ADRP`/`ADD` pair) are deferred to the second
  target and are listed here only to keep the abstraction target-parametric.

## Output Sections

Consistent logical sections across object formats, mapped per target:

| Logical | COFF | ELF | Mach-O | Contents |
| :-- | :-- | :-- | :-- | :-- |
| code | `.text` | `.text` | `__TEXT,__text` | function machine code |
| rodata | `.rdata` | `.rodata` | `__TEXT,__const` | string/array literals |
| data | `.data` | `.data` | `__DATA,__data` | mutable globals (none in alpha) |
| symbols | COFF symtab | `.symtab` | symtab | symbol table |
| relocs | per-section | `.rela.*` | per-section | relocation entries |

## Platform Support Ordering

1. **PE/COFF (`x86_64-pc-windows-msvc`)** — first target; object emitter exists.
   Link via the platform toolchain (`link.exe`/`lld-link`) against the C runtime
   for process entry/exit.
2. **ELF (`x86_64-unknown-linux-gnu`)** — second; reuse the section/symbol
   abstraction, emit ELF relocations, link via `cc`/`ld`.
3. **Mach-O (`aarch64-apple-darwin`, `x86_64-apple-darwin`)** — third; add
   ARM64 code lowering and Mach-O sections.

The compiler drives the platform linker rather than shipping its own linker for
the first milestones; a self-contained linker is a non-goal until PE/ELF/Mach-O
emission is proven end to end.

## Verification Checks

- **Pre-link (per object):** section/symbol/relocation well-formedness asserted
  by the object emitter plus checked-in snapshots.
- **Link driver:** verify the produced binary has the expected entry symbol and
  no unresolved externals; capture the linker's own diagnostics and re-emit them
  through the `N####` model.
- **Post-link smoke:** run the produced binary for representative fixtures and
  compare exit code / stdout against the AST-runtime result, extending the
  existing cross-backend parity tests to a native backend once available.
- **Reports:** every check emits both a concise human line and JSON, matching
  the existing `check`/`run`/`inspect` diagnostic contract.

## Non-Goals For This Plan

- Incremental/​separate compilation caching.
- Link-time optimization.
- Dynamic linking / shared-library output (static-linked executables first).
- A hand-written linker while platform linkers are available.
