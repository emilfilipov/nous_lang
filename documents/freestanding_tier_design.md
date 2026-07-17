# Lullaby Freestanding / `no-runtime` (Kernel) Tier — Design Proposal

**Status:** Design proposal, 2026-07-14. **Stage 1 (the tier gate + enforcement)
is delivered** (2026-07-15) — see §10.2 stage 1 and the "Stage 1 delivery" note
at the end of §10.2. **The raw-pointer addressing surface `addr_of` / `ptr_offset` /
`ptr_cast` is delivered** (2026-07-16, §10.2 increment 4) — see §2.2 and the
"Raw-pointer addressing delivery" note (§10.4). **Native x86-64 codegen for the
whole raw-pointer surface is delivered** (2026-07-16) — a kernel-shaped function is
now native-eligible instead of skipping, and `addr_of` is a real `lea` so
`ptr_write(addr_of(x), 5)` genuinely mutates `x`; see §10.5 and the "Raw-Pointer
Surface" section of [native_backend_contract.md](native_backend_contract.md).
**MMIO and port-mapped I/O are delivered** (2026-07-16, §4.1) — MMIO composes from
the delivered `int_to_ptr`/`ptr_offset`/`volatile_store` + void functions with no
intrinsics of its own, and `port_in8/16/32` / `port_out8/16/32` lower to real x86
`in`/`out` (native-only; the interpreters refuse them with `L0444` rather than
fabricate a device value). **Static-buffer arenas are delivered** (2026-07-16, §5)
— `region <name> in <buffer>` + `arena_alloc(region, count)` give a `no-runtime`
module its first bounded, allocator-free storage (until now a driver could reach
hardware but had nowhere to put what it read, since `alloc`/`list`/`map` are all
`L0441`-rejected here by design). Overflow is a defined, deterministic edge
(`ud2` natively, `L0460` on the interpreters), and the arena has **full four-tier
parity**. See the as-built record in §5.2 — it deviates from §5's text in three places where that text is
wrong about the delivered compiler. The **privileged** set of §4 (`read_cr`/`write_cr`,
`read_msr`/`write_msr`, `halt`/`cli`/`sti`) remains undelivered, as does the
pluggable panic handler (§8), which is the natural next increment: it replaces the
arena's overflow trap with a call to the program's own `panic fn`. This document proposes the concrete, buildable shape of
Lullaby's **freestanding / `no-runtime` tier** — the capability set that makes 1.0
a systems language you can write a kernel, boot code, embedded firmware, and FFI
shims in.

Canonical language rules: see [core_language_rules.md](core_language_rules.md).
The decided direction, the two-tier identity, and the 10-item kernel checklist are
fixed in [execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md) —
this document designs *within* that decision and does not re-litigate it. It
reconciles with the existing `ptr<T>` / `unsafe` / raw-memory surface in
[lullaby_memory_management.md](lullaby_memory_management.md) and
[lullaby_type_system.md](lullaby_type_system.md), the atomics surface in
[atomics_design.md](atomics_design.md), the region model in
`execution_tiers_and_1_0_scope.md`, and the *already delivered* native-backend
freestanding machinery in [native_backend_contract.md](native_backend_contract.md)
(the `--freestanding` flag, `L0426`, the raw-byte `asm` statement, the direct-PE
writer, the ELF/Mach-O/AArch64 freestanding `_start` stubs) and
[linker_and_binary_output_plan.md](linker_and_binary_output_plan.md). It is
stylistically consistent with [concurrency_model_design.md](concurrency_model_design.md):
every meaningful surface-syntax choice is an explicit **OWNER DECISION NEEDED**
fork with 2–3 options, trade-offs, and a recommendation.

This proposal **writes no code and edits no other document.** It creates only this
file. A follow-up doc pass (a dedicated documentation sub-agent) must reconcile
`native_backend_contract.md`, `execution_tiers_and_1_0_scope.md`,
`lullaby_memory_management.md`, `repository_map.md`, `roadmap_1_0.md`, and
`diagnostic_registry.md` once the owner accepts a direction here.

---

## 0. Design invariants (the two hard rules)

Everything below serves two non-negotiable properties from the canonical doc:

1. **No hidden allocation.** In `no-runtime` code the compiler must never secretly
   call an allocator or grow a heap. A `list<T>` push, a `string` concat, an
   escaping value auto-copied into a *growable* arena, an actor mailbox enqueue —
   each of those is a hidden `malloc`-class event and is **unacceptable in an
   interrupt handler or a boot path**. In `no-runtime`, allocation is *explicit and
   bounded*: it comes from a caller-provided static buffer (§5) or it is a compile
   error (§1).
2. **No hidden control flow.** No implicit refcount `dec`/`drop`, no
   panic-unwinding through user frames, no scheduler yield, no GC safepoint. Every
   branch and every call in `no-runtime` code is visible in the source. Bounds
   checks remain (safety), but their failure edge calls a *user-provided* handler
   (§8), never a runtime `abort`.

These two rules are what distinguish the freestanding tier from "the safe tier with
`unsafe` sprinkled in". The compiler *enforces* them (§1.3), so a kernel author gets
a hard diagnostic instead of a surprise allocation.

---

## 1. Entering the tier — `no-runtime` gating

**OWNER DECISION NEEDED — how code opts into freestanding.**

The freestanding tier removes guarantees (RC, actors, host allocator, growable
heap) and adds powers (raw pointers everywhere, `asm`, MMIO). The opt-in must make
the tier boundary *visible in source* — an LLM or a reviewer should see, at the top
of a file, that this is kernel-mode code where the two hard rules apply.

| Option | Shape | Trade-offs |
| :-- | :-- | :-- |
| **A. Module/file-level `no-runtime` directive** (first non-comment line) | `no-runtime` on its own line at the top of a `.lby` file | The whole compilation unit is freestanding: every `fn` in it is checked against the two hard rules, RC/actors/growable-heap are *unavailable* (hard error, not lint), and the native backend emits the freestanding entry model. Greppable, self-documenting, matches how a kernel is actually organized (whole files/modules are kernel code). One new keyword. **Recommended.** |
| B. Compile target / CLI flag only (`lullaby native --freestanding`, already exists) | no source marker; the flag decides | Zero syntax. But the *source* no longer says whether it is kernel code — the same file behaves differently under different flags, which is exactly the ambiguity that bites LLM-generated code and reviewers. The flag already exists and stays (it is the *output* contract, §9); it should not also be the *only* semantic gate. |
| C. Per-function `unsafe fn` / `unsafe` blocks only | freestanding-ness is inferred from `unsafe` usage | Too fine-grained and wrong-grained: `unsafe` marks *a raw operation*, not *a runtime tier*. Most of a kernel is safe arena code (the whole point of Lullaby's kernel story) that is still `no-runtime`; forcing it under `unsafe` would erase the "most of a kernel stays arena-safe" advantage. |

**Recommendation: Option A (module-level `no-runtime` directive), with the existing
`--freestanding` flag retained as the orthogonal *output* contract.**

```lby
# kernel/vga.lby — a freestanding module
no-runtime

fn clear_screen
    let mut i i64 = 0
    while i < 80 * 25
        # safe arena/value code — still no-runtime, no hidden alloc
        i += 1
```

- `no-runtime` is a **module directive**, not a block. It appears once, as the first
  non-comment line of a `.lby` file. It sets the tier for every declaration in that
  file. (Spelling alternatives the owner may prefer: `no-runtime`, `freestanding`,
  `bare`, `kernel`. Recommend `no-runtime` — it names the actual guarantee and reads
  in English. The hyphen appears in no other Lullaby keyword, so an underscore form
  `no_runtime` is the fallback if the lexer should avoid `-` adjacency with the
  subtraction operator; **OWNER DECISION** on the exact token.)
- The two tiers still share **one syntax and one type system** — a `no-runtime`
  file uses the same `fn`/`struct`/`enum`/`if`/`while`/`for`/`region`/`match`/`?`
  grammar. Only the *available* constructs and the *runtime assumptions* change.
- Relationship to `--freestanding` (already delivered): the directive is the
  **semantic** gate (what the language lets you write); the flag is the **binary**
  gate (guarantee no CRT link, `L0426`). A `no-runtime` module *should* be built
  with `--freestanding` for the no-CRT guarantee, and the compiler warns
  (proposed `L0440`, a warning) if a `no-runtime` module is built without it — but
  they are separable so you can, e.g., unit-test a `no-runtime` driver's pure logic
  in a hosted harness.

### 1.1 What is *unavailable* in `no-runtime`

A `no-runtime` module **cannot name or lower to** any construct that implies the
safe-tier runtime. Each is a hard compile error (not a lint):

- **Reference counting:** `rc<T>`, `rc_new`/`rc_clone`/`rc_release`/`rc_get`, and
  the implicit `inc`/`dec`/`drop` insertion. RC is *never present* in this tier
  (canonical). A `ref<T>` borrow (non-owning, raises no refcount) is *allowed* — it
  is a pure alias with no runtime.
- **Actors & the scheduler:** `actor`/`state`/`on`/`spawn`/`tell`/`ask`/`await`/
  `Future<T>`/`shared<T>`/`supervise` (see `concurrency_model_design.md` §4.2). The
  freestanding tier exposes raw concurrency primitives instead (§4 below).
- **Host-allocated / growable arenas:** the default *implicit* function/loop arenas
  and the *growable* explicit `region` all assume a host allocator that can grow a
  chunk on overflow. In `no-runtime`, arenas exist but are **backed by a
  caller-provided static buffer** and **cannot grow** (§5).
- **Any implicit heap growth:** `list<T>` push/`map<K,V>` insert/`string` concat and
  other operations that call the growable allocator. `array<T>` (fixed length) and
  `string` *slices/reads* are fine; anything that *grows* is not (§1.2).
- **Panic → abort:** the safe tier's `panic → abort`/`panic → supervisor` path.
  Replaced by the user panic handler (§8).

Proposed diagnostic **`L0441`** (semantic): *"`<construct>` is unavailable in a
`no-runtime` module (it requires the Lullaby runtime); use the freestanding
equivalent or move this code out of the `no-runtime` module."* The message names the
offending construct and, where one exists, its freestanding replacement (`rc<T>` →
`ptr<T>`/arena; `list.push` → fixed `array<T>` or an arena buffer; `spawn` → raw
thread/atomics intrinsics).

### 1.2 What is *available* in `no-runtime`

- All scalars, `struct`/`enum`/`match`, fixed `array<T>`, `option`/`result`/`?`,
  generics, `trait`/`impl`, functions, `if`/`while`/`for`/`loop`, `region` blocks
  (static-buffer-backed, §5), `ref<T>` borrows.
- `ptr<T>` + pointer arithmetic + `unsafe` (§2), inline `asm` (§3), volatile/MMIO +
  port I/O + privileged intrinsics (§4), atomics/fences (`atomics_design.md`, all
  allocation-free), `size_of`/`align_of`/`offset_of`/`ptr_to_int`/`int_to_ptr`
  (already delivered), `repr`/`packed`/`align` layout control (§7), ISR/`naked`
  functions (§6), the user panic handler (§8), and freestanding output control (§9).

### 1.3 Enforcing the two hard rules

The compiler proves "no hidden allocation / no hidden control flow" during
lowering, using machinery that already exists:

- **Allocation-site classification.** The IR already annotates allocating
  expressions with an escape annotation (the local escape pass from the arena work,
  `execution_tiers_and_1_0_scope.md` §"Implementation representation"). In a
  `no-runtime` module, the semantic pass walks these annotations: any allocation
  that would lower to `__lullaby_arena_alloc` *with grow-a-new-chunk semantics* or
  to the RC free-list allocator is rejected (`L0441`). Allocations that lower to a
  *fixed-buffer* arena bump (§5) are allowed. This is a classification the compiler
  already computes; `no-runtime` just makes the "would grow the heap" case an error
  instead of emitting the grow path.
- **Control-flow classification.** The RC drop-insertion pass already knows every
  point it *would* insert an `inc`/`dec`/`drop` (`lullaby_memory_management.md`
  §"Scope-Based Drop Insertion"). In `no-runtime`, that pass asserts it inserts
  **zero** RC ops (there is no RC), and the actor-lowering and panic-unwinding paths
  are disabled at the tier gate. The only compiler-inserted control flow that
  remains is the bounds check, whose failure edge is a *visible* call to the user
  panic handler (§8), not hidden.
- **Result:** a `no-runtime` build that would need the heap or the runtime fails to
  compile with a specific `L0441`, naming the construct. The kernel author never
  ships a binary with a surprise `malloc` in an ISR.

---

## 2. `unsafe` blocks + raw pointers

Raw pointers already exist: `ptr<T>` is a delivered reference type
(`lullaby_type_system.md`), `unsafe` is a delivered block form
(`lullaby_memory_management.md`: `raw_write` inside `unsafe`), and the raw-memory
builtins `ptr_read`/`ptr_write`, `ptr_to_int`/`int_to_ptr`, `volatile_load`/
`volatile_store`, `size_of`/`align_of`/`offset_of` are delivered. This section
reconciles and completes the surface for pointer *arithmetic*, *deref*, *cast*, and
*address-of*, which early boot code needs pervasively.

### 2.1 The `unsafe` block (delivered form, confirmed)

`unsafe` is an indentation-only block (no braces), exactly like `if`/`while`:

```lby
unsafe
    ptr_write(p, 42)
    let x i64 = ptr_read(p)
```

Raw-pointer deref/arithmetic/cast, `asm`, MMIO, port I/O, and privileged intrinsics
are all `unsafe` operations: using them outside an `unsafe` block is a compile error
(`L0330` already covers `asm`; **as delivered (§10.4) the raw-pointer operations reuse
that same `L0330`** and the once-proposed **`L0442`** was not needed — it remains
unimplemented and unregistered, reserved at most for the undelivered MMIO/port stages,
covers the raw-pointer/MMIO operations — *"raw-pointer / hardware operation `<op>`
requires an `unsafe` block"*). Note `unsafe` and `no-runtime` are **orthogonal**:
`unsafe` marks a raw operation and is available in *both* tiers; `no-runtime` marks
the module's runtime tier. Most of a `no-runtime` kernel is *safe* code outside any
`unsafe` block.

### 2.2 Raw pointer operations

**OWNER DECISION NEEDED — pointer deref / arithmetic / address-of / cast spelling.**

Lullaby's constraints rule out C's `*p`, `&x`, `p->f`: there are no C-style deref/
address-of sigils in the grammar, and `*`/`&` are the multiply / (future) bitwise-and
operators. The delivered surface is **builtin-function style** (`ptr_read(p)`,
`ptr_write(p, v)`). Two coherent directions:

| Option | Deref / write / addr-of / offset / cast | Trade-offs |
| :-- | :-- | :-- |
| **A. Builtin functions (extend the delivered set)** | `ptr_read(p)` / `ptr_write(p, v)` / `addr_of(x)` / `ptr_offset(p, n)` / `ptr_cast<U>(p)` | Consistent with the *already shipped* `ptr_read`/`ptr_write`/`ptr_to_int`; no new operators; greppable; unmistakably a raw op; trivial for tiny LLMs (call syntax they already know). Slightly verbose in arithmetic-heavy code. **Recommended.** |
| B. Method/UFCS on `ptr<T>` | `p.read()` / `p.write(v)` / `x.addr()` / `p.offset(n)` / `p.cast<U>()` | Reads fluently and chains (`p.offset(4).read()`); reuses the delivered UFCS method dispatch. But it makes a raw memory access look like an ordinary safe method call — blurring the hazard boundary the way C's `p->f` does, contrary to "hardware ops are visibly distinct". |
| C. Dedicated operators / keywords | `deref p` / `ref_to x` / `p + n` overloaded on pointers | Terse. But overloading `+` on `ptr<T>` reintroduces hidden semantics (scaled vs byte arithmetic ambiguity), and new keyword operators enlarge the surface tiny models must learn. |

**Recommendation: Option A** — extend the delivered builtin-function set. It keeps
raw memory access *lexically obvious* (every access is a named `ptr_*` call inside
`unsafe`), needs no new operators, and is source-compatible with what already ships.
The concrete surface:

```lby
no-runtime

fn poke_and_peek base ptr<u32> -> u32
    unsafe
        # address arithmetic: element-scaled by default, byte-scaled explicitly
        let slot ptr<u32> = ptr_offset(base, 4)        # base + 4*size_of(u32)  (element stride)
        let raw  ptr<u32> = ptr_offset_bytes(base, 3)  # base + 3 bytes         (raw byte stride)
        ptr_write(slot, 0xDEADu32)
        let v u32 = ptr_read(slot)                     # deref-read
        # address-of a local / global (needs the value to be addressable)
        let mut n u32 = 7u32
        let np ptr<u32> = addr_of(n)                   # &n
        # reinterpret cast between pointer element types
        let bp ptr<byte> = ptr_cast<byte>(base)        # (byte*)base
        v
```

- **`ptr_offset(p ptr<T>, n i64) -> ptr<T>`** — element-scaled arithmetic (`p + n*size_of(T)`), the common and least error-prone form.
- **`ptr_offset_bytes(p ptr<T>, n i64) -> ptr<T>`** — raw byte arithmetic, for unaligned/device layouts. Two names make the scaling *explicit* (the one real footgun in C pointer math).
- **`ptr_read(p ptr<T>) -> T`** / **`ptr_write(p ptr<T>, v T) -> void`** — deref read/write (delivered).
- **`addr_of(x) -> ptr<T>`** — address-of an addressable lvalue (local, global, struct field, array element). The value must be *addressable*; taking the address of a temporary is **`L0458`** (the delivered code — see §10.4; the `L0442` this bullet originally proposed was never implemented or registered). **As delivered**, `addr_of` is *place-backed* on the interpreters and genuinely aliases: `ptr_write(addr_of(x), 5)` makes `x == 5`, and a read after an independent write to `x` observes the new value (see §10.4). An `addr_of` pointer **may be passed into a callee**, which reads and writes the caller's place for real — the out-parameter idiom, and valid C (C11 6.2.4p6: a call does not end the caller's block). The interpreters reach the caller's environment through the **env shelf** (§10.4); the native raw-pointer codegen increment (§10.5) makes native `addr_of` a real `lea`. **`L0459`** is now reserved for a *genuinely dangling* pointer — one whose block or function has already ended (a real program error, undefined behaviour in C) — plus the one residue of a pointer handed to another **thread**, which runs its own interpreter and address space. See §10.4.
- **`ptr_cast<U>(p ptr<T>) -> ptr<U>`** — reinterpret the element type (no value conversion). `ptr_to_int`/`int_to_ptr` (delivered) round-trip a pointer to/from an integer address.
- **`ptr_null<T>() -> ptr<T>`** and **`is_null(p)`** (the latter delivered) — the null pointer and its test. There is no implicit null: a `ptr<T>` is never checked for you (that is the point of `unsafe`), but `is_null` is available where you want it.
- Interpreter behavior: on the AST/IR/bytecode interpreters a `ptr<T>` from `alloc`/`int_to_ptr` is a heap-slot handle (delivered semantics). **As delivered (§10.4),** `addr_of` introduces a second *byte-addressed* address space so `ptr_offset`/`ptr_read` walk place-backed regions and the size law `ptr_to_int(ptr_offset(p, 1)) - ptr_to_int(p) == size_of(T)` holds; `ptr_cast` is the identity on the address (a static-only pointee reinterpretation). Aliasing through an `addr_of` pointer *is* modelled — the region names the original place, so reads and writes update and observe it — for the pointer's frame; an escaped pointer is refused (`L0459`), never approximated. What remains a native-codegen concern is byte-exact *reinterpretation* of storage (reading an `i64`'s bytes through a `ptr<byte>`), since the interpreters address typed cells rather than raw bytes.

---

## 3. Inline assembly

This is the headline fork. Inline `asm` is *already delivered* in the crudest
possible form (`native_backend_contract.md` §"The `asm` surface"): a statement that
takes a comma-separated list of `i64` byte literals and emits them verbatim into
`.text` (`asm 72, 199, 192, 42, 0, 0, 0` = `mov rax, 42`), gated behind `unsafe`
(`L0330`), shape-checked (`L0425`), interpreter-rejected (`L0425`). That is enough to
*prove the pipeline* (the `asm_mov.lby` fixture exits 42) but is **not production
inline assembly**: it has no mnemonics, no operand binding, no register allocation
awareness, no clobber model. A kernel author writing `lgdt`, `cli`, `wrmsr`, or a
context switch cannot hand-assemble bytes and manually thread values through
registers. This section designs the real surface; the delivered raw-byte form is
retained as the lowest-level escape (`asm_bytes`).

**OWNER DECISION NEEDED — inline-assembly surface.**

| Option | Shape | Trade-offs |
| :-- | :-- | :-- |
| **A. Rust-style `asm!` template with operand constraints** | a template string + `in`/`out`/`inout`/`clobber` operand bindings | Proven design (Rust/LLVM), gives the compiler a real model of which registers are read/written/clobbered so it can allocate around the block correctly and keep surrounding codegen valid. Most powerful; safest composition with the register allocator. Costs a template-string mini-language (`{0}`, `{name}`) that is slightly un-Lullaby (string-embedded metasyntax) and a parser for operand specs. **Recommended (adapted to indentation-only, see below).** |
| B. Lullaby-native indentation block, one instruction per line, operands bound by name | `asm` block; each line is a mnemonic + operands; Lullaby locals referenced directly | Reads the most Lullaby-like (no template string, no `{}` placeholders; indentation-only). But it requires the compiler to own a full assembler (mnemonic → encoding) for every instruction a kernel needs — a very large, target-specific surface to build and keep correct — and to define operand/clobber binding anyway. High implementation cost; easy to ship an incomplete assembler that silently lacks an instruction. |
| C. Intrinsics-only (no general `asm`) | expose `cli()`, `sti()`, `hlt()`, `lgdt(p)`, `rdmsr(n)`, … as builtins; no free-form asm | Simplest, fully modelled, no assembler. But it can never cover the long tail (a custom context-switch sequence, a vendor instruction) — a kernel *will* need an instruction the stdlib didn't foresee, and then the author is stuck. Good as a *convenience layer on top of* a general mechanism, not as the only mechanism. |

**Recommendation: Option A — a Rust/LLVM-style `asm!` with operand constraints,
adapted to Lullaby's indentation-only, colon-free surface — with Option C's common
intrinsics layered on top (§4) and Option B's per-line block explicitly rejected as
too costly to make correct.** Rationale: the operand/clobber model is the load-bearing
part (it is what lets the compiler emit correct code *around* the asm and lets the
author pass Lullaby values in and out safely); a template that forwards to the
existing assembler/`rust-lld` path avoids Lullaby owning a full x86-64 assembler; and
the delivered raw-byte `asm_bytes` remains as the final escape for an instruction the
template layer can't yet encode.

### 3.1 Proposed `asm` surface (indentation-only adaptation of Option A)

Because `#` is the comment character and there are no braces or colons, the operand
bindings are an **indentation block of clauses**, not a `: : :`-delimited string tail:

```lby
no-runtime

# Disable interrupts (no operands, but clobbers "memory" ordering).
fn disable_interrupts
    unsafe
        asm "cli"
            clobber mem

# Read a model-specific register: ecx = msr index in, edx:eax out -> u64.
fn read_msr index u32 -> u64
    unsafe
        let mut hi u32 = 0u32
        let mut lo u32 = 0u32
        asm "rdmsr"
            in  ecx = index
            out eax = lo
            out edx = hi
            clobber mem
        (to_u64(hi) << 32) | to_u64(lo)

# Write a byte to an I/O port (see also the port intrinsics in §4).
fn outb port u16, value byte
    unsafe
        asm "out dx, al"
            in dx = port
            in al = value
            clobber mem
```

- **`asm "<template>"`** — the mnemonic/template string is a normal Lullaby string
  literal. It may contain positional/named placeholders resolved from the operand
  block (`asm "mov {dst}, {src}"` with `out reg = dst` / `in reg = src`), matching
  Rust's `{name}` scheme; for register-fixed instructions (`rdmsr`, `out dx, al`) the
  template names the architectural registers directly and the operand block *binds*
  them to Lullaby values.
- **Operand clauses** (each on its own indented line under the `asm`):
  - `in <reg-or-name> = <expr>` — load `expr` into the operand before the block.
  - `out <reg-or-name> = <lvalue>` — store the operand into the Lullaby lvalue after.
  - `inout <reg-or-name> = <lvalue>` — read-modify-write.
  - `clobber <reg|mem|cc>` — declares a register / memory / condition-code clobber so
    the compiler does not assume those survive the block. `clobber mem` is a compiler
    barrier (ordering fence) around the asm.
  - Constraint spelling: a fixed register name (`eax`, `dx`) pins the operand; a
    class name (`reg`, `reg32`) lets the compiler choose. **OWNER DECISION** on
    whether 1.0 ships *only* fixed-register operands (simplest: the author names the
    register, the compiler just moves values in/out and honours clobbers) or also the
    register-class allocator form (`reg`). **Recommend fixed-register-only for the
    first increment** — it covers essentially all kernel instructions (which use
    architectural registers anyway), needs no register allocator integration, and the
    `reg` class form is a clean later addition.
- **`asm_bytes <b0>, <b1>, …`** — the delivered raw-byte statement, renamed for
  clarity (or kept as `asm` with a numeric argument list distinguished from the
  string form). Retained as the lowest-level escape: an instruction the template
  layer cannot yet encode can always be emitted as bytes. `L0425` (shape) and the
  interpreter-rejection behavior are unchanged.
- **`unsafe` + interpreter behavior:** `asm`/`asm_bytes` remain `unsafe`-only
  (`L0330`) and remain **native-only** — the interpreters cannot execute machine code
  and reject any `asm` with `L0425` (delivered). New operand/clobber *shape* errors
  (unknown register, `out` to a non-lvalue, malformed template placeholder) are a
  check-time proposed **`L0443`**.
- **Divergence:** a trailing `asm` that leaves the return value in the ABI return
  register is treated as divergent-like and satisfies a non-void function's
  final-value requirement (delivered behavior, kept).

### 3.2 Why not Option B (per-line native assembler)

Owning a correct, complete x86-64 (and AArch64) assembler is a multi-month subsystem
that must encode every addressing mode a kernel touches, and shipping it *incomplete*
means a kernel author hits "unknown mnemonic" on a routine instruction. The template
form forwards the instruction text to the same assembler path the toolchain already
uses (the native backend already produces `.text` bytes and, for the linker path,
drives `rust-lld`), so Lullaby models only *operands and clobbers* — the part that
actually needs language integration — not the full instruction set.

---

## 4. Volatile / MMIO, port I/O, and privileged instructions

> **DELIVERED (2026-07-16) — MMIO and port I/O.** The MMIO half of this section
> needed **no implementation at all**: it composes from the already-delivered
> `int_to_ptr` + `ptr_offset` + `volatile_store` + void functions, and is now
> *pinned by a test* rather than merely assumed to work. The port-I/O half —
> `port_in8/16/32`, `port_out8/16/32` — is implemented and lowers to real x86
> `in`/`out`. The **privileged** set (`read_cr`/`write_cr`, `read_msr`/`write_msr`,
> `halt`/`cli`/`sti`, `invlpg`) remains undelivered. See the "MMIO and port-I/O
> delivery" note at the end of this section (§4.1) for the as-built record,
> including the diagnostic codes actually assigned.

These are the hardware edge. Volatile load/store already ship (`volatile_load`/
`volatile_store`, delivered, `unsafe`-gated). Port I/O and privileged-register access
are new. They are exposed as **`unsafe` intrinsics** (Option C from §3, layered on the
general `asm` so the long tail is still reachable).

**OWNER DECISION NEEDED — MMIO / port-I/O / privileged access as intrinsics vs raw `asm`.**

- **A. Named intrinsics for the common operations, `asm` for the rest (recommended).**
  Ship `volatile_load`/`volatile_store` (done), `port_in8/16/32` + `port_out8/16/32`,
  and a small privileged set (`read_cr`/`write_cr`, `read_msr`/`write_msr`,
  `halt`/`cli`/`sti`, `invlpg`). Modelled, testable, self-documenting; the `asm`
  template (§3) covers anything not pre-named. Best ergonomics + completeness.
- B. `asm`-only (no intrinsics). Smallest surface, but every driver re-hand-writes
  `out dx, al`, and the compiler can't give a nice signature/diagnostic for a wrong
  port width. Rejected — too raw for the "gentler than C" story.
- C. A `volatile`/`mmio` *qualifier* on `ptr<T>` so ordinary `ptr_read`/`ptr_write`
  through a qualified pointer are automatically volatile. Elegant, but adds a type
  qualifier to the type system (a larger change) and hides the volatility at the use
  site. Consider post-1.0; for 1.0 keep volatility *explicit at the call*.

**Recommendation: Option A.** Concrete surface (all `unsafe`, all `no-runtime`-
available, all allocation-free and control-flow-free):

```lby
no-runtime

# --- MMIO (delivered volatile builtins) ---
fn vga_put cell_index i64, ch u16
    unsafe
        let base ptr<u16> = int_to_ptr(0xB8000)      # VGA text buffer
        volatile_store(ptr_offset(base, cell_index), ch)

# --- Port I/O ---
fn serial_write_byte b byte
    unsafe
        port_out8(0x3F8, b)                          # COM1 data register

fn serial_ready -> bool
    unsafe
        (port_in8(0x3F8 + 5) & 0x20) != 0            # line-status register, THR-empty bit

# --- Privileged registers / instructions ---
fn enable_paging pml4 ptr<byte>
    unsafe
        write_cr(3, ptr_to_int(pml4))                # CR3 = PML4 physical base
        write_cr(0, read_cr(0) | 0x80000000)         # set CR0.PG
```

Intrinsic signatures (proposed):

- **MMIO:** `volatile_load(p ptr<T>) -> T`, `volatile_store(p ptr<T>, v T) -> void`
  (delivered). On native, no elision/reordering; on interpreters, plain heap load/
  store over the single-threaded abstract heap (delivered, correct).
- **Port I/O:** `port_in8(port u16) -> u8`, `port_in16 -> u16`, `port_in32 -> u32`,
  and `port_out8(port u16, v u8)`, `port_out16`, `port_out32`. Lower to `in`/`out`
  on x86-64. Not meaningful on AArch64 (MMIO-only architecture) → a `no-runtime`
  program using `port_*` targeted at AArch64 is proposed **`L0444`** (*"port I/O is
  x86-only; use MMIO (`volatile_*`) on this target"*).
- **Privileged:** `read_cr(n i64) -> i64` / `write_cr(n i64, v i64)` (control
  registers, `n` a compile-time constant 0/2/3/4/8), `read_msr(i u32) -> u64` /
  `write_msr(i u32, v u64)`, `halt()`, `cli()`, `sti()`, `invlpg(addr i64)`. Each
  lowers to the fixed instruction (or an `asm` template internally).
- **Interpreter behavior:** port I/O and privileged intrinsics touch real hardware and
  cannot be executed on the interpreters — they are **native-only** and rejected on
  the interpreters with `L0444` (mirroring how `asm`/`extern` are native-only), so a
  cross-backend fixture never claims to have "run" them. MMIO via `volatile_*` *is*
  interpretable (delivered) because it maps to the abstract heap.

### 4.1 MMIO and port-I/O delivery (2026-07-16) — the as-built record

Delivered exactly on the Option A recommendation above, with the spellings this
section already fixed. What follows is what actually shipped, including the two
places the as-built differs from the proposal.

**MMIO: delivered by composition, no intrinsics added.** This section listed
`volatile_load`/`volatile_store` as "delivered" and it was right — but the *whole
MMIO idiom* already worked natively and was simply never tested. The canonical
VGA poke compiles today in a `no-runtime` module under `lullaby native
--freestanding` into a **1536-byte direct PE** with no C runtime:

```lby
no-runtime

fn vga_put off i64 ch i64
    unsafe
        let base ptr<i64> = int_to_ptr(753664)   # 0xB8000
        volatile_store(ptr_offset(base, off), ch)
```

It needs **no MMIO intrinsics of its own**: a device register mapped into the
address space *is* memory, so `int_to_ptr` + `ptr_offset` + `volatile_store` +
void functions compose to reach it. Option C of this section's fork (a
`volatile`/`mmio` pointer qualifier) therefore remains correctly deferred — there
is nothing it would unblock. Now pinned by `mmio_vga_poke_compiles_freestanding_native`
(`crates/lullaby_cli/tests/cli/suite15.rs`) over
`tests/fixtures/valid/no_runtime/freestanding_mmio_vga.lby`, **compile-only**:
`0xB8000` is unmapped in a user-mode process, so the exe is emitted and never run.

**Port I/O: delivered as specified.** The six builtins ship with the exact
spelling and signatures proposed above — plain call syntax, no turbofish, the
width baked into the name:

```text
port_in8(port u16)  -> u8       port_out8(port u16, value u8)   -> void
port_in16(port u16) -> u16      port_out16(port u16, value u16) -> void
port_in32(port u16) -> u32      port_out32(port u16, value u32) -> void
```

Port I/O is the one hardware surface pointers **cannot** synthesize: the x86 I/O
port space is a separate address space that only `in`/`out` reach.

- **Typing.** The port is `u16` (the architectural space is exactly `0..=0xFFFF`,
  carried in `DX`); the data width is fixed by the builtin's name and is
  **unsigned** in both directions (a port value is a raw device byte/word/dword
  with no sign to extend). Lullaby has no implicit coercion, so a literal port is
  written with the delivered typed-literal suffix — `port_out8(0x3F8u16, b)` — or
  `to_u16(...)`. That is deliberate: a silently-truncated port number drives the
  wrong device. Typing lives in `crates/lullaby_semantics/src/semantics_port_io.rs`.
- **Gating.** `unsafe`-gated reusing **`L0330`**, exactly as §2.2 anticipated
  ("the raw-pointer operations reuse that same `L0330`"). **Available in
  `no-runtime`**: `semantics_no_runtime.rs` needed **no change** — its `L0441`
  gate rejects heap/runtime *types* and host-allocator builtins, and a port
  builtin names only `u8`/`u16`/`u32`, allocates nothing, and adds no hidden
  control flow. It is kernel core, like `ptr_read`/`volatile_*`. Pinned by
  `port_io_is_available_in_a_no_runtime_module`.
- **Native codegen.** `crates/lullaby_ir/src/native_object_portio.rs` emits both
  port forms (immediate `imm8` for constant ports `0..=255`, `DX` for the full
  range) × all three widths × read/write, with the `0x66` operand-size prefix on
  the 16-bit forms only, and renormalizes each read into the 8-byte cell
  (zero-extending, via the shared `emit_normalize_rax`). The full encoding table
  is in [native_backend_contract.md](native_backend_contract.md) §"Port-Mapped
  I/O". There is no 64-bit port I/O — the architecture caps port access at 32
  bits, which is why the surface stops at 32.
- **Verified by bytes, NOT by execution.** `in`/`out` are privileged: they fault
  (general protection) at CPL 3 unless IOPL/the TSS I/O bitmap allows them, and no
  device sits behind a port in a test harness. Running one would crash the harness
  rather than verify it. So `crates/lullaby_ir/src/native_object_portio_tests.rs`
  (15 tests) asserts all twelve encodings plus the prefix rules, the `imm8`↔`DX`
  boundary at 255, per-read renormalization, and operand staging;
  `port_io_compiles_freestanding_native` compiles a real 2560-byte direct PE
  (compile-only) from `tests/fixtures/valid/no_runtime/freestanding_port_io.lby`.

**Diagnostic codes — two deviations from this document's proposals.** The
registry's tail had moved on since this document was written (`L0440` was assigned
to an unrelated `match`-arm error, not the proposed `no-runtime`-without-
`--freestanding` warning), so the proposed codes were re-checked rather than taken
on faith:

- **`L0444`** is used as this section proposed — port I/O cannot run on an
  interpreter — but its scope is **broader than the AArch64 note here suggested**.
  It is a *runtime* refusal covering every interpreter, and it is also what an
  AArch64/WASM build ends up raising: those backends do not know the port names,
  so the function skips cleanly (`L0339` / unsupported-builtin) and falls back to
  an interpreter, which refuses with `L0444`. One code, one meaning ("port I/O
  cannot execute here"), no backend edits needed.
- **`L0442`** is used for **port/data width errors**, not for the `unsafe` gate.
  §2.2 released it ("was not needed... reserved at most for the undelivered
  MMIO/port stages"), and this is that stage. A dedicated code lets the message
  explain *why* the width is fixed and point at the typed-literal suffix, which
  the generic argument-type codes cannot.

Both are registered in [diagnostic_registry.md](diagnostic_registry.md); the
`registry_sync.rs` guard enforces it.

**The honest acceptance divergence.** The three interpreters **refuse** port I/O
with `L0444` rather than invent a device value — a fabricated read would silently
mis-drive a PIC/PIT/UART, which is far worse than a loud refusal. So **no parity
is claimed** for port I/O: native compiles it, the interpreters decline to define
it, framed exactly like the cross-frame `addr_of` divergence in §10.4. MMIO, by
contrast, *is* interpretable (`volatile_*` maps onto the abstract heap) and keeps
full four-tier parity.

**Still undelivered in this section:** the privileged set (`read_cr`/`write_cr`,
`read_msr`/`write_msr`, `halt`/`cli`/`sti`, `invlpg`). Those need either the `asm`
template (§3) or their own fixed-instruction lowerings, and `read_cr`/`write_cr`
additionally need a compile-time-constant register number — a check with no
precedent in the delivered builtins.

---

## 5. Static-buffer-backed arenas

> **DELIVERED (2026-07-16).** `region <name> in <buffer>` + `arena_alloc(region, count)`
> ship and **run natively**, giving a `no-runtime` module its first bounded,
> allocator-free storage. Overflow is a defined, deterministic edge (`ud2`, the same
> edge as the native bounds check) — the seam §8's pluggable panic handler replaces.
> The arena has **full four-tier parity** — it runs on the three interpreters too,
> reusing the same place-backed `addr_of` machinery (an arena cell is an ordinary
> `array<i64>` element, so there is nothing to reinterpret). **Four premises in the
> text below are wrong about the delivered compiler** (there is no `region` *block*; implicit arena allocation is vacuous in
> this tier; the buffer is `array<i64>` bumping in 8-byte cells, not `array<byte>`) —
> see the as-built record in §5.2 for what shipped and why.

The canonical region model (`execution_tiers_and_1_0_scope.md`) says the freestanding
tier uses "the same `region` surface, but arenas are backed by a caller-provided
static buffer (no host allocator)". This is the feature that lets *most of a kernel
stay arena-safe* — bounds-checked, no use-after-free, no manual `free` — while only
the hardware edge drops to raw pointers.

### 5.1 The model

In the safe tier, an implicit function/loop arena or an explicit `region` block gets
memory from the host allocator and **grows a new chunk on overflow**
(`__lullaby_arena_alloc`). In `no-runtime`, there is no host allocator, so an arena
must be handed a fixed byte buffer up front and **fail to the panic handler on
overflow** (never grow). Everything else about arenas — bump allocation, bulk reset at
scope exit, value-semantic auto-copy on escape *within the buffer* — is identical, so
safe arena code ports to the kernel unchanged except for where the memory comes from.

**OWNER DECISION NEEDED — how a `region` binds to a caller-provided static buffer.**

| Option | Shape | Trade-offs |
| :-- | :-- | :-- |
| **A. `region … in <buffer>` clause on the explicit region block** | `region scratch in kernel_heap` where `kernel_heap` is a `static` byte buffer | Reuses the *exact* delivered `region` block grammar, adds one `in <buffer>` clause; the buffer is visibly the backing store; overflow policy is attached to the region. Greppable, minimal new surface. **Recommended.** |
| B. A distinct `arena` construct separate from `region` | `arena a from buf size 4096` | Clearer that this is fixed-buffer (not growable), but forks the region grammar into two constructs users must learn, and breaks the "same `region` surface" promise in the canonical doc. |
| C. Implicit — the whole `no-runtime` module shares one linker-provided heap section | no per-region binding; `region` blocks draw from a global `.kernel_heap` | Zero per-site syntax, but hides *which* buffer backs an allocation and makes overflow a global rather than local concern — worse for a kernel that wants separate bounded pools (per-CPU, per-subsystem). |

**Recommendation: Option A** — an `in <buffer>` clause on the delivered `region`
block, with an explicit `static` buffer the author declares and sizes. The buffer is
an ordinary Lullaby value (a `static` fixed `array<byte>` or a `ptr<byte>` + length),
so it can live in `.bss`, be a linker-provided region, or be a stack buffer.

```lby
no-runtime

# A fixed, statically-sized backing buffer in .bss (see §7/§9 for section control).
static kernel_scratch array<byte> = array_fill(64 * 1024, 0b)

fn handle_request req ptr<Request> -> i64
    # A bounded arena carved from the static buffer; bulk-freed at dedent.
    region work in kernel_scratch
        # Safe, bounds-checked, arena-allocated code — NO host allocator involved.
        let parsed Parsed = parse(req)           # allocations bump into `kernel_scratch`
        let n i64 = summarize(parsed)
        n
    # `work` is reset here at dedent: the bump pointer rewinds to the region base.
    # kernel_scratch is reusable for the next request. No per-object free.
```

- **`region <name> in <buffer>`** — opens a bump arena over `<buffer>` (a `static`
  `array<byte>`, or `region <name> from <ptr>, <len>` for a raw pointer + length).
  Allocations inside bump within the buffer; at dedent the region is reset (bump
  pointer rewinds — a single O(1) reset, no per-object work).
- **Overflow → the user panic handler (§8), never grow, never `malloc`.** When a
  bump would exceed the buffer, the freestanding `__lullaby_arena_alloc` calls the
  registered panic handler (proposed reason `arena_overflow`) instead of requesting
  a new chunk. This is the *only* allocation-failure path, and it is explicit and
  user-controlled — satisfying "no hidden allocation" (the buffer is caller-provided
  and bounded) and "no hidden control flow" (overflow is a visible call to the
  author's handler).
- **Nesting & per-CPU pools:** regions nest (a nested `region … in other_buffer`
  carves a sub-arena from a different buffer), so a kernel can keep separate bounded
  pools (per-CPU scratch, a DMA pool, a request pool) each with its own overflow
  behavior.
- **Escape policy is unchanged but bounded:** value-semantic auto-copy on escape
  copies into the *enclosing* region — which, in `no-runtime`, is another fixed
  buffer, so an escape that would exceed it hits the same overflow → panic path.
  `ref<T>` (a genuinely shared/dynamic owner) is *not* available (no RC); data that
  must outlive all buffers is expressed with raw `ptr<T>` under `unsafe`.
- **Implementation:** the freestanding `__lullaby_arena_alloc` / `__lullaby_arena_reset`
  helpers (already named in the canonical region contract) take the buffer base+limit
  from the region descriptor instead of the host allocator; overflow branches to the
  panic-handler symbol (§8) rather than the grow path. A malformed binding (buffer not
  `static`/sized, or a non-byte buffer) is proposed **`L0445`**.


### 5.2 Static-buffer arena delivery (2026-07-16) — the as-built record

Delivered on this section's Option A recommendation — `region <name> in <buffer>`,
memory from a caller-provided buffer, bump allocation, a defined overflow edge, no
host allocator. **Four of this section's stated premises turned out to be wrong
about the delivered compiler**, and each forced a documented deviation rather than
an unbuildable literal reading. What follows is what actually shipped.

**Deviation 1 — there is no `region` *block* to reuse.** This section says Option A
"reuses the *exact* delivered `region` block grammar, adds one `in <buffer>`
clause". It does not exist. The delivered `region` is a flat **metadata
declaration** — `region NAME: size=N, align=N, kind=static|dynamic,
mutable=true|false` — which is validated (`L0340`/`L0341`), lowered to a
`region_create` marker for memory analysis, and has **no block, no scoping, and no
allocation behaviour whatsoever**. So the arena form is a **statement in that same
delivered statement position**, scoped to its enclosing block:

```lby
no-runtime

fn two_cells -> i64
    let buf array<i64> = [0, 0, 0, 0, 0, 0, 0, 0]   # the caller's buffer
    region scratch in buf                            # a bump arena over it
    unsafe
        let a ptr<i64> = arena_alloc(scratch, 1)
        let b ptr<i64> = arena_alloc(scratch, 1)
        ptr_write(a, 30)
        ptr_write(b, 12)
        ptr_read(a) + ptr_read(b)                    # 42 — distinct cells
```

"Reset at dedent" is exactly what an enclosing-block scope gives for a region
declared at the top of a block, so nothing is lost.

**Deviation 2 — implicit arena allocation is *vacuous* in `no-runtime`.** This
section's sketch has the region block's body allocate implicitly (`let parsed Parsed
= parse(req)` bumping into the buffer). Nothing would. Every type whose allocation
the escape analysis arena-izes — `list`/`string`/`map`/`rc` — is `L0441`-rejected in
this tier (§1.1); what remains is by-value scalars, structs, and fixed arrays, none
of which heap-allocate. An implicit bump would have literally nothing to bump. So
allocation is **explicit**, via `arena_alloc(region, count) -> ptr<T>` — which is
also what a kernel actually writes.

**Deviation 3 — the buffer is `array<i64>` and the bump unit is the 8-byte CELL,
not the byte.** This section spells the buffer `static ... array<byte>` with
byte-granular bumping. The delivered arena bumps 8-byte cells over an `array<i64>`
instead, which makes every pointer it returns exactly as well-formed as an
`addr_of(buf[i])` — the delivered, tested kernel idiom it reuses wholesale.

The reason is `arena_alloc`'s own contract: it yields a `ptr<T>` for an **8-byte
pointee** (`i64`/`u64`/`isize`/`usize`), so the buffer it bumps must be made of
8-byte cells for the returned pointer's stride and its storage to agree. A buffer of
any other element width is refused (`L0445`), and the native backend gates on the
element's **byte width** rather than its word count — a packed `array<u8>` reports a
word count of 1 and would otherwise pass and be over-run 8x.

> **Note (superseded reasoning).** An earlier version of this deviation justified
> the `array<i64>` buffer by claiming an `array<byte>` "is an array of 8-byte cells
> rather than packed bytes", and that native `addr_of` "lowers for 8-byte scalars
> only". **Both are now false.** Narrow-element arrays ARE packed at their C width
> (`array<u8>` is one byte per element) and `addr_of(a[i])` into one IS lowered —
> see "Narrow array elements are PACKED" in `native_backend_contract.md`. What
> remains true, and is all this deviation actually needs, is that a narrow *scalar*
> is still a normalized 8-byte cell and that an arena's pointee is 8 bytes. A
> byte-granular arena is therefore no longer blocked by the value model; it is
> simply not a 1.0 item.

**Deviation 4 — the buffer is a function-LOCAL `array<i64>`, not a `static`.** §5's
example declares `static kernel_scratch array<byte> = array_fill(64 * 1024, 0b)` at
module scope. Lullaby has no `static` declaration at all, so the backing buffer is an
ordinary local binding in scope. That is enough for the delivered shape — a bounded
pool per function, which is the `handle_request` example's actual structure — but it
does mean a buffer's lifetime is its frame's, not the program's. A module-scope pool
shared across calls needs `static`, a later increment that pairs naturally with §9's
`section "…"` placement (a kernel wants its pool in a named `.bss` section anyway).

**The surface, as built.**

- **`region <name> in <buffer>`** — opens a bump arena over `<buffer>`, a fixed
  `array<i64>` binding in scope with an inferable length. Scoped to the enclosing
  block. The `region <name> from <ptr>, <len>` raw-pointer variant this section also
  proposed is **not** delivered (no fixture needed it; it is a clean later add).
- **`arena_alloc(region, count) -> ptr<T>`** — bump `count` 8-byte cells, returning
  a pointer to the first. `region` is a **compile-time region name, not a value**
  (there is no `arena` type, matching how `region` names work everywhere else). `T`
  comes from the caller's annotation, defaulting to `ptr<i64>` — the delivered
  `int_to_ptr`/`ptr_cast` context rule, since the raw-pointer builtins take no
  turbofish. `unsafe`-gated with **`L0330`**, like every other raw-pointer producer.
- **Available in `no-runtime`, which is the whole point.** `semantics_no_runtime.rs`
  needed **no change**: `L0441` rejects heap/runtime *types* and host-allocator
  *builtins*, and an arena names only `array<i64>`/`ptr<T>`, allocates nothing from
  the host, and adds no hidden control flow. Pinned by
  `arena_is_available_in_a_no_runtime_module`.
- **`L0445`** — a malformed arena: the backing name is not in scope, is not a fixed
  `array<i64>`, the region name collides, **the buffer already backs another region**,
  or `arena_alloc`'s first operand does not name a declared region. The
  two-regions-one-buffer case is the sharp one: each region bumps from its own cursor
  starting at zero, so `region a in buf` + `region b in buf` would both hand out
  `&buf[0]` and silently clobber each other. It is exactly what this section's
  **per-CPU pools** motivation invites an author to write, so it is rejected rather
  than left to corrupt data — separate pools need separate buffers. This section proposed `L0445` for exactly this, and the
  registry confirmed it free. **`L0446`–`L0449` were deliberately avoided** even
  though unassigned: this document proposes them for other, undelivered sections
  (`naked fn` §6, `repr`/`align` §7, `panic fn` §8, `section` §9).

**Overflow: `ud2`, and the §8 seam.** A bump that would leave the buffer **traps
with `ud2`** — the same instruction and reasoning as the delivered native bounds
check for an out-of-range `buf[i]`. It is immediate, deterministic, and can never
hand back a pointer past the buffer's end; a null or truncated pointer would be a
silent wrong answer corrupting memory at some later, unrelated point. Pinned by
`arena_overflow_hits_the_panic_edge_natively`, which asserts what is actually
observable — the process dies on the trap (`0xC000001D`,
`STATUS_ILLEGAL_INSTRUCTION`, verified) rather than exiting, and specifically does
**not** produce the value the program would have returned had the overflowing
allocation wrongly succeeded. It cannot hang.

**§8 (the pluggable panic handler) is the next increment, and this is its seam.**
When it lands, `emit_arena_overflow_trap` calls the program's `panic fn` with
`kind = arena_overflow` instead of trapping. That one function changes; the range
check, the cursor, and the bump are already in their final form. Per decision **A5**
the handler still terminates and does **not** unwind, so the shape of the edge (a
divergent tail with no return path) is unchanged by that work.

**Native implementation.** An arena is three things and no runtime: the author's
`array<i64>` local (the arena adds no storage), **one dedicated frame word per
region** holding the bump cursor as a cell count (reserved by `NativeCtx::plan`,
collected by `collect_arena_regions` in `native_object_frame.rs` — the module split
out for exactly this), and the bump itself. `arena_alloc` emits: load the cursor,
`add` the count, `jc` on unsigned wrap, `cmp`/`ja` against the buffer's length,
commit, then `lea` the element address. The carry check is not decoration — `count`
is a signed `i64` the author supplies, so a huge or negative value could wrap back
into range and hand out a pointer *before* the buffer; both checks are unsigned, so
a negative count reads as an enormous one and is caught. A zero-cell request is
well-defined: it commits nothing and returns the current position. The prologue
**zeroes each cursor** (`emit_arena_cursor_init`) — a frame slot holds whatever the
last call left there. Code lives in `crates/lullaby_ir/src/native_object_arena.rs`.

**Composition with the arena-FIRST escape analysis: disjoint by construction.** That
analysis (`native_object_eligibility.rs`) is where a per-iteration sub-region rewind
freeing a still-live cell is a demonstrated miscompile class, so the interaction was
checked rather than assumed. There is none, structurally: it governs the **host heap
bump pointer** (`__lullaby_heap_next`), which a static-buffer arena never reads or
writes — the arena's memory is the author's local and its cursor is a private frame
word no rewind knows about.

**Separate storage is the whole argument, and it is enough.** An earlier draft added
a third leg — "the two are gated on disjoint programs, since every heap-touching
construct is `L0441`-rejected where arenas live". That is **false**: the arena
surface is deliberately **not** gated to `no-runtime` (like `unsafe` and the
raw-pointer builtins, it is available in both tiers), so a safe-tier function may
legitimately mix an arena with heap work and arena-first eligibility. Such a program
compiles and runs correctly, and always did — the conclusion never rested on that
leg. It is removed rather than repaired: a proof with a fictional leg is worse than
a shorter true one.

**No conservative exclusion was needed**, and none was made. Functions without an
arena emit byte-identical code (`arena_buffers` is empty for them), so the
escape-analysis fuzzers stay green.

**Full four-tier parity — and the refusal that was wrong.** The arena runs on the
AST, IR, and bytecode interpreters as well as native, all agreeing on `109`.

**How bytecode gets there: a deliberate tree-walker fallback**, the same shape §10.4
documents for `addr_of`. `VmCompiler::compile_expr` refuses both arena markers
(`arena_region`, `arena_alloc`) **by name**, so any function using an arena runs on
the tree-walker.

This is **deliberately unimplemented, not impossible** — a distinction this document
gets wrong once already (the `L0460` refusal below claimed an impossibility the
mechanism disproved) and must not repeat, because a false structural claim here
forecloses a real option for the next implementer:

- `arena_region`'s operands are lowered as **string literals** (`IrExprKind::String`),
  so they compile to `VmOp::PushConst(Value::String(..))` and reach
  `dispatch_named_call` fully intact — measured with a temporary arm that echoes its
  arguments: `args=[String("pool"), String("buf")]`. `VmOp::FieldLocal(usize, String)`
  already carries a name in the op stream, so a `VmOp::ArenaRegion(String, String)` is
  constructible with existing machinery.
- The shipped bug corroborates this: it was `L0401 unknown **function**
  'arena_region'`, not `L0403 unknown **variable**`. The arguments evaluated and the
  name arrived; only the arm was missing.
- The **real** obstacle is that `dispatch_named_call(&mut self, name, args:
  Vec<Value>)` has no `env` access, while arena state is a name-keyed env binding
  (`arena_key`). A dedicated op or env plumbing would be needed.
- Only `arena_alloc` carries a bare `Variable` region operand, and that is a
  *lowering choice* — `arena_region` disproves any claim that a region name cannot
  survive the flat stream.

The choice not to build it is a cost/benefit one: the arena is a freestanding-tier
declaration rather than a hot path, and the tree-walker already models it exactly. It
is reversible by anyone who wants the VM to carry arenas natively.

This refusal is written down because for one increment it was **accidental**, and the
gap it left was real. `arena_alloc`'s bare region operand resolved to no VM slot, so
`compile_expr` happened to fail on its own and the function fell back — arena
programs worked, but by luck. A `region` declared and never allocated from has no
such operand: it compiled cleanly, reached `VmOp::Call`, and hit a
`dispatch_named_call` with no `arena_region` arm, raising `L0401 unknown function`
on bytecode while `check`, AST, IR and native all returned `0`. Every fixture and
every generated program had paired a region with an allocation, so nothing saw it.
`freestanding_arena_scoping.lby`'s `region_without_alloc` and the fuzzer's
`region_no_alloc` shape now cover it.

This is a **correction to the first version of this increment**, and the reasoning
matters. That version refused `arena_alloc` on all three interpreters with `L0460`,
arguing their place-backed, typed-cell pointer model could not reinterpret a
buffer's storage as freshly-typed cells. But **deviation 3 above had already
destroyed that argument**: once the arena bumps in whole 8-byte cells of an
`array<i64>`, an arena cell *is* an ordinary buffer element, so `arena_alloc(r, n)`
is exactly `addr_of(buf[cursor])` plus an integer cursor — and every interpreter
defines both halves. There was nothing to reinterpret and therefore nothing to
refuse. The registry entry was self-refuting: its own remedy told the reader to "use
a fixed `array<i64>` directly with `addr_of(buf[i])` + `ptr_offset`, which every
interpreter does define" — the arena's own mechanism.

The distinction is the point. **`L0459` earns its refusal by refusing the
impossible**: the interpreters genuinely cannot reach another frame's locals. The old
`L0460` refused something they demonstrably *could* do, which makes it work avoided
wearing a limitation's clothes. So the arena is implemented on all three
interpreters (reusing the shared `addr_of` machinery wholesale — the returned
pointer is the same place-backed pointer, and genuinely aliases the buffer), and
`L0460` is retargeted to **arena overflow**, the failure that genuinely exists.

**Nothing about the arena is native-only — including across frames.** An earlier
version of this record said a pointer escaping its buffer's frame was refused by the
interpreters with `L0459`, inherited from `addr_of` rather than added. The inheritance
was real, but the **env shelf** (`651d22c`) has since made a bare name resolve across
frames, so passing an arena pointer into a callee — valid C, since a call does not end
the caller's block — now **runs on every tier** and agrees (measured: `99` on all
four). The paragraph is retired rather than reworded: there is no arena-specific
divergence left to describe.

That retirement is a claim about a *closed* set of shapes, so the set is named. It
rests on `freestanding_arena_scoping.lby` and the `fuzz_arena_scoping_matches_across_tiers`
net agreeing on all four tiers for: a loop-body region's reset at dedent, a shadowed
buffer, an arena pointer crossing the call ABI, and a region declared but never
allocated from. That last shape is listed because the retirement was first written
**while it was still broken** — every fixture and generated program then paired a
region with an allocation, the `L0401` described above went unseen, and the claim was
made on an untested shape. It is honest now; it was not then. A genuinely *dangling*
pointer (its buffer's frame has returned) is still `L0459` on the interpreters and
real undefined behaviour natively, exactly as for any `addr_of` pointer — see the
lifetime note below.

**One scoping model, and the three ways it was silently three.** The arena's scope and
buffer binding are now defined once and shared:

- a `region <name> in <buffer>` is **scoped to its enclosing block**, and
- its backing buffer is **pinned at the declaration**, to a slot — never re-resolved
  by name later.

Each tier had drifted to its own model, and every drift was a *silent wrong answer*
with no diagnostic:

| Shape | Was | Root cause |
| :-- | :-- | :-- |
| `region` in a loop body | interpreters `300`, **native `123`** | Native zeroed the cursor only in the **prologue**, so it never re-zeroed on block re-entry — contradicting this document's own "reset at dedent". It now zeroes **at the declaration site**, which is inside the loop body. |
| `region` in an inner block, used after | `check` **accepted**; interpreters `L0445`, native `7` | The checker tracked regions in a flat **per-function** map. `arena_regions` now lives on `Scope`, which is cloned per block — so `check` no longer accepts what the tiers reject. |
| buffer shadowed by an inner `let` | interpreters `0`, **native `7`** | The interpreters re-resolved the buffer **by name** on every allocation, so a shadowing `let buf` silently retargeted a live arena. They now hold the buffer's `RootSlot`, pinned at the declaration. |

All three are `406` on all four tiers (`freestanding_arena_scoping.lby`). The
slot-pinning also **immunizes the arena against the env shelf**: with a name-keyed
buffer, the shelf's cross-frame resolution could have reached a *caller's* buffer
entirely. (A callee naming a buffer it does not own is rejected by the checker —
`L0445` — so the shelf has no route in.)

**Lifetime: an arena pointer does not outlive its buffer.** The arena's memory *is*
the buffer, so a pointer into it is valid exactly as long as the buffer's binding —
its enclosing frame. Natively, using one after that frame returns reads whatever now
occupies the stack (measured: a stale `42`), which is **real undefined behaviour,
precisely as the equivalent C is** and as the delivered `addr_of` already documents
(`L0459`, case 1). It is `unsafe`-gated for exactly this reason; the interpreters
diagnose it rather than read stale storage. Do not return an arena pointer from the
function that owns the buffer.

**Verification.** `tests/fixtures/valid/no_runtime/freestanding_arena_alloc.lby`
compiles under `lullaby native --freestanding` to a real direct-PE exe (no linker)
and **runs, exiting 109** — `two_cells` (42: distinct allocations do not alias) +
`loop_sum` (60: the arena composes with a loop) + `block` (7: a multi-cell request
walked with `ptr_offset`) — and the AST, IR, and bytecode interpreters all produce
`109` too. `freestanding_arena_overflow.lby` traps natively (`0xC000001D`) and aborts
with `L0460` on the three interpreters. `freestanding_arena_scoping.lby` pins the one
scoping model at **406 on all four tiers** — a loop-body region resetting at dedent, a
shadowing `let` failing to retarget a live arena, and an arena pointer crossing the
call ABI. Six negative fixtures under `tests/fixtures/invalid/no_runtime/` pin `L0445`
(×5: backing not in scope, wrong type, unknown region, two regions over one buffer,
and a region used after its block) and `L0330`. Tests in `crates/lullaby_cli/tests/cli/suite15.rs`; the differential
fuzzer is `crates/lullaby_cli/tests/cli/fuzz_arena.rs`.

That fuzzer carries **both** oracles: a cross-engine
differential (native must equal the interpreters, and the three must agree) and the
**generator's own arithmetic** (so a bug corrupting every tier identically still
fails). It straddles the buffer's capacity so an exactly-full allocation must succeed while one
cell past it must trap. Its teeth are **measured, not assumed**: removing the cursor
zeroing and weakening the range check from `ja` to `jae` each make it fail, and it
passes against the real implementation. Notably, the generator's first version had
**no teeth for the cursor zeroing** — a process's initial stack pages are already
zero, so an arena called straight from `main` read a zero cursor by luck and the
injected bug passed. The generator now calls a stack-dirtying function first, which
is what makes the zeroing observable.

**Still undelivered in this section:** the `region <name> from <ptr>, <len>`
raw-pointer binding, per-CPU pools sharing one buffer (two regions over one buffer is
`L0445` — separate pools need separate buffers; nesting *distinct* buffers works via
ordinary block scoping), explicit mid-scope reset, and the escape-copy interaction (vacuous today, since the
escaping values that would be copied are `L0441`-rejected in this tier).

---

## 6. Interrupt handlers & naked functions

A kernel must declare functions the CPU calls directly on an interrupt/exception —
these need a **different prologue/epilogue** (save *all* registers, end with `iret`
not `ret`, and on some vectors consume an error code) — and sometimes a **`naked`**
function with *no* compiler-generated prologue at all (the author writes the entire
body in `asm`, e.g. the very first boot entry or a context switch).

**OWNER DECISION NEEDED — ISR declaration form.**

| Option | Shape | Trade-offs |
| :-- | :-- | :-- |
| **A. Prefix keywords `interrupt fn` / `naked fn` (recommended)** | `interrupt fn timer_isr frame ptr<IntFrame>` ; `naked fn boot_entry` | Consistent with the delivered `export fn` / `extern fn` / `pub` prefix-keyword pattern; the calling convention is visible at the declaration; the compiler generates the correct ISR prologue/epilogue (`interrupt`) or none (`naked`). Two new prefix keywords. **Recommended.** |
| B. An attribute/decorator line above the `fn` | `@interrupt` / `@naked` on the preceding line | Groups with the `repr`/section attributes (§7/§9) if those also use `@`. But `#` is the comment char so an attribute sigil must be chosen (`@`), and a decorator line is a second way to modify a declaration alongside the existing prefix keywords — two mechanisms for one job. |
| C. Convention-only (a normal `fn` whose ABI is set by how the IDT entry is built) | no language marker; the author writes a `naked`-style trampoline by hand | Zero new surface, but the compiler can't generate the register-save prologue, so *every* ISR is hand-written asm — throwing away the "arena-safe kernel" advantage exactly where interrupts (a huge fraction of kernel code) live. |

**Recommendation: Option A** — `interrupt fn` and `naked fn` prefix keywords,
matching the delivered `export`/`extern` prefixes.

```lby
no-runtime

struct IntFrame
    repr c                 # C layout so the CPU-pushed frame matches (see §7)
    ip   u64
    cs   u64
    flags u64
    sp   u64
    ss   u64

# A hardware interrupt handler. The compiler emits the ISR prologue (push all GPRs,
# align stack), runs the body, then the ISR epilogue (restore GPRs, `iret`).
interrupt fn timer_isr frame ptr<IntFrame>
    unsafe
        tick()
        port_out8(0x20, 0x20b)          # EOI to the PIC
    # no `ret`/`iret` written by hand — the epilogue supplies `iret`

# An exception that carries a CPU error code (a distinct convention).
interrupt fn page_fault frame ptr<IntFrame>, error_code u64
    unsafe
        handle_fault(read_cr(2), error_code)   # CR2 = faulting address

# A naked function: NO compiler prologue/epilogue; the whole body is asm.
naked fn _boot
    unsafe
        asm "mov rsp, {stack_top}"
            in stack_top = ptr_to_int(addr_of(boot_stack_top))
        asm "call kmain"
        asm "hlt"
```

- **`interrupt fn`** — the compiler generates the platform ISR calling convention:
  save/restore the full register set, correct stack alignment, and terminate with
  `iret` (x86-64) instead of `ret`. The handler receives a pointer to the CPU-pushed
  interrupt frame (a `repr c` struct the author defines, §7); a second `error_code`
  parameter selects the error-code-pushing vector convention. Body is ordinary
  Lullaby (it can be *entirely safe* arena/value code — only the device pokes are
  `unsafe`), which is the arena-safe-ISR advantage.
- **`naked fn`** — the compiler emits **no** prologue/epilogue and no implicit
  `ret`. The body must be `asm`/`asm_bytes` only (referencing Lullaby locals is
  unavailable because there is no frame); a `naked fn` with non-`asm` statements is
  proposed **`L0446`**. Used for the earliest boot entry, context switches, and
  trampolines.
- Both are `no-runtime`-tier constructs; using them in a non-`no-runtime` module is
  `L0441`. Native lowering adds two prologue/epilogue variants to the emitter; on the
  interpreters an `interrupt fn` body runs as an ordinary function (its ABI is a
  native-only concern) and a `naked fn` (asm-only) is native-only like any `asm`.

---

## 7. `repr(C)` / packed / alignment control

Lullaby *already* lays structs out in **C-natural layout** by default
(`lullaby_memory_management.md` §"Raw-Memory Layout Intrinsics": fields in
declaration order, each aligned to its natural alignment, size rounded to the struct
alignment). So `repr(C)` is *already the default* — the new capabilities the kernel
needs are **`packed`** (remove padding, for hardware register blocks and on-wire
structures) and **explicit alignment** (over-align a struct to a cache line or a page).

**OWNER DECISION NEEDED — layout-attribute spelling.**

Rust's `#[repr(C, packed)]` / `#[repr(align(N))]` is impossible verbatim: `#` is the
comment character. The options that fit Lullaby's colon-free, brace-free, prefix-keyword
grammar:

| Option | Shape | Trade-offs |
| :-- | :-- | :-- |
| **A. A `repr` header line inside the struct body (recommended)** | first indented line of the struct is `repr c packed align 16` | No sigil clash (it is a keyword line, not `#[...]`); reads top-of-struct like a field but is clearly a layout clause; extends naturally (`repr c`, `repr c packed`, `repr c align 64`). Matches indentation-only structure. One new contextual keyword (`repr`) + `packed`/`align`. **Recommended.** |
| B. Trailing modifiers on the `struct` line | `struct Regs packed align 16` | Terse and prefix-keyword-consistent (`export fn`-style). But the `struct` line then mixes the name with layout modifiers, and a long modifier list crowds the declaration line; less room to grow. |
| C. A `@`-decorator line above the struct | `@repr(c, packed)` then `struct Regs` | Familiar to Rust/Python users, but introduces a decorator sigil (`@`) and a bracketed `(…)` mini-syntax that exists nowhere else in Lullaby, enlarging the grammar for one feature. |

**Recommendation: Option A** — a `repr` clause as the struct body's first line.

```lby
no-runtime

# Packed hardware register block: no padding, C field order, matches the device.
struct UartRegs
    repr c packed
    data       u8     # offset 0
    int_enable u8     # offset 1  (no padding because packed)
    fifo_ctrl  u8     # offset 2
    line_ctrl  u8     # offset 3

# Over-aligned to a 4 KiB page (a page table).
struct PageTable
    repr c align 4096
    entries array<u64>     # 512 entries; the struct is page-aligned

# Default (no repr line) is already C-natural layout — repr c is implicit.
struct Point
    x f64
    y f64
```

- **`repr c`** — C-compatible layout (already the default; stating it is
  documentation + a guarantee against a future default change). This is the only
  `repr` needed for 1.0; a Lullaby-native/reorderable `repr` is a post-1.0 option.
- **`packed`** — remove inter-field padding (fields at their natural byte offsets
  with no alignment padding; struct alignment 1). Reading a misaligned field through a
  `ptr<T>` is the author's responsibility (matches C's packed footgun; `volatile_*`
  and `ptr_read` still work byte-wise).
- **`align <N>`** — over-align the struct to `N` bytes (`N` a power of two ≥ the
  struct's natural alignment). Under-aligning below natural is `L0447`.
- These fold into the delivered layout engine (`size_of`/`align_of`/`offset_of`
  already compute C-natural layout; `packed`/`align` adjust the padding/alignment
  inputs). `packed` + `align` together, or an illegal `align`, are `L0447`. Available
  in *both* tiers (a safe-tier FFI struct also wants `repr c packed`), but load-bearing
  for `no-runtime`.

---

## 8. User panic handler for bounds/safety failures

The safe tier's bounds check calls `panic → abort`/`panic → supervisor`. A kernel has
no OS to abort to, so — exactly like Rust's `#[panic_handler]` — a `no-runtime`
program **must register its own panic handler**, and the bounds-check machinery calls
*it* on failure. The canonical doc requires the bounds check be "parameterizable with
a user panic hook so the same machinery serves the freestanding tier".

**OWNER DECISION NEEDED — how the panic handler is registered.**

| Option | Shape | Trade-offs |
| :-- | :-- | :-- |
| **A. A designated `panic fn` with a fixed, well-known symbol (recommended)** | `panic fn on_panic info ptr<PanicInfo> -> never` | One obvious construct; the compiler wires the bounds-check failure edge to call this symbol; a `no-runtime` program without exactly one `panic fn` is a hard error (like Rust's "no `#[panic_handler]`"). Greppable, mirrors the established `interrupt fn`/`naked fn` prefix-keyword pattern. **Recommended.** |
| B. Runtime registration via a builtin | `set_panic_handler(my_handler)` called from `main` | Flexible (swap handlers), but the handler is then reached through a function pointer the bounds check must load — hidden indirection and a window before registration where a panic has nowhere to go. Worse for the "no hidden control flow" rule. |
| C. A magic `export fn` by name convention | `export fn __lullaby_panic(...)` | No new keyword, reuses `export`, but relies on a magic string name (fragile, unobvious) and doesn't read as "this is the panic handler". |

**Recommendation: Option A** — a `panic fn` with a `never`-returning signature.

```lby
no-runtime

struct PanicInfo
    repr c
    kind   u32        # 0 = bounds, 1 = arena_overflow, 2 = assert, 3 = unreachable
    index  i64        # e.g. the out-of-range index (kind-dependent)
    length i64        # e.g. the collection length
    site   ptr<byte>  # optional source-location string (see below)

# Exactly one panic handler per freestanding program. It must not return.
panic fn on_panic info ptr<PanicInfo> -> never
    unsafe
        serial_puts("KERNEL PANIC\n")
    halt_forever()          # a `-> never` helper that ends in `hlt`/loop

fn halt_forever -> never
    unsafe
        loop
            asm "hlt"
```

- **`panic fn on_panic info ptr<PanicInfo> -> never`** — the return type is a new
  divergent type **`never`** (a value-less bottom type; `-> never` means "does not
  return"). The compiler requires a `panic fn` to be `never`-returning: a bounds
  failure cannot resume the faulting operation, so the handler must halt, reset, or
  loop. (Spelling: `never` vs `!` — `!` is the reserved error-throw token
  [lullaby_error_handling.md](lullaby_error_handling.md), so `never` avoids the clash.
  **OWNER DECISION** on `never` vs `noreturn`.)
- **What the bounds-check machinery calls:** the delivered/planned array-index bounds
  check (whose failure currently aborts) is made **parameterizable** — its failure
  edge builds a `PanicInfo` (kind = bounds, `index`, `length`) and calls the
  `on_panic` symbol. The same edge serves arena overflow (§5, kind = arena_overflow),
  `assert` failures, and reaching an `unreachable`. This is the single machinery the
  canonical doc requires to serve both tiers: safe tier wires the failure edge to
  `abort`/supervisor; `no-runtime` wires it to `on_panic`.
- **Enforcement:** a `no-runtime` program with **zero** `panic fn` is proposed
  **`L0448`** (*"a `no-runtime` program must define exactly one `panic fn`"*); two or
  more is also `L0448`. A `panic fn` in a non-`no-runtime` module is `L0441` (the safe
  tier uses `panic → supervisor`). `PanicInfo`'s exact fields are part of the ABI and
  fixed by this design (a `repr c` struct so a handler can read it byte-stably).
- **No unwinding:** consistent with `lullaby_error_handling.md`'s let-it-crash model,
  a panic in `no-runtime` does **not** unwind through frames (that would be hidden
  control flow and needs no runtime tables) — it is a *direct call* to `on_panic`,
  which diverges. Expected, recoverable failures still use `result<T,E>`/`?` (which
  need no runtime and are fully available).

---

## 9. Freestanding binary output

Much of this is **already delivered** (`native_backend_contract.md`): the
`--freestanding`/`--no-std` flag, the no-CRT guarantee (`L0426` rejects `extern fn`
under `--freestanding`), the direct-PE writer (`write_pe_executable`, no external
linker), and freestanding `_start` stubs for ELF/Mach-O/AArch64 that call `main` and
issue a raw `exit` syscall with no libc. What the *kernel* tier adds on top is
**author control over the entry symbol, the sections, and true flat/ELF kernel-image
emission** (an ELF the bootloader or `objcopy` turns into a flat binary), plus the
freestanding-`main` conventions.

### 9.1 Custom entry symbol

A hosted program's entry is the compiler's `_start`/`_lullaby_start` stub that calls
`main` and exits. A kernel's entry is *its own* symbol (`_start`, `kmain`, a
multiboot header target) that the bootloader jumps to, and it must **not** be wrapped
in an exit-syscall stub (there is nothing to exit to).

**OWNER DECISION NEEDED — designating the freestanding entry.**

| Option | Shape | Trade-offs |
| :-- | :-- | :-- |
| **A. An `entry fn` prefix keyword (recommended)** | `entry fn _start` marks the raw entry; the compiler emits it at the image entry point with no exit-stub wrapper | Consistent with `export`/`extern`/`interrupt`/`naked` prefix keywords; the entry is greppable and unmistakable; the compiler knows to *not* synthesize the hosted `_start` stub and to place this symbol at `AddressOfEntryPoint`/`e_entry`. **Recommended.** |
| B. A CLI flag `--entry <symbol>` | `lullaby native --freestanding --entry kmain` | No syntax, and it mirrors the linker `/entry:` option already used. But the entry is then invisible in source, and the compiler still needs to know *not* to wrap it in the exit stub — a property better attached to the declaration. Keep `--entry` as an override, not the primary mechanism. |
| C. Convention (`main` is always the entry) | reuse the delivered `main` + `_start` stub | Simplest, but the delivered stub *exits the process* after `main` returns — wrong for a kernel `main` that must never return, and it forces the bootloader-facing symbol to be named `main`. Insufficient for real boot. |

**Recommendation: Option A** — an `entry fn` keyword, with `--entry` retained as a
CLI override.

```lby
no-runtime

# The bootloader jumps here directly. No exit stub, no CRT. Must not return.
entry fn _start -> never
    unsafe
        asm "mov rsp, {top}"
            in top = ptr_to_int(addr_of(boot_stack_top))
    kmain()
    halt_forever()
```

### 9.2 Section control

A kernel needs specific symbols in specific sections: a multiboot/boot header in an
early section, the entry in `.text.boot`, page tables in a page-aligned `.bss`, a
`.stack` region. Section placement pairs with a linker script (which the author
supplies; Lullaby drives output, not the final memory map for a kernel).

**OWNER DECISION NEEDED — per-symbol section placement.** Recommend a **`section
"<name>"` prefix clause** on the declaration, consistent with the prefix-keyword
family and directly mapping to the object writers' existing per-section model
(COFF/ELF/Mach-O sections already exist in `object_model.rs`):

```lby
no-runtime

section ".text.boot" entry fn _start -> never
    ...

section ".bss.pagetables" static pml4 PageTable = zeroed()

section ".multiboot" static header MultibootHeader = multiboot_header()
```

- `section "<name>"` prepends a target section name; the object writers already
  emit named sections and per-section relocations, so this threads the author's name
  through to the section header. An unknown/ill-formed section name is proposed
  **`L0449`**. (Alternative considered: a `@section(...)` decorator — rejected for the
  same sigil/`(…)` reason as §7 Option C.)

### 9.3 Direct-ELF and flat-binary emission

- **Direct-ELF (executable, not relocatable):** the delivered path emits a
  *relocatable* ELF object (`ET_REL`, linked by `rust-lld`) plus a freestanding
  `_start` for hosted Linux exit. The kernel tier needs a **directly written
  executable/loadable ELF** (`ET_EXEC` or `ET_DYN`) with author-controlled program
  headers and entry — the ELF analogue of the delivered direct-PE writer
  (`write_pe_executable`). Recommendation: extend the direct-image writer that already
  produces PE32+ in-house to also lay a **fixed-base ELF executable** around the same
  `.text`/`.rodata`/`.bss` (the neutral `ObjectModel` already carries all three),
  resolving every intra-image reference at emit time (exactly as the PE writer does),
  with the entry at the `entry fn` symbol and no interpreter/`PT_INTERP`. This keeps
  the compile-speed moat (no external linker) on the ELF kernel path too.
- **Flat binary:** for boot sectors / early stages that run before ELF is parsed, emit
  a **raw flat binary** — the `.text`+`.rodata`+`.bss`(zeroed) laid out contiguously
  at a fixed load address (`--load-addr`), no headers at all, entry at offset 0. This
  is the simplest writer (concatenate the already-final sections at the load base and
  resolve REL32s), reusing the same emit-time relocation resolution.
- **CLI:** `lullaby native --freestanding --format elf-exec|flat [--load-addr 0x100000] [--entry _start] -o kernel.bin`. `--format elf-exec` and `--format flat` join the delivered `--target` container selection; they are *output-image* formats, not object containers. The direct-PE default is unchanged.

Relationship to the delivered direct-PE writer: this is the **same technique**
(fixed-base image, all references resolved at emit time, no external linker) applied
to two more output shapes (ELF-exec and flat). The PE writer proves the approach; the
kernel formats extend it.

---

## 10. Implementation sketch + staged plan

**No code is written by this proposal.** This is the subsystem-impact map and a
staged, production-complete increment plan (each stage independently shippable, fully
tested including negative cases, deterministic, and doc-complete before it lands — per
the Production Quality Standard). It reuses delivered machinery wherever possible.

### 10.1 Subsystems touched

- **Lexer:** new keywords/directives `no-runtime`, `interrupt`, `naked`, `panic`
  (as `panic fn`), `entry`, `section`, `repr`, `packed`, `align`, `never`, `in`
  (region clause — may already tokenize), and the `asm` operand clause words
  (`in`/`out`/`inout`/`clobber`). Reserve each with an `L0211`-style "planned syntax"
  rejection until its stage lands, so partial rollout never mis-parses.
- **Parser / AST:** the `no-runtime` module directive; `asm` string-template statement
  with an operand-clause block (extend the delivered raw-byte `asm` node); the `repr`
  struct-header clause; `region … in <buffer>` clause on the region node;
  `interrupt fn`/`naked fn`/`entry fn`/`panic fn`/`section "…"` declaration modifiers;
  `never` return type; the `ptr_*`/`port_*`/`read_cr`/etc. intrinsics parse as
  ordinary calls (no grammar change). `formal_grammar.md` gains the new productions.
- **Type system / semantics:** tier gating (§1) — mark the module `no-runtime`,
  reject unavailable constructs (`L0441`); classify allocation/control-flow sites
  against the two hard rules (§1.3, reusing the escape + drop-insertion passes);
  `never` as a bottom type that unifies with any expected type and satisfies the
  final-value/return requirement; `panic fn` uniqueness + `never` signature (`L0448`);
  `interrupt fn`/`naked fn` signature constraints (`L0446`); `repr`/`packed`/`align`
  validity feeding the delivered layout engine (`L0447`); `asm` operand/clobber shape
  (`L0443`); MMIO/port/privileged intrinsic signatures + `unsafe` gate (`L0442`/
  `L0444`); `addr_of` addressability (delivered as `L0458`), `ptr_offset`/`ptr_cast`
  typing (delivered; unsized pointee `L0431`, `unsafe` gate `L0330`).
- **IR:** an `asm` op carrying (template, operands, clobbers) — a superset of the
  delivered raw-byte op; a `region-in-buffer` variant on the region-enter op; a
  bounds-check-failure edge parameterized to call the `panic fn` symbol; `interrupt`/
  `naked`/`entry` function-kind tags; section-name tags on symbols. Region-enter/reset
  ops already exist.
- **Native x86-64 emitter:** the `asm` template lowering (fixed-register operand
  moves + clobber honoring, forwarding the instruction text to the assembler path);
  `port_in/out` (`in`/`out`), `read_cr`/`write_cr`/`read_msr`/`write_msr`/`cli`/`sti`/
  `hlt`/`invlpg` instruction lowering; the ISR prologue/epilogue (save-all + `iret`,
  with/without error code); the `naked` no-prologue path; the freestanding
  static-buffer `__lullaby_arena_alloc`/`__lullaby_arena_reset` (buffer base+limit,
  overflow→panic symbol); the parameterized bounds-check-failure call to `on_panic`.
  `volatile_load`/`volatile_store` already lower.
- **Object/exe writers:** author-named sections threaded through the delivered
  COFF/ELF/Mach-O writers and neutral `ObjectModel`; a **direct-ELF-executable**
  writer (the ELF analogue of `write_pe_executable`); a **flat-binary** writer;
  `entry fn`/`--entry`/`--load-addr` wiring; the `no-runtime` build path that skips the
  hosted `_start` exit-stub synthesis.
- **Interpreters (AST/IR/bytecode):** most `no-runtime` code (safe arena/value logic,
  `ptr<T>` over the abstract heap, `volatile_*`, `repr` layout, `region … in buffer`
  modeled as a bounded abstract arena, `panic fn` as an ordinary divergent function)
  runs on the interpreters, so tier logic and the panic path are cross-backend
  testable. `asm`/`port_*`/privileged intrinsics/`interrupt`/`naked` are **native-only**
  and rejected on the interpreters (`L0425`/`L0444`), mirroring `asm`/`extern` today —
  so a parity fixture never claims to have "run" hardware.

### 10.2 Staged, production-complete increment plan

Headline: **land the tier gate and the safe-arena-kernel core first (so most of a
kernel is expressible and testable on the interpreters), then the hardware edge
(asm/MMIO/ISR), then the kernel output formats — each stage a complete, tested
increment.**

1. **Tier gate + enforcement.** ✅ **DELIVERED (2026-07-15).** The `no-runtime` module
   directive; unavailability of RC/actors/growable-heap (`L0441`); the two-hard-rules
   classifier over the existing escape/drop passes; `never` type. Value-neutral for
   existing programs. Verified by fixtures that a `no-runtime` module rejects
   `rc`/`spawn`/`list.push` and accepts scalar/struct/array/`region` code, on all three
   interpreters. (`never`, and the two-hard-rules classification driven by the
   escape/drop *pass annotations*, arrive with their consuming stages — the panic
   handler in stage 3 and static-buffer arenas in stage 2; stage 1 enforces the
   allowed/rejected boundary directly over the AST + checked expression types, which is
   sufficient and complete for the gate.)
2. **Static-buffer arenas.** ✅ **DELIVERED (2026-07-16)** — `region <name> in
   <buffer>` + `arena_alloc(region, count)`, a per-region frame bump cursor, and a
   defined overflow edge (`ud2` natively, `L0460` on the interpreters; §8 makes both
   call the user handler). **Full four-tier parity.** See §5.2 for the as-built
   record, including four deviations forced by wrong premises in §5's text.
3. **User panic handler + parameterized bounds check.** `panic fn`/`PanicInfo`/
   `L0448`; wire the bounds-check-failure edge (and arena overflow, `assert`,
   `unreachable`) to call it. Negative fixtures: out-of-range index calls `on_panic`
   with the right `PanicInfo`; missing/duplicate handler is `L0448`. Cross-backend.
4. **Raw-pointer surface completion.** ✅ **DELIVERED (2026-07-16)** for the core
   addressing trio `addr_of`/`ptr_offset`/`ptr_cast` (extending delivered
   `ptr_read`/`ptr_write`/`ptr_to_int`), `unsafe` gating (reusing `L0330`),
   interpreter byte-addressed semantics with the size law. See §10.4. The extra
   convenience spellings `ptr_offset_bytes`/`ptr_null`/`is_null` and native lowering
   are a later increment (native raw-pointer codegen does not yet exist for *any* of
   the raw-pointer builtins — a function using them cleanly skips to the interpreters).
5. **`repr`/`packed`/`align`.** Layout-engine extension + `L0447`; `size_of`/
   `align_of`/`offset_of` reflect packed/over-aligned layouts. Layout fixtures across
   backends (interpreters compute layout too).
6. **Inline `asm` (real).** The string-template + operand-clause form (fixed-register
   operands first), clobber honoring, `L0443`; retain `asm_bytes`. Native-only; the
   `asm_mov`-class fixtures extend to operand binding (e.g. a `rdmsr`/`outb` fixture
   run natively).
7. **MMIO / port I/O / privileged intrinsics.** `port_*`, `read_cr`/`write_cr`/
   `read_msr`/`write_msr`/`cli`/`sti`/`hlt`/`invlpg`; `L0444` on the interpreters and
   on AArch64 for port I/O. Native lowering + structural encoding tests.
8. **ISR / naked functions.** `interrupt fn` (prologue/epilogue + `iret`, error-code
   variant) and `naked fn` (`L0446`); native-only lowering; interpreter runs
   `interrupt fn` bodies as ordinary functions.
9. **Freestanding output control.** `entry fn`/`--entry`, `section "…"` (`L0449`),
   the direct-ELF-executable writer, the flat-binary writer, `--format elf-exec|flat`,
   `--load-addr`. Structural writer tests (parse the image back), and — where a runner
   exists — a boot-and-check smoke test (e.g. the worked example §11 under an emulator).
10. **Post-1.0 refinements** (above the 1.0 line): register-class (`reg`) asm operands,
    a `volatile`/`mmio` pointer qualifier (§4 Option C), a Lullaby-native reorderable
    `repr`, richer linker-script integration, more targets. Surfaces stay stable.

Stages 1–9 are the freestanding 1.0 deliverable (the canonical 10-item checklist);
stage 10 is spot convenience only.

### 10.3 Stage 1 delivery (2026-07-15)

The tier gate and its enforcement are implemented and test-locked. What shipped:

- **Directive & lexing.** `no-runtime` is the one hyphenated Lullaby keyword. The
  lexer (`crates/lullaby_lexer/src/lib.rs`) recognizes the exact contiguous spelling
  `no-runtime` as a single `Keyword::NoRuntime` token; a bare `no`/`runtime` and a
  spaced `no - runtime` subtraction remain ordinary identifiers/operators.
- **Parser & AST.** The parser (`crates/lullaby_parser/src/lib.rs`) accepts the
  directive only as the first non-comment line (before any `import`/declaration);
  a later occurrence is an `L0201` misplacement. The flag rides on
  `Program::is_no_runtime` (serde-defaulted, so existing artifacts stay valid), the
  formatter re-emits it, and the module loader marks the merged unit `no-runtime`
  if **any** module opts in (conservative default-deny; per-module granularity in
  mixed-tier projects is a later stage).
- **Enforcement.** `crates/lullaby_semantics/src/semantics_no_runtime.rs` runs after
  the main checker (so it can consult the recorded per-expression types) and, only
  for a `no-runtime` module, emits `L0441` for: a heap/runtime **type** anywhere in
  a signature/field/payload/alias/const spelling (`list`/`map`/`string`/`rc`/`ref`/
  `Future`/`Actor`, nesting-aware); an **actor** declaration, `spawn`, `tell`,
  `await`, or `async fn`; a **closure literal**; the host-allocator builtins
  `alloc`/`dealloc`; and any **expression whose value type** is one of those
  heap/runtime types (catching string building and collection builders without an
  annotation). A module without the directive is entirely unaffected.
- **Allowed core (verified to run).** Scalars, fixed `array<T>`, structs/enums over
  scalar fields, `option`/`result`, control flow, functions, and the raw hardware
  surface (`unsafe` + raw `ptr<T>` + `ptr_read`/`ptr_write`/`volatile_*`/`int_to_ptr`/
  `ptr_to_int`) all type-check and run on the three interpreters, and a scalar-`main`
  `no-runtime` module still compiles under `lullaby native --freestanding`.
- **Tests.** `crates/lullaby_cli/tests/cli/suite15.rs` plus the fixtures under
  `tests/fixtures/valid/no_runtime/` and `tests/fixtures/invalid/no_runtime/`, and
  lexer unit tests for the hyphenated keyword.

Explicitly **not** in stage 1 (later freestanding stages, unchanged from §10.2):
static-buffer-backed arenas (§5), the pluggable `panic fn`/`never`/parameterized
bounds check (§8), the completed raw-pointer surface `addr_of`/`ptr_offset`/`ptr_cast`
(§2.2), `repr`/`packed`/`align` (§7), real inline `asm` operand binding (§3),
MMIO/port-IO/privileged intrinsics (§4), `interrupt`/`naked` functions (§6), and
`entry`/`section`/direct-ELF/flat-binary output (§9). Stage 1 deliberately does **not**
reject the raw/`unsafe`/`ptr` primitives that already exist — they are the kernel core.

### 10.4 Raw-pointer addressing delivery (2026-07-16)

The core addressing trio `addr_of` / `ptr_offset` / `ptr_cast` (§2.2) is implemented
and test-locked, extending the delivered `ptr_read`/`ptr_write`/`ptr_to_int`/
`int_to_ptr`/`volatile_*` surface. What shipped:

- **Surface (the delivered subset of §2.2).**
  - `addr_of(place) -> ptr<T>` — the address of an addressable place: a local
    (`Variable`), an array element (`Index`), or a struct field (`Field`). A
    whole-array place decays to `ptr<T>` (a pointer to its element type), matching C
    array decay, so `addr_of(a)` and `addr_of(a[0])` agree. The place's type `T` must
    have a defined C-natural layout.
  - `ptr_offset(p: ptr<T>, n: isize) -> ptr<T>` — element-scaled arithmetic:
    `result = base + n * size_of(T)`. `n` is a signed `isize`/`i64` element count
    (negative walks backward). **`size_of(T)` scaling rule:** the scale factor is the
    C-natural `size_of` of the pointee `T` (`i8`/`u8`/`bool`/`byte` = 1, `i16`/`u16`
    = 2, `i32`/`u32`/`f32`/`char` = 4, `i64`/`u64`/`f64`/any pointer = 8, a struct/
    fixed `array<T>` its computed layout size). **Supported `T`:** any type with a
    defined layout — scalars, pointer/reference handles, structs, and fixed
    `array<T>`; an *unsized* pointee (a `string`/`list`/`map`/growable value) is
    rejected with `L0431`.
  - `ptr_cast(p: ptr<T>) -> ptr<U>` — reinterpret the pointee type with no value
    conversion and no address change. The target `U` comes from the caller's expected
    annotation when it is a **modern** raw pointer (mirroring `int_to_ptr`), defaulting
    to `ptr<i64>` when there is none. **Spelling choice:** the design sketch in §2.2
    shows a turbofish `ptr_cast<byte>(p)`; the delivered raw-pointer builtins take no
    turbofish, so — as the minimal consistent form — the target element type is
    supplied by the `let bp ptr<byte> = ptr_cast(base)` context, exactly as
    `int_to_ptr` already resolves its pointee.
    **The pointer model comes from the operand, not the annotation.** `ptr_cast`
    retargets a pointee *within* a model; it never crosses the `ptr_T`-box/`ptr<T>`-address
    divide that `L0303`/`L0313` enforce everywhere else. A `ptr_T` operand yields exactly
    `ptr_T` (identity — a box is one opaque cell), and a legacy `ptr_U` annotation cannot
    capture a `ptr<T>` address. This is what makes `native_object_rawptr.rs`'s
    `is_legacy_box_pointer` *spelling* test sound as a whole-program property: a
    `ptr_T`-typed expression really is always `alloc`-derived. The native
    `refuse_legacy_box_pointer` gate stays as defense in depth.
- **`unsafe` gating (both tiers).** All three are raw-pointer operations: using one
  outside an `unsafe` block is the existing unsafe-required diagnostic `L0330`,
  identical to `ptr_read`/`int_to_ptr`. They are available in the safe tier *and* the
  `no-runtime` tier under `unsafe`.
- **`no-runtime` behavior.** They are part of the kernel core, **not** rejected by
  the tier gate (`L0441`): each yields an allowed `ptr<T>` value and is not a
  host-allocator builtin, so a `no-runtime` module freely uses them (verified by
  `tests/fixtures/valid/no_runtime/freestanding_addr_of.lby`).
- **Interpreter model (byte-addressed regions).** The delivered `ptr<T>` model is an
  abstract *heap-slot handle* with no adjacency, which cannot express arithmetic. The
  interpreters (`crates/lullaby_runtime/src/raw_pointer.rs`, shared by all three)
  therefore add a second **byte-addressed** address space above `RAW_POINTER_BASE`
  (`1 << 44`, disjoint from small heap-slot handles). A region is pure *addressing
  metadata* — a byte base, an element `stride = size_of(element)`, a cell count, and a
  stable coordinate of the addressed place; `ptr_offset` advances the byte address by
  `n * stride`, and a read/write (routed there by `ptr_read`/`ptr_write`/`volatile_*`
  when the address is in raw space) maps `addr` back to cell `(addr - base) / stride`.
  This makes the **size law** `ptr_to_int(ptr_offset(p, 1)) - ptr_to_int(p) ==
  size_of(T)` hold on the interpreters exactly as in real addressing.
- **`addr_of` is place-backed, so it genuinely aliases.** The addressed place is
  **not** copied. It is decomposed into a root binding plus a `ResolvedPlace{Field,
  Index}` path (the model ordinary assignment already uses,
  `lullaby_runtime/src/lib.rs`), and reads/writes through the pointer go straight to
  that storage. So `ptr_write(addr_of(x), 5)` makes `x == 5`, and
  `ptr_read(addr_of(x))` after an independent `x = 99` observes `99` — exactly what a
  real `lea`-based native `addr_of` does. This is the **hybrid** the stage-2 review
  sketched: *place-backed for storage, region-backed for adjacency*, so a write through
  `ptr_offset(addr_of(a[0]), i)` mutates `a[i]` for real while the size law stays
  exact. It retires the stage-2 `L0459` store refusal.
- **The root is pinned by scope id, never re-resolved by name.** A region records the
  binding's `(scope id, entry index)`; `Env` scope ids are monotonic and never reused.
  Resolving by name at access time would silently follow a nested shadowing `let` and
  address a *different* binding. Pinning means resolution either finds the exact
  binding whose address was taken, or finds nothing.
- **Cross-frame `addr_of` resolves: the env shelf (stage 4).** A pointer passed into a
  callee names the caller's live place, and the callee reads and writes that place for
  real. This is the out-parameter idiom (`scanf("%d", &x)`, `strtol(s, &end, 10)`,
  `qsort`) and it is **well-defined C, not undefined behaviour**: C11 6.2.4p6 ties an
  automatic object's lifetime to its *block*, and a call does not end the caller's
  block.

  Stage 3 refused these because each frame's locals live in that frame's own `Env` on
  the Rust stack. Stage 4 adds an **env shelf**: an interpreter-owned stack of the
  *ancestor* frames' `Env`s. At a call boundary the caller's `Env` is swapped out of
  its `&mut Env` slot and pushed onto the shelf for the dynamic extent of the call, so
  a callee reaches it by the `RootSlot::env` id the `addr_of` already recorded.
  `RawResolve::Escaped` is gone: a live region always resolves, because a returned
  frame's regions are dropped by `exit_frame`.

  **Why this shape.** Keeping the *current* frame as a plain `&mut Env` and shelving
  only ancestors is what makes it free. The two obvious alternatives both tax every
  program: `Rc<RefCell<Env>>` puts a runtime borrow check on **every variable access**,
  and one frame-id-indexed `Vec<Env>` (current frame included) puts a bounds check
  there. The shelf is touched only at a call boundary, and only once the program has
  taken an address — `RawPointerMemory::shelf_needed()` is false until the first
  `addr_of`. That gate is **sound by construction, not an optimization gamble**: with
  no live region nothing can resolve to a place, so the shelf's contents are
  unobservable. Measured cost on the interpreter hot path (interleaved A/B, fib and
  the loop, all three tiers): within the ±2% run-to-run noise floor.

  The invariant: **every live region's `Env` is either the current frame's `&mut Env`
  or on the shelf.** A region created in frame `F` keeps `shelf_needed` true for as
  long as `F` lives, so every call `F` makes shelves `F`'s environment; and `F`'s
  region cannot outlive `F`, because `exit_frame` drops it.
- **`L0459` now means genuinely dangling, and only that.** A place-backed address is
  meaningful only while its place exists. Two things end that, and both are *detected*:
  the **scope died** (the block holding the root binding was popped — `Env::at` finds
  nothing) or the **frame returned** (`exit_frame` dropped its regions). Both are
  **genuine program errors**: returning `&local`, or using a pointer to an inner-block
  or loop-body local after that block ended, is undefined behaviour in C, so refusing
  forbids no defined program. A pointer handed to **another thread** (`spawn`, an
  `async fn`, `parallel_map`) is a separate case and **not** `L0459`: each thread builds
  its own interpreter with its own `RawPointerMemory`, so the address names no region
  there and is refused as unmapped (`L0406`) — verified, not assumed.

  **Honest refusal is the invariant.** A deref has exactly two outcomes: the owning
  `Env` is found and the access touches the **real, live storage**, or it is a hard
  error (`unreachable_frame`, a deliberate **fail-closed guard** rather than an
  `unwrap`/`unreachable!()` — not expected to fire, and the reason a *missed* shelving
  site could only ever cost a loud refusal rather than a wrong answer). No path reads a
  copy. This matters because stage 2 *did* read a by-value
  snapshot and so got some cross-frame shapes right by luck while silently returning
  the old value for a genuine stale read (`addr_of(x)`; `x = 99`; `peek(p)`). A silent
  wrong answer is the failure mode this model exists to prevent, and stage 4 removes
  the refusals without reintroducing it. (This is **not** the `volatile_*` situation:
  `volatile_load`/`store` are semantically *correct*, with only an unobservable
  single-threaded optimization barrier unmodelled.)
- **No acceptance divergence remains for cross-frame `addr_of`.** The stage-3
  divergence — native compiling `poke(addr_of(x))` while the interpreters refused it —
  is retired. `tests/fixtures/valid/no_runtime/freestanding_cross_frame.lby` pins the
  out-parameter idiom, a callee read after a later independent write, two frames deep,
  a buffer walked and filled in a callee, the size law and negative offsets across the
  frame boundary at **327 on all four tiers** (native included);
  `tests/fixtures/valid/raw_ptr_cross_frame.lby` covers the surface more broadly at
  **432** on the three interpreters (its `i32` buffer puts it outside the native
  i64-scalar subset, so native skips it cleanly with `L0339`).
- **An unmapped raw address is `L0406`, not `L0459`.** An `int_to_ptr` value that
  merely lands in raw space (an MMIO register, a fixed physical address) is not an
  `addr_of` pointer and the diagnostic must not blame one: it reports an unmapped
  address, and points at a native freestanding target for real MMIO.
- **Tier coverage.** ast / ir / bytecode: **yes** (identical results; the bytecode VM
  falls back to the tree-walker for `addr_of`, which needs the place expression the
  flat op stream cannot carry, and shares the same raw-pointer space). **native /
  WASM: clean skip** — a function using any raw-pointer builtin is ineligible and
  skips (`L0339`/`L0338`), never miscompiled; this matches the *entire* existing
  raw-pointer surface (`ptr_read`/`int_to_ptr`/`volatile_*` are interpreter-only
  today). Native raw-pointer codegen — for the whole surface, not just these three —
  is a later increment.
- **New diagnostics.** `L0458` (semantic — `addr_of` of a non-addressable place /
  temporary) and `L0459` (runtime — an `addr_of` pointer dereferenced outside its
  place's lifetime: its block or function has already ended. Stage 4 narrowed this to
  genuinely dangling pointers only; a pointer merely *passed into a callee* resolves
  for real). `ptr_offset` on an unsized pointee reuses `L0431`; a non-pointer argument
  reuses `L0331`; the `unsafe` gate reuses `L0330` (the proposed `L0442` was not
  needed and remains unregistered).
- **Tests.** `crates/lullaby_cli/tests/cli/suite15.rs` (the `addr_of`/`ptr_offset`/
  `ptr_cast` run + negative tests), the `raw_pointer.rs` unit tests (region walk,
  size law, narrow-scalar stride, non-region offset), and the fixtures under
  `tests/fixtures/valid/no_runtime/freestanding_addr_of.lby`,
  `tests/fixtures/valid/raw_ptr_addressing.lby`, and
  `tests/fixtures/invalid/raw_ptr/`.

---

### 10.5 Native raw-pointer codegen delivery (2026-07-16)

**Delivered: the entire raw-pointer surface now has native x86-64 codegen.** This
was the kernel long-pole. Before this increment, *every* raw-pointer builtin made
its function native-ineligible (a clean `L0339` skip), so kernel-shaped code only
ran on a tree-walking interpreter — the freestanding tier targets bare metal, but
had no bare-metal codegen for its own defining surface. It does now.

The canonical, detailed contract is the **"Raw-Pointer Surface (Freestanding
Tier)"** section of [native_backend_contract.md](native_backend_contract.md); this
is the tier-level delivery record. (§2.2's prose describes the *surface*; this
records what the native backend does with it.)

- **Code.** `crates/lullaby_ir/src/native_object_rawptr.rs` (new): lowering for
  `addr_of`, `ptr_read`/`ptr_write`, `volatile_load`/`volatile_store`,
  `ptr_offset`, `ptr_cast`, `int_to_ptr`/`ptr_to_int`, plus the address-taken gate.
  Dispatched from the `Call` arm of `lower_native_expr`
  (`native_object_expr.rs`) ahead of the unknown-function gate;
  `plan_register_promotion` (`native_object_regalloc.rs`) consults
  `body_takes_address`. No interpreter, runtime, semantics, parser, WASM, or
  AArch64 file was touched.
- **`addr_of` is a real `lea`.** `ptr_write(addr_of(x), 5)` genuinely makes
  `x == 5` — the direct-PE exe exits **5**.
- **`L0459` was untouched by this increment** (superseded — see below). At the time
  this increment landed, the interpreters' `addr_of` snapshotted the place by value and
  so refused the store with `L0459` while native aliased correctly.

  **Superseded twice since.** Stage 3 (§10.4) made the interpreter model place-backed,
  so an in-frame store aliases there too and the store refusal is gone; stage 4 (§10.4)
  added the env shelf, so a pointer passed into a callee resolves as well. The tiers no
  longer diverge on either, and `L0459` now fires only for a genuinely dangling
  pointer. The invariant that survived every stage: **a loud refusal, never a silent
  wrong answer, on every tier.**
- **`ptr<T>` needed no eligibility change.** It was already a pointer-sized scalar
  in the native value model (`resolve_native_type` → `NativeType::I64`), so
  `ptr<T>` parameters, locals, and returns pass and return in a GPR like any `i64`.
  Verified end to end rather than assumed.
- **`volatile_*` are genuinely non-eliding, and no pass needed changing.** Every
  builtin is an `IrExprKind::Call`; the native command runs only the inliner, which
  rejects any `Call` body and only substitutes bare-variable/literal arguments; CSE,
  LICM, copy-prop, and DCE all treat a `Call` as opaque; and the backend's own
  peepholes (immediate folding, strength reduction, the SIMD/affine reduction
  detectors, register promotion) never match a `Call`. The guarantee was **verified
  and pinned** — including a test that a reduction-shaped volatile loop is not
  closed-formed away — not retrofitted.
- **The register-promotion hazard is gated explicitly.** A promoted local lives in
  `rbx`/`rsi` and has no address, so a `lea` of its dead frame slot would silently
  drop the store. `plan_register_promotion` now refuses to promote *any* local in a
  function containing an `addr_of`. (A raw-pointer chain also happens to fail the
  existing `expr_reg_promotable` type check, but that is incidental and correctness
  does not lean on it.) Proven at 145 vs. the 45 the miscompile would give.
- **Arena/RC interaction: none, by construction.** The `addr_of` gate admits only an
  8-byte integer/pointer scalar place, so no `string`/`list`/`map`/`rc`/heap-carrying
  aggregate can have its address taken natively, and the heap-escape/arena analysis
  never sees one. No conservative exclusion was needed; widening `addr_of` to
  heap-carrying places would require revisiting that analysis first.

**What compiles vs. what skips (default-deny).** Two properties of the native value
model bound soundness, and both refusals are precise `L0339` skips:

| Shape | Native |
| :-- | :-- |
| `addr_of` of an 8-byte scalar local/param (`i64`/`u64`/`isize`/`usize`/`ptr<T>`) | **compiles** (`lea`) |
| `addr_of(s.f)` / `addr_of(s.a.b)` of an 8-byte field | **compiles** (`lea`) |
| `ptr_read`/`ptr_write`/`volatile_*` over an integer/pointer pointee (1/2/4/8) | **compiles** (exact-width, sign/zero-extending) |
| `ptr_offset` over an integer/pointer pointee; `ptr_cast`; `int_to_ptr`/`ptr_to_int` | **compiles** (scaled `lea`; no-ops) |
| `ptr<T>` as a parameter / local / return | **compiles** (GPR scalar) |
| `addr_of` of an **array element** (`a[0]`, `a[i]`) or a **whole array** (decay to `ptr<element>`) | **compiles** — native stack aggregates now ASCEND in address (measured: `addr_of(pair.hi) − addr_of(pair.lo) == +8`, agreeing with `offset_of`), so `ptr_offset(addr_of(buf[0]), 1)` walks FORWARD, agreeing with C, `offset_of`, *and* the interpreters. **This is the buffer-walk kernel idiom.** A runtime index keeps the ordinary unsigned bounds-check trap |
| `addr_of` into a **fat-pointer (runtime-length) array parameter** | **skips** — the descriptor shares the *caller's* storage with no copy, sound only because the parameter is read-only; an address into it could be used to mutate the caller's array |
| `addr_of` of a **narrow array element / decay** (`array<i32>`, `array<u8>`, `array<i16>`, …) | **compiles** — narrow-element arrays are **packed** at the element's C width, so `ptr_offset` strides `size_of(T)` and lands on the next element, agreeing with C and with the interpreters' `size_of(element)` region stride. **This is what makes a driver's BYTE BUFFER walkable.** (Previously skipped: the element cell was 8 bytes while `ptr_offset` strided 4/1, so a walk desynchronized) |
| `addr_of` of a narrower-than-8-byte **scalar** | **skips** — a narrow *scalar* is still stored as a normalized 8-byte cell (only array ELEMENTS pack), so a width-correct store would corrupt its upper half. Refused by the width-agreement law: 8-byte storage vs a 4-byte pointee |
| a `bool`/`char` pointee | **skips** — a raw load could break the cell's value invariant |
| an `f64`/`f32` pointee | **skips** — the result would have to land in XMM, not `rax` |
| a struct/array pointee for `ptr_offset` | **skips** — its C stride is not guessed |
| `addr_of` of a closure-captured variable | **skips** — the capture lives in the env block, not a frame slot |
| a `void`-returning function (e.g. `fn poke p ptr<T> v T`) | **skips** — a *pre-existing, raw-pointer-independent* native gap (a plain `fn bump n i64` skips identically); it blocks the natural kernel spelling and is the cheapest next win |

**The descending-frame gap this increment documented is now CLOSED.** Native stack
aggregates lay their words out at ASCENDING (C-compatible) addresses: word `k` sits
8·k bytes *higher* than word 0 (`[rbp - (slot - 8k)]`; `[ptr + 8k]` through a
word-0 pointer), so `offset_of`-based pointer arithmetic from an `addr_of(s.f)`
pointer now DOES hold natively — `ptr_offset(addr_of(pair.lo), 1)` reaches
`pair.hi`, and `addr_of(pair.hi) - addr_of(pair.lo)` is `+8`, matching
`offset_of`. The canonical description is
[native_backend_contract.md](native_backend_contract.md) "Aggregate word order
(ASCENDING, C-compatible)"; the frame itself still grows downward per the Win64
ABI. Because the layout is now C-shaped, the array-element / whole-array `addr_of`
refusals the descending layout forced are **lifted**, which is what unblocks buffer
walking.

One honest boundary remains, and it is an *interpreter-model* boundary rather than
a layout one: walking ACROSS struct fields (`ptr_offset(addr_of(s.lo), 1)`, or the
equivalent hand-rolled `int_to_ptr(ptr_to_int(addr_of(s.a)) + offset_of(S, "b"))`)
is native-only. The interpreters model each `addr_of` place as a single-cell
snapshot region, so an inter-field walk is a program they do not define — native is
free to define it, and does. Where the interpreters DO define a program (a
read/walk within one array), native matches all three bit-for-bit. Retiring that
asymmetry means making the interpreters' `addr_of` place-backed — separate work,
tracked with the `L0459` store refusal in §2.2.

**Verification.** `crates/lullaby_ir/src/native_object_rawptr_tests.rs` (20 unit
tests over the emitted bytes — a new file, since `native_program_tests.rs` is
already past the test-file size cap), including `direction_flip_struct_fields_ascend`
(pins `addr_of(s.hi) - addr_of(s.lo) == +8` on the emitted `lea` displacements),
`addr_of_array_element_lowers_as_a_lea`,
`addr_of_whole_array_decays_to_element_zero`,
`addr_of_runtime_array_element_bounds_checks_and_computes_an_address`, and
`addr_of_into_fat_array_parameter_skips_cleanly`. `crates/lullaby_cli/tests/cli/suite15.rs`
compiles real `.exe`s through the direct-PE writer (no linker, no toolchain gate)
and asserts exit codes: **5** (the headline aliasing proof), **145** (the promotion
hazard), **71** (volatile non-elision), **47** (the size law across `i64`/`i32`/
`i16`/`byte`/`ptr<i64>` plus a negative walk), **42** (`addr_of(s.f)`
write-through), **42** (a `no-runtime` module under `lullaby native --freestanding`
passing a `ptr<i64>` in and returning one out), and **44** on all four tiers
(native == AST == IR == bytecode for the read/walk shapes the interpreters define).
Fixtures: `tests/fixtures/native_only/raw_ptr_*.lby` and
`tests/fixtures/valid/no_runtime/freestanding_native_rawptr.lby` +
`freestanding_addr_of_reads.lby`.

**Buffer walking (the ascending-layout payoff).**
`tests/fixtures/valid/no_runtime/freestanding_buffer_walk.lby` walks a buffer with
`addr_of(buf[0])` + `ptr_offset`, decays a whole array, walks from a runtime index,
and checks the size law (including a negative walk back): **124** on native and on
all three interpreters (`native_buffer_walk_matches_the_interpreters`,
`native_freestanding_buffer_walk_runs`). `freestanding_addr_of.lby` — previously
the fixture pinning the *refusal* — now compiles and returns **127** on all four
tiers (`addr_of_addressing_surface_matches_the_interpreters`): an array walked with
a runtime `ptr_offset(base, i)` in a `while` loop, the size law, a
`ptr<i64>`→`ptr<byte>`→`ptr<i64>` cast round-trip, and a struct-field read.
`freestanding_buffer_write.lby` is the write-through walk: **105** natively. (At the
time of this increment it was asserted to be *refused* with `L0459` on the three
interpreters, the snapshot model having no way to alias. Superseded: stage 3's
place-backed model made it **105 on all four tiers**, and
`freestanding_cross_frame.lby` now pins the same parity for the *cross-frame* shapes
at **327 on all four tiers** — see §10.4.)

Differential fuzz:
`fuzz_raw_pointer_reads_agree_across_interpreters` (150 programs) and
`fuzz_raw_pointer_reads_native_matches_interpreter` (100 programs) in
`crates/lullaby_cli/tests/cli/fuzz.rs` require native to either match the
interpreter exactly or skip cleanly with `L0339` and no exe.

**Next freestanding sub-stage.** Items (1) and (2) of the list below are now
**delivered**: native `void`-returning functions landed (2026-07-16), and MMIO +
port I/O landed on top of them (2026-07-16, §4.1) — MMIO purely by composition,
port I/O as real `in`/`out`. The privileged set (`read_cr`/`write_cr`,
`read_msr`/`write_msr`, `halt`/`cli`/`sti`, `invlpg`) is the remainder of §4 and
is best done *after* the `asm` template (§3), which most of it would lower
through anyway.

The highest-value next increments are, in order: (1) **static-buffer arenas**
(§5) — the biggest remaining one, and the feature that makes "most of a kernel
stays arena-safe" real rather than aspirational; a driver can now talk to
hardware but still has nowhere bounded to put its data; (2) the **panic fn**
(§8), which arenas need anyway for their overflow edge (§5's `arena_overflow`),
making it the natural follow-on rather than a parallel track; (3)
**interrupt/naked functions** (§6); then **direct-ELF/flat-binary output**
(§9.3). Sequencing note: (1) and (2) are coupled through the overflow path, and
(4) is the only one that can be built independently of the others today.

---

## 11. Worked example: a minimal freestanding "hello, VGA + serial" kernel

A complete `no-runtime` program that a bootloader can jump to: it writes a byte
pattern to the VGA text buffer (MMIO) and a string to the serial port (port I/O) from
a custom entry, backs its scratch work with a static buffer arena, and defines the
required panic handler — proving every piece composes. (Uses the recommended surface
from each fork above.)

```lby
# kernel/main.lby — a minimal freestanding kernel image.
no-runtime

# --- Static backing storage (lands in .bss via section control) ---
section ".bss.scratch"  static kscratch  array<byte> = array_fill(16 * 1024, 0b)
section ".bss.stack"    static kstack     array<byte> = array_fill(64 * 1024, 0b)

# --- MMIO: VGA text buffer at 0xB8000, cells are (char | attribute<<8) ---
fn vga_write_cell index i64, ch byte, attr byte
    unsafe
        let base ptr<u16> = int_to_ptr(0xB8000)
        let cell u16 = to_u16(ch) | (to_u16(attr) << 8)
        volatile_store(ptr_offset(base, index), cell)

fn vga_puts s string, attr byte
    let bytes array<byte> = to_bytes(s)
    let mut i i64 = 0
    while i < len(bytes)
        vga_write_cell(i, bytes[i], attr)      # bounds-checked; failure -> on_panic
        i += 1

# --- Port I/O: COM1 serial at 0x3F8 ---
fn serial_put b byte
    unsafe
        # wait for the transmitter-holding-register-empty bit (LSR bit 5)
        while (port_in8(0x3F8 + 5) & 0x20) == 0
            asm "pause"
                clobber mem
        port_out8(0x3F8, b)

fn serial_puts s string
    let bytes array<byte> = to_bytes(s)
    let mut i i64 = 0
    while i < len(bytes)
        serial_put(bytes[i])
        i += 1

# --- Kernel main: mostly SAFE arena code; only the pokes are unsafe ---
fn kmain -> never
    region boot in kscratch                    # bounded arena from the static buffer
        vga_puts("LULLABY", 0x0F0b)            # white-on-black to the screen
        serial_puts("lullaby kernel up\n")     # and to the serial console
    halt_forever()

fn halt_forever -> never
    unsafe
        loop
            asm "hlt"

# --- Custom entry: the bootloader jumps here. No CRT, no exit stub. ---
section ".text.boot"  entry fn _start -> never
    unsafe
        asm "mov rsp, {top}"                   # set up our own stack
            in top = ptr_to_int(ptr_offset(addr_of(kstack), len(kstack)))
    kmain()

# --- Required panic handler: bounds/arena/assert failures land here ---
panic fn on_panic info ptr<PanicInfo> -> never
    serial_puts("KERNEL PANIC\n")
    halt_forever()

struct PanicInfo
    repr c
    kind   u32
    index  i64
    length i64
    site   ptr<byte>
```

Build:

```
lullaby native --freestanding --format elf-exec --entry _start -o kernel.elf kernel/main.lby
# or, for a flat image loaded at 1 MiB:
lullaby native --freestanding --format flat --load-addr 0x100000 -o kernel.bin kernel/main.lby
```

What this proves composes: the `no-runtime` gate (no RC/actors/heap growth); a static-
buffer `region`; MMIO via delivered `volatile_store`; port I/O via `port_in8`/
`port_out8`; inline `asm` with an operand (`mov rsp, {top}`) and a clobber (`pause` /
`hlt`); a custom `entry fn` in a named section with no exit stub; a `panic fn` the
bounds check calls; and `repr c` layout on `PanicInfo` — the full checklist in one
image. Most of `kmain`/`vga_puts`/`serial_puts` is *safe, bounds-checked* code; only
the four hardware pokes are `unsafe` — the "arena-safe kernel, raw only at the edge"
story the canonical doc promises.

---

## 12. Open questions / risks for the owner

1. **Keyword budget.** This proposal adds `no-runtime`, `interrupt`, `naked`, `panic`
   (as `panic fn`), `entry`, `section`, `repr`, `packed`, `align`, and `never` to a
   deliberately tiny keyword set — plus the `asm` operand words and the `ptr_*`/`port_*`/
   `read_cr`-family intrinsics (builtins, not keywords). Confirm the owner is
   comfortable, or whether some (`packed`/`align` as `repr` sub-words only; `section`
   as a CLI/attribute rather than a keyword) should be trimmed. (Mirrors the same
   concern raised for the actor keywords.)
2. **`asm` operand model depth for 1.0** (§3.1): fixed-register-only vs also
   register-class (`reg`) allocation. Fixed-register covers essentially all kernel
   instructions and needs no allocator integration; the recommendation is
   fixed-register-first. Confirm that is acceptable, or whether `reg` classes must be
   in 1.0.
3. **Owning a full assembler vs forwarding instruction text** (§3.2): the recommended
   template form forwards the mnemonic to the assembler path rather than Lullaby
   encoding every instruction. Risk: the boundary between "encode in-house" (operand
   moves, clobbers) and "forward" (the instruction) must be clean; a partially-owned
   assembler is the worst outcome. Confirm the forward-the-text approach.
4. **`no-runtime` directive vs `--freestanding` flag coupling** (§1): they are
   orthogonal (semantic gate vs output guarantee). Risk of confusion — two knobs that
   *usually* go together. Mitigation: warn (`L0440`) when a `no-runtime` module is
   built without `--freestanding`. Confirm the two-knob model, or whether the directive
   should *imply* the flag.
5. **The `never` bottom type** touches the type system broadly (unification, final-
   value/return checking, exhaustiveness). It is small but load-bearing (the panic
   handler and `halt_forever` need it). Confirm `never` (vs reusing the existing
   divergent-like treatment of `throw`/trailing-`asm` without a named type).
6. **Direct-ELF-executable + flat writers** (§9.3) are new emit paths. They reuse the
   proven direct-PE technique, but a kernel image's correctness is only *fully*
   provable under an emulator (QEMU) in CI — heavier than the current structural
   writer tests. Risk: without an emulator smoke test, the kernel formats are
   structurally-verified only (like the x86-64 ELF/Mach-O objects today). Recommend a
   QEMU-gated boot-and-check test for the §11 example, gated like the Docker/arm64 and
   node/WASM tests already are.
7. **Multiboot / boot-protocol headers** (§9.2): a real x86 kernel needs a
   multiboot/multiboot2 header at a fixed early offset. Section control + a `repr c`
   `static` covers *emitting* one, but the exact required layout/checksum is the
   author's responsibility for 1.0. Confirm that is in-scope-enough, or whether a
   `multiboot_header()` helper belongs in a freestanding support module.
8. **AArch64 freestanding parity.** Port I/O is x86-only (`L0444`); MMIO, `asm`, ISR
   conventions, and `read_msr`-analogues differ on AArch64 (system registers via
   `mrs`/`msr`, exception vectors not `iret`). This proposal specifies x86-64 concretely
   and leaves the AArch64 ISR/system-register specifics to the AArch64 backend's own
   increment (the AArch64 backend already exists for the scalar core). Confirm x86-64
   leads and AArch64 freestanding follows.
9. **Diagnostic-code assignment.** The proposed codes (`L0440` warn, `L0441`–`L0449`)
   extend the current L044x tail (the registry's live tail is `L0439`, plus `L0501`/
   `L0502`/`L0601`). These are **proposals only** — this document does not edit
   `diagnostic_registry.md`; the reconciliation doc pass assigns final codes.

---

## 13. Summary of OWNER DECISION NEEDED forks

| # | Decision | Recommendation |
| :-- | :-- | :-- |
| 1 | Entering the tier / `no-runtime` gating | **Module-level `no-runtime` directive** + keep `--freestanding` as the orthogonal output contract |
| 2 | Raw-pointer deref/arith/addr-of/cast spelling | **Builtin functions** (`ptr_read`/`ptr_write`/`addr_of`/`ptr_offset`/`ptr_cast`), extending the delivered set |
| 3 | Inline-assembly surface | **Rust/LLVM-style `asm` template** (indentation-only, operand/clobber clauses; fixed-register operands first) + intrinsics on top + `asm_bytes` escape |
| 4 | MMIO / port I/O / privileged access | **Named `unsafe` intrinsics** (`volatile_*`, `port_*`, `read_cr`/`read_msr`/`cli`/…) + `asm` for the long tail |
| 5 | Static-buffer arena binding | **`region … in <buffer>`** clause on the delivered region block; overflow → panic handler, never grow |
| 6 | ISR / naked function form | **`interrupt fn` / `naked fn`** prefix keywords (matching `export`/`extern`) |
| 7 | `repr` / packed / alignment spelling | **`repr c packed align N`** header line inside the struct body |
| 8 | User panic handler registration | **`panic fn on_panic info ptr<PanicInfo> -> never`**, exactly one per program; bounds check calls it |
| 9 | Freestanding entry symbol | **`entry fn`** prefix keyword (no exit stub), `--entry` as CLI override |
| — | Section placement (sub-fork of 9) | **`section "<name>"`** prefix clause on declarations |
| — | Output formats (sub-fork of 9) | **`--format elf-exec` / `--format flat`** direct writers (the ELF/flat analogues of the delivered direct-PE writer) |

Each fork is designed so the *surface* stays stable if the owner later reverses the
underlying mechanism — the tier can ship on these recommendations and evolve beneath
them, exactly as the concurrency proposal is structured.
