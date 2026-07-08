# FFI / C-ABI Interop Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).
Builds on [native_backend_contract.md](native_backend_contract.md) (the native
backend contract and the first `extern`/`export` FFI increments), and composes
with [generics_design.md](generics_design.md), [closures_design.md](closures_design.md)
(function values → C callbacks), [struct_design.md](struct_design.md)
(`repr(C)` layout), and [lullaby_type_system.md](lullaby_type_system.md)
(raw pointers, scalar widths).

This document is the implementation-grade contract for Lullaby's Foreign
Function Interface (FFI) against the platform C ABI. It specifies the source
surface (`extern`, `export`), the calling-convention lowering for each supported
target, the exact type marshalling table, linking and symbol resolution,
callbacks, C-header export, the safety model, and the diagnostics. FFI is a
**native-only** feature: the AST/IR/bytecode interpreters cannot execute real C
FFI and reject it with a clear diagnostic (see [[interpreter parity]] below).

The first two increments — calling C (`extern fn`) and exposing Lullaby to C
(`export fn`) for the i64-scalar Win64 case — are **delivered** and recorded in
[native_backend_contract.md](native_backend_contract.md). This document is the
full design those increments are the first slice of; sections tagged
**(delivered)** match shipped behavior, and sections tagged **(planned)**
specify the intended extension.

ClickUp: list **13 Native Codegen and FFI** (ticket family "[Design] FFI /
C-ABI design doc" and its implementation follow-ups).

## 1. Goals and non-goals

Goals:

- Call C functions from Lullaby across the platform C ABI (System V AMD64,
  Windows x64, AArch64 AAPCS64).
- Expose Lullaby functions to C with plain C linkage, and generate a matching
  C header so a C consumer can `#include` and link.
- A precise, checkable type-marshalling table between Lullaby types and C types,
  with explicit memory-ownership rules.
- Reference, resolve, and link both dynamic (`.so`/`.dll`/`.dylib`) and static
  (`.a`/`.lib`) libraries from the CLI/build surface.
- Pass a Lullaby function value to C as a C function pointer (callbacks).
- Stay sound: every FFI operation is `unsafe`, and misuse produces a stable
  `L####` diagnostic rather than undefined behavior at the language layer.
- Stay at parity with the interpreters by making FFI a native-only feature the
  interpreters reject deterministically.

Non-goals (this design, deferred to later tickets):

- C++ name mangling / ABI, exceptions, RTTI, or templates.
- Variadic C calls with Lullaby-side type checking (`printf`-style). A narrow
  fixed-prototype variadic mode is a planned follow-up (§10).
- Bitfields, unions with overlapping active members, `long double`/`__int128`,
  SIMD vector types, and `#pragma pack` beyond `repr(C, packed)`.
- Automatic binding generation from C headers (a `bindgen`-style tool is a
  separate ticket that would emit `extern`/`repr(C)` declarations into `.lby`).

## 2. Source surface

### 2.1 `extern` — declaring an imported C function

An `extern fn` declaration names an external C symbol and its C-ABI signature.
It is indentation-consistent with every other `fn`: it is a single header line
with **no indented body** (the body-less form is what distinguishes it), so the
existing indentation-only rule is unchanged — there is simply no block to open.

```lby
extern fn llabs x i64 -> i64

extern fn strlen s cstr -> i64

extern fn memcpy dst ptr<byte> src ptr<byte> n i64 -> ptr<byte>
```

Grammar (mirrors the delivered form and extends the type surface):

```
extern_decl   = [ "pub" ] "extern" [ abi_spec ] "fn" name params [ "->" type ] NEWLINE
abi_spec      = "abi" STRING            # "c" (default), "stdcall", "sysv", "win64", "aapcs"
params        = { name type }           # source order, whitespace-separated (existing fn param form)
```

Rules:

- `extern` prefixes a `fn`; an optional `pub` may precede it (`pub extern fn`).
  `extern` combines with neither `async` nor `export`.
- The declaration ends after the return type — **no indented body**. A body on
  an `extern fn` is `L0427` (§9).
- The Lullaby function name **is** the C symbol name by default. To bind a C
  symbol whose name is not a legal Lullaby identifier, or to rename, an optional
  `symbol` clause overrides it:

  ```lby
  extern fn c_open path cstr flags i32 -> i32 symbol "open"
  ```

- An optional `from "lib"` clause records the library the symbol resolves from,
  so the build surface can add the right link input without a separate CLI flag
  (§6). It does not change type checking:

  ```lby
  extern fn SDL_Init flags u32 -> i32 from "SDL2"
  ```

- The calling convention defaults to the platform C convention (`abi "c"`). An
  explicit `abi` is only needed for Windows-specific conventions such as
  `stdcall` (§4.4). `abi "c"` is portable and selects System V / Win64 / AAPCS
  by target.
- Calls to an `extern fn` are type-checked exactly like ordinary calls (arity,
  argument types, return type) against the registered C-ABI signature. An
  `extern` name that is not a builtin resolves through the ordinary
  user-function call path, so the builtin catalog is unaffected — choose a name
  that is not a Lullaby builtin (`llabs`, not `abs`).

### 2.2 `export` — exposing a Lullaby function to C

An `export fn` is a normal, bodied Lullaby function additionally published under
its plain, unmangled C name so C can call **into** Lullaby:

```lby
export fn add_seven x i64 -> i64
    x + 7
```

Rules:

- `export` prefixes a `fn` (optional `pub export fn`). Unlike `extern`, an
  export **has a body**. `export` is mutually exclusive with `extern` and
  `async`.
- The Lullaby function name is the exported C symbol name. A `symbol "..."`
  clause may rename it, mirroring `extern`.
- The exported function must have a C-marshallable signature (§5). A signature
  outside the currently supported marshalling set is rejected at check time with
  `L0424` (delivered for the i64-scalar restriction; the code widens as the
  marshalling table grows).
- `export` is meaningful only to native codegen. On the interpreters an
  `export fn` runs exactly like an ordinary `fn`.

### 2.3 `repr(C)` structs

A struct used across the C boundary must opt into the C layout with a `repr`
attribute so its field order, offsets, alignment, and padding match the target
C ABI (Lullaby's default struct layout is an implementation detail and may
reorder fields):

```lby
repr(C)
struct Rect
    x i32
    y i32
    w i32
    h i32

repr(C, packed)
struct Header
    tag u8
    len u32
```

- `repr(C)` guarantees fields are laid out in declaration order with C's natural
  alignment and tail padding (see §5.3 for the exact algorithm).
- `repr(C, packed)` forces alignment 1 (no inter-field padding); it is `unsafe`
  to take a `ptr<T>` to a packed field (misaligned access) and the compiler
  notes this.
- Only a `repr(C)` struct may appear in an `extern`/`export`/callback signature
  or be pointed at by a `ptr<T>` that crosses the boundary. Passing a
  non-`repr(C)` struct across FFI is `L0428` (§9).

## 3. Interpreter parity ([[interpreter parity]])

FFI is defined as **native-only**. The AST, IR, and bytecode interpreters have
no C runtime, no linker, and no machine-code execution, so they cannot perform a
real foreign call. The intended behavior is precise and uniform:

- **`extern fn` call on an interpreter** → runtime diagnostic `L0423`:
  *"cannot call extern (C-ABI) function `NAME` on an interpreter; compile with
  `lullaby native` to link and run it"*. (Delivered.)
- **`export fn` on an interpreter** → runs exactly as an ordinary `fn` (there is
  nothing foreign about the body); only its C symbol visibility is native-only.
  (Delivered.)
- **A C callback trampoline requested on an interpreter** → the same `L0423`
  family, because there is no C to call back from.
- `lullaby check` fully validates `extern`/`export` declarations and their call
  sites (arity, types, `repr(C)`, `symbol`/`from` clauses), so type errors are
  caught statically on every backend — only *execution* is native-only.

This keeps all interpreter backends at behavioral parity: they agree that FFI is
not executable and emit the identical diagnostic, rather than one backend
panicking or silently no-op-ing. It mirrors the delivered `asm`/`L0425` model.

## 4. Calling convention and lowering

FFI lowering sits on top of the native backend contract
([native_backend_contract.md](native_backend_contract.md)): source is validated,
lowered to typed IR, and to bytecode, and only the native emitter turns an FFI
call into a real ABI-conformant `call`. The internal Lullaby ABI is adapted to
the platform C ABI at the call boundary.

### 4.1 Windows x64 (`x86_64-pc-windows-msvc`) — delivered baseline

- **Integer/pointer arguments**: first four in `RCX`, `RDX`, `R8`, `R9`; the
  rest on the stack, right-to-left, above the 32-byte shadow space.
- **Floating arguments** (`f32`/`f64`): first four in `XMM0`–`XMM3`, positionally
  aligned with the integer registers (argument *n* uses integer register *n* or
  XMM *n*, never both).
- **Shadow space**: the caller always reserves 32 bytes below the return
  address; the emitter already does this for every call site.
- **Return**: integers/pointers in `RAX`; `f32`/`f64` in `XMM0`. A struct larger
  than 8 bytes, or not 1/2/4/8 bytes, is returned via a hidden pointer in `RCX`
  (sret), shifting the other arguments right by one register (§5.3).
- **Alignment**: `RSP` is 16-byte aligned at each `call` (already guaranteed by
  the frame model).
- **Variadics**: floating arguments passed to a variadic C function are also
  copied into the matching integer register (shadow rule) — relevant only to the
  planned variadic mode (§10).

### 4.2 System V AMD64 (`x86_64-unknown-linux-gnu`, `x86_64-apple-darwin`) — planned

- **Integer/pointer arguments**: `RDI`, `RSI`, `RDX`, `RCX`, `R8`, `R9`, then
  stack.
- **Floating arguments**: `XMM0`–`XMM7`, then stack. Integer and SSE classes are
  counted independently (no shadow coupling), so argument *n* consumes the next
  free register **of its class**.
- **Aggregate classification**: a struct ≤ 16 bytes is classified field-by-field
  into eightbytes (INTEGER vs SSE); each eightbyte goes in the next register of
  its class if enough registers remain, otherwise the *whole* aggregate goes on
  the stack (the SysV MEMORY rule). A struct > 16 bytes, or one containing an
  unaligned field, is MEMORY (passed on the stack; returned via hidden sret
  pointer in `RDI`).
- **Return**: `RAX`(/`RDX` for the second integer eightbyte); `XMM0`(/`XMM1`)
  for SSE; MEMORY via hidden sret pointer, returned again in `RAX`.
- **No shadow space**; `%al` must hold the count of vector registers used for a
  variadic call (planned variadic mode only).
- **Stack**: 16-byte aligned at `call`.

### 4.3 AArch64 AAPCS64 (`aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`) — planned

- **Integer/pointer arguments**: `X0`–`X7`, then stack.
- **Floating/SIMD arguments**: `V0`–`V7`, then stack. Homogeneous
  floating-point aggregates (HFAs, up to 4 members) are passed in consecutive
  `V` registers.
- **Aggregates**: ≤ 16 bytes are passed in `X` registers; > 16 bytes are passed
  by reference to a caller-allocated copy (the indirect rule), with the pointer
  in the next `X` register.
- **Return**: `X0`(/`X1`); `V0` for floating; large aggregate via hidden pointer
  in `X8` (the indirect result register).
- **Apple variant note**: on `aarch64-apple-darwin`, stack-passed arguments are
  packed at their natural size (not slot-rounded to 8 bytes) and variadic
  arguments always go on the stack — the emitter must special-case the Apple
  AArch64 rules.

### 4.4 Windows `stdcall` (32-bit / legacy) — planned, opt-in

Selected by `abi "stdcall"`. Arguments pushed right-to-left on the stack; callee
cleans the stack; the C symbol is decorated `_NAME@N` where `N` is the byte size
of all arguments. Only meaningful on 32-bit x86 targets, which are not a current
native target; the `abi` field is defined now so the surface is stable.

### 4.5 Composition with the native backend contract

- FFI call sites reuse the existing `call rel32` + relocation machinery. A call
  to an `extern` symbol emits an `IMAGE_REL_AMD64_REL32` (COFF) /
  `R_X86_64_PLT32` (ELF) / `ARM64_RELOC_BRANCH26` (Mach-O) relocation against an
  **undefined external symbol**, reusing the exact mechanism that already imports
  `ExitProcess`/`ucrt` symbols (delivered for COFF).
- `extern_functions` and `export_functions` on `IrModule`/`BytecodeModule` carry
  the FFI symbol sets (serde-defaulted for artifact compatibility, delivered).
  The native emitter adds `extern` names to the callable set and materializes
  unresolved relocation targets as undefined externals; it publishes `export`
  names as externally visible defined `.text` symbols.
- The internal Lullaby ABI still applies to Lullaby↔Lullaby calls; only the FFI
  boundary is adapted to the platform C ABI. This keeps ordinary codegen
  unchanged and localizes the ABI logic to the boundary shim.

## 5. Type marshalling

A Lullaby type crossing the FFI boundary is lowered to a C type with a fixed,
checked mapping. A signature using any type **not** in this table is rejected at
check time (`L0424` for exports, `L0429` for extern params/returns; §9).

### 5.1 Scalars

| Lullaby type | C type | Size | Align | Class | Notes |
| :--- | :--- | :--- | :--- | :--- | :--- |
| `i8`  | `int8_t`  | 1 | 1 | INTEGER | delivered (extern call) |
| `i16` | `int16_t` | 2 | 2 | INTEGER | delivered (extern call) |
| `i32` | `int32_t` | 4 | 4 | INTEGER | delivered (extern call) |
| `i64` | `int64_t` | 8 | 8 | INTEGER | delivered (extern call + export) |
| `u8`  | `uint8_t` | 1 | 1 | INTEGER | delivered (extern call) |
| `u16` | `uint16_t`| 2 | 2 | INTEGER | delivered (extern call) |
| `u32` | `uint32_t`| 4 | 4 | INTEGER | delivered (extern call) |
| `u64` | `uint64_t`| 8 | 8 | INTEGER | delivered (extern call) |
| `isize`| `intptr_t`| 8 | 8 | INTEGER | delivered (extern call); 64-bit on current targets |
| `usize`| `size_t`  | 8 | 8 | INTEGER | delivered (extern call); 64-bit on current targets |
| `f32` | `float`   | 4 | 4 | SSE | delivered (extern call + export); XMM arg/return routing |
| `f64` | `double`  | 8 | 8 | SSE | delivered (extern call + export); XMM arg/return routing |
| `bool`| `_Bool`   | 1 | 1 | INTEGER | delivered (extern call); 0/1, any nonzero from C reads as `true` |
| `char`| `uint32_t`| 4 | 4 | INTEGER | delivered (extern call); Lullaby `char` is a Unicode scalar, not C `char` |
| `byte`| `uint8_t` | 1 | 1 | INTEGER | delivered (extern call); the C `unsigned char`/octet type |

For an **extern call** every scalar above is marshalled today. An integer-class
argument is passed in the low bits of its Win64 register (`rcx`/`rdx`/`r8`/`r9`),
already sign/zero-normalized to its width in the interpreter's cell model (which
is exactly what the ABI requires), and a narrow C **return** is re-normalized in
`rax` (sign-extend for signed kinds, zero-extend for unsigned; `i64`/64-bit kinds
are a no-op). A **float** (`f32`/`f64`) argument is passed in the SSE register at
its **position** (`xmm0..3`, positionally aligned with the integer registers — a
float at position N uses `xmm N` while an integer at position N uses integer
register N, and each position consumes its slot in exactly one sequence, so
`ldexp(double, int)` sends `double`→`xmm0` and `int`→`rdx`). A `f32`/`f64`
**return** is read from `xmm0` (`movsd`/`movss`; f32 keeps only its low four
bytes). Both directions are **delivered** and native-only: a caller of a float or
mixed float/int extern compiles and links, while the interpreters still reject the
extern call with `L0423`.

The `export` direction accepts the delivered scalar set `i64`/`f64`/`f32` (a
non-scalar export signature is `L0424`); the float export receives each float
parameter in its positional SSE register and returns its float in `xmm0`. Widening
the export marshalling to the full integer-width set and to pointers/structs is a
follow-up.

Rules:

- Widths are exact and target-independent (no `int`/`long` ambiguity is exposed
  at the Lullaby level; a C `int` maps to `i32`, `long` maps to `i32` on Win64
  and `i64` on LP64, `size_t` maps to `u64` on all 64-bit targets — the binding
  author picks the concrete Lullaby width to match the platform).
- Sub-64-bit integers are **not** silently widened in the value model: they are
  passed in the low bits of the argument register with the ABI's required
  extension (zero-extend for unsigned, sign-extend for signed) and truncated on
  return per the C ABI.
- Lullaby `char` is deliberately a 32-bit Unicode scalar, distinct from C
  `char` (a byte). Use `byte`/`u8` for a C `char*` byte, not `char`.

### 5.2 Raw pointers

- `ptr<T>` maps to `T*` (C pointer to the marshalled `T`). `ptr<void>` maps to
  `void*`. Pointer size/alignment is 8 on all current targets.
- A null pointer is representable only through `unsafe` construction; safe source
  cannot produce a null `ptr<T>` (the native contract already forbids lowering
  null from safe operations).
- Dereferencing (`ptr_read`/`ptr_write`) requires `unsafe` (delivered `L0330`)
  and is the caller's responsibility to keep in bounds and correctly typed.
- `ptr<T>` is *not* automatically freed. Memory obtained from C (`malloc`,
  library allocators) is owned by C; the Lullaby side must call the matching C
  free routine via another `extern fn`. Ownership is documented per-binding, not
  inferred.

### 5.3 `repr(C)` structs

Layout algorithm (must match the target C compiler):

1. Start `offset = 0`, `struct_align = 1`.
2. For each field in declaration order: `field_align = align_of(field_type)`
   (1 for `packed`); round `offset` up to a multiple of `field_align`; place the
   field at `offset`; advance `offset += size_of(field_type)`; set
   `struct_align = max(struct_align, field_align)`.
3. Round the final `offset` up to a multiple of `struct_align`; that is
   `size_of(struct)`. A zero-field `repr(C)` struct has no ABI meaning; passing
   one is `L0428`.

Passing rules follow §4 per target: small structs go in registers
(field-classified on SysV/AArch64; whole-struct in a register when ≤ 8 bytes on
Win64), large structs go on the stack or by hidden pointer (sret). Nested
`repr(C)` structs and fixed arrays inside a `repr(C)` struct are laid out
recursively by the same algorithm.

Struct *values* as parameters/returns/arguments are **planned** (the delivered
native increment supports scalar-flattened structs as *locals* only); the
first FFI struct increment passes them **by pointer** (`ptr<Rect>`), which needs
no register classification and lands before by-value struct passing.

### 5.4 Arrays

- A Lullaby fixed array `array<T>` crossing FFI decays to `ptr<T>` (a pointer to
  the first element) plus a separate length argument the C API expects — Lullaby
  arrays are not self-describing to C. There is no implicit length passing; the
  binding names the length parameter explicitly:

  ```lby
  extern fn sum_i32 data ptr<i32> n i64 -> i64
  ```

- A `repr(C)` struct field that is a fixed C array (`T[N]`) is laid out inline by
  §5.3. Growable `list<T>`/`map<K,V>` do **not** cross the boundary (their
  runtime layout is not C-ABI-stable); passing one is `L0429`.

### 5.5 Strings: `string` ↔ `char*` / `cstr`

Lullaby `string` is a length-prefixed, UTF-8, runtime-owned handle; C strings
are NUL-terminated `char*`. The mapping is explicit and ownership is spelled out.

- `cstr` is an FFI-only marshalling type meaning "a borrowed, NUL-terminated,
  UTF-8 `const char*`". It is the type you write in an `extern`/`export`
  signature; it is not a general Lullaby value type.
- **Lullaby `string` → C (`cstr`, outbound argument)**: the compiler materializes
  a NUL-terminated UTF-8 copy of the string bytes into a temporary buffer whose
  lifetime is the duration of the foreign call, and passes a `const char*` to it.
  The buffer is freed when the call returns. The callee must treat it as
  borrowed and must not retain the pointer past the call. A Lullaby string
  marshalled to `cstr` must not contain an interior NUL; an interior NUL is a
  runtime error `L0430` (a `cstr` cannot represent it).
- **C `char*` → Lullaby `string` (inbound return / out-parameter)**: a returned
  `cstr`/`char*` is **borrowed by default** — the Lullaby side must copy it into
  an owned `string` (`string_from_cstr(p)`) if it needs to outlive C's ownership,
  and must not free it. If the C API documents that the caller owns the returned
  buffer, the binding uses `owned_cstr` and the Lullaby runtime takes ownership,
  freeing it with the C allocator's `free` when the derived `string` is dropped.
  The default is *borrow* precisely because "who frees this?" is a per-API fact
  the binding author must state.
- **UTF-8 and validation**: Lullaby strings are UTF-8, matching modern C string
  conventions. Converting an inbound `cstr` to `string` validates UTF-8;
  invalid UTF-8 is `L0431` at runtime (the binding can instead take `ptr<byte>`
  to receive raw bytes without validation).
- Ownership summary:
  - Outbound `string`→`cstr`: **Lullaby owns** the temporary; freed after the
    call; C must not retain.
  - Inbound `cstr` (borrowed): **C owns**; Lullaby copies if it needs to keep it;
    Lullaby must not free.
  - Inbound `owned_cstr`: **ownership transfers to Lullaby**; freed via the C
    allocator when dropped.

### 5.6 Function pointers

A Lullaby function type `fn(A...) -> R` marshals to a C function pointer
`R (*)(A...)` when all `A...`/`R` are themselves marshallable. This is the type
used for callbacks (§7).

## 6. Linking

FFI adds link inputs; the object-emission path is unchanged from
[native_backend_contract.md](native_backend_contract.md).

### 6.1 Symbol resolution model

- Every `extern fn` becomes an **undefined external symbol** in the emitted
  object; the linker resolves it against the provided libraries.
- Every `export fn` becomes an **externally visible defined** `.text` symbol.
- A `from "lib"` clause records a required library per symbol; the build surface
  aggregates these into the link line. An `extern` with no `from` relies on a
  CLI-provided library (§6.3) or the default C runtime.

### 6.2 Dynamic vs static libraries

| Kind | Windows | Linux | macOS |
| :--- | :--- | :--- | :--- |
| Dynamic | link import lib `NAME.lib` → `NAME.dll` at runtime | `-lNAME` → `libNAME.so` | `-lNAME` → `libNAME.dylib` |
| Static  | `NAME.lib` (archive) | `libNAME.a` | `libNAME.a` |

- **Windows**: dynamic linking goes through an *import library* (`NAME.lib`)
  that resolves to the runtime `NAME.dll`; the delivered path already discovers
  `kernel32.lib`/`ucrt.lib` via the MSVC `LIB` environment variable and passes
  them to `rust-lld`. Static libraries are archives (`.lib`) added directly.
- **Linux** (planned): dynamic and static both use `-lNAME` with `-L<dir>` search
  paths, resolved by the system linker (or `rust-lld`'s ELF flavor); a
  `.so`/`.a` is chosen by search order (`-Bstatic`/`-Bdynamic` to force one).
- **macOS** (planned): `-lNAME`/`-L<dir>` resolving `.dylib`/`.a`; frameworks via
  `-framework NAME`. `install_name`/`@rpath` handling for produced dynamic
  outputs is a follow-up.

### 6.3 CLI / build surface

- `lullaby native FILE.lby -l NAME` adds a library to link (repeatable),
  `-L DIR` adds a search path (repeatable), and `--link-static NAME` /
  `--link-dynamic NAME` force the link kind for `NAME`. These aggregate with each
  `from "..."` clause; a `from` name and an explicit `-l` for the same library
  dedupe.
- The delivered link step is **best-effort**: the object is written
  unconditionally (the reliable floor), then linking is attempted with `rust-lld`
  discovered under the rustc sysroot; if `rust-lld` or a required import library
  is missing, the command reports the object and explains linking was
  unavailable rather than failing. FFI keeps this contract: a missing FFI library
  degrades to "object written, link skipped, here is why".
- Per-target differences are surfaced in diagnostics: a link failure names the
  target triple and the unresolved symbol/library (`L0432`, §9).
- The project manifest (`lullaby.json`) gains an optional `native.link` array
  (library names, search paths, static/dynamic preference, per-target overrides)
  so a project can declare its native link inputs once instead of on every CLI
  invocation.

## 7. Callbacks — passing a Lullaby function to C

A C API that takes a function pointer (e.g. `qsort`'s comparator) receives a
Lullaby function value marshalled to a C function pointer.

```lby
extern fn qsort base ptr<void> n u64 size u64 cmp fn(ptr<void>, ptr<void>) -> i32 -> void

fn cmp_i32 a ptr<void> b ptr<void> -> i32
    unsafe
        let x i32 = ptr_read(a)
        let y i32 = ptr_read(b)
        x - y

fn main -> i64
    # ... fill an array, then:
    qsort(base, n, 4, cmp_i32)
    0
```

Design:

- **Only a top-level (non-capturing) function value** may be marshalled to a C
  function pointer in the first callback increment. Its address is taken
  directly — no trampoline, no runtime allocation — because it already has the
  C-ABI calling convention when its signature is fully marshallable. This is the
  simplest correct case and needs no lifetime management: the code lives for the
  whole program.
- **Capturing closures** ([closures_design.md](closures_design.md), a follow-up)
  require a **trampoline**: the compiler emits a small stub whose signature is
  the C function pointer type and which loads the captured environment from a
  fixed slot before dispatching to the closure body. Because C function pointers
  carry no environment word, the environment must be reachable another way. Two
  supported patterns:
  1. **Context-argument APIs** (the common, safe case): the C API passes a
     `void* user_data` alongside the callback (`qsort_r`, most GUI/event APIs).
     The trampoline recovers the closure environment from that `user_data`, so no
     global state is needed. This is the recommended and lifetime-clean form.
  2. **Context-free APIs**: a per-callback-site thunk with a statically allocated
     environment slot. This is `unsafe`, not re-entrant, and the environment
     lives for the program; it is gated behind an explicit
     `as_c_callback_static(f)` builtin so the author opts into the limitation.
- **Lifetime**: the marshalled pointer is valid only while the underlying
  function/closure environment is alive. For a top-level function that is the
  whole program; for a `user_data` closure it is the closure's lifetime; misuse
  (retaining the pointer past the environment) is `unsafe` and undiagnosable at
  the language layer — the safety model (§9) documents the invariant.
- The callback signature is type-checked against the `extern` parameter's
  `fn(...)` type exactly like a Lullaby function value (delivered `L0390`
  covers the signature-mismatch case).

## 8. Exports and C-header generation

`export fn` publishes a C-callable symbol (§2.2, delivered for i64-scalar). To
let a C consumer actually call in, the compiler generates a matching header.

```lby
export fn add_seven x i64 -> i64
    x + 7

repr(C)
struct Rect
    x i32
    y i32
```

`lullaby native FILE.lby --emit-header out.h` writes:

```c
/* Generated by lullaby; do not edit. */
#ifndef LULLABY_FILE_H
#define LULLABY_FILE_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif

typedef struct Rect { int32_t x; int32_t y; } Rect;

int64_t add_seven(int64_t x);

#ifdef __cplusplus
}
#endif
#endif /* LULLABY_FILE_H */
```

Rules:

- One prototype per `export fn`, using the §5 marshalling for each parameter and
  the return type; the `symbol "..."` name (if any) is the emitted C name.
- Each `repr(C)` struct reachable from an exported signature emits a matching C
  `typedef struct`, laid out per §5.3 so the C compiler computes identical
  offsets.
- The header is guarded, `extern "C"`-wrapped for C++ consumers, and includes
  `<stdint.h>` for the fixed-width types. It is deterministic (stable ordering)
  so it can be checked in.
- **Library objects**: an export-only program (no `main`) emits a library object
  with no entry stub (delivered), so a C `main` links against it without an
  entry-point collision. A program with both `main` and exports keeps the entry
  stub and additionally exposes the exports.

## 9. Safety model and diagnostics

### 9.1 `unsafe` integration

FFI is inherently unsafe: the compiler cannot verify a foreign function honors
its declared signature, keeps pointers in bounds, or respects ownership.

- **Calling an `extern fn` is `unsafe`** and must appear inside an `unsafe`
  block (consistent with `ptr_read`/`ptr_write`/`asm`). Calling one outside
  `unsafe` is `L0433` — or, if a distinct message is not wanted, this family may
  be folded into the existing `L0330` (raw-operation-outside-`unsafe`).
- **Defining an `export fn` is safe** (its body is ordinary Lullaby); the
  unsafety is on the C caller's side, outside the language.
- **Marshalling a callback** is safe for a top-level function; the static-thunk
  closure form (`as_c_callback_static`) is `unsafe`.

Invariants the FFI caller must uphold (documented, not machine-checked):

1. The `extern` signature matches the real C symbol's ABI signature exactly.
2. Pointers passed to C point to live, correctly typed, in-bounds memory for the
   duration C uses them.
3. Ownership follows §5.5 / §5.2: the side that allocates frees, unless an
   `owned_*` marshalling type transfers ownership.
4. A marshalled callback pointer is not used after its environment dies.
5. `cstr` arguments contain no interior NUL and (for validated conversions) valid
   UTF-8.

### 9.2 Proposed diagnostics

These extend the existing registry ranges (semantic `L03xx`, runtime `L04xx`)
**consistently**; they are proposed here and must be added to
[diagnostic_registry.md](diagnostic_registry.md) when implemented (this design
does not edit the registry). Delivered codes (`L0423`, `L0424`, `L0426`) are
listed for completeness.

| Code | Phase | Meaning | Status |
| :--- | :--- | :--- | :--- |
| `L0423` | runtime | Cannot call an `extern fn` on an interpreter. | delivered |
| `L0424` | semantic | Unsupported `export fn` signature (outside the marshalling set). | delivered |
| `L0426` | ir | `--freestanding` conflicts with an `extern fn`'s C-runtime need. | delivered |
| `L0427` | parser | An `extern fn` has an indented body (it must be body-less). | proposed |
| `L0428` | semantic | A non-`repr(C)` (or zero-field/packed-unsafe) struct crosses the FFI boundary. | proposed |
| `L0429` | semantic | An `extern fn` parameter/return uses a non-marshallable type (`list`/`map`/non-`repr(C)` struct/etc.). | proposed |
| `L0430` | runtime | A `string` marshalled to `cstr` contains an interior NUL. | proposed |
| `L0431` | runtime | An inbound `cstr` is not valid UTF-8 when converted to `string`. | proposed |
| `L0432` | resource | Native link failed to resolve an FFI symbol or library (names the target triple). | proposed |
| `L0433` | semantic | An `extern fn` call (or static callback thunk) used outside `unsafe`. | proposed (or fold into `L0330`) |
| `L0434` | semantic | Invalid FFI attribute/clause (`abi`, `symbol`, `from`, `repr`) shape or unknown value. | proposed |

Each diagnostic carries the standard JSON fields (see
[diagnostic_registry.md](diagnostic_registry.md)) and, for target-specific link
failures, the target triple in `notes`.

## 10. Scope and sequencing

FFI depends on native codegen: every FFI capability is meaningful only after
object emission + linking. Increments are ordered so each is production-complete
and independently testable, matching the delivered native slices.

**Delivered** (see [native_backend_contract.md](native_backend_contract.md)):

1. Call C — `extern fn`, i64-scalar Win64, `ucrt.lib` link, `L0423` on
   interpreters.
2. Expose to C — `export fn`, i64-scalar Win64, library objects, C caller test.
3. Call C via `extern fn` for the **full integer scalar subset** — all
   fixed-width integers (`i8`…`u64`, `isize`/`usize`) plus `bool`/`char`/`byte`,
   Win64 integer registers with narrow-return normalization in `rax`. The
   `extern fn` C-ABI signatures are threaded through the IR/bytecode
   (`extern_signatures`) so the native emitter marshals each width.
4. **Float scalar marshalling** (`f32`/`f64`) for both `extern` calls and
   `export fn` on Win64. A float argument is routed to the SSE register at its
   position (`xmm0..3`, positionally aligned with the integer registers, so a
   mixed `f(double, int)` sends `double`→`xmm0` and `int`→`rdx`); a float return
   is read from `xmm0` (`movsd`/`movss`). An `export fn` receives each float
   parameter in its positional SSE register and returns its float in `xmm0`. This
   completes the scalar C-ABI story (all integer widths + `bool`/`char`/`byte` +
   `f32`/`f64`). Verified end to end against the C runtime's `sqrt`
   (`double sqrt(double)`) and `ldexp` (`double ldexp(double, int)`, mixed
   float/int).

**First production-complete FFI increment (this design's near-term target):**

- Full scalar marshalling (§5.1: all int widths, `f32`/`f64`, `bool`,
  `char`/`byte`) for `extern` and `export` on Win64. **Extern-call integer
  widths + `bool`/`char`/`byte` are delivered (item 3 above); `f32`/`f64` scalar
  marshalling — positional `xmm0..3` arguments and the `xmm0` return, for both
  extern calls and `export fn` — is now delivered (item 4 below). Widening the
  `export` direction to the full integer-width set remains.**
- `ptr<T>` parameters/returns (§5.2) and `repr(C)` structs passed **by pointer**
  (§5.3), including nested/array fields.
- `string`↔`cstr` marshalling with the ownership rules and `L0430`/`L0431`
  (§5.5).
- Top-level-function callbacks (§7 first bullet).
- C-header generation (`--emit-header`, §8).
- CLI link surface (`-l`/`-L`/static-dynamic, `from`/`symbol`/`abi` clauses,
  manifest `native.link`) (§6.3), with graceful degradation.
- Diagnostics `L0427`–`L0434` (§9.2).

**Deferred (later tickets, dependency-ordered):**

- By-value `repr(C)` struct passing/returning with register classification per
  target (§4.2/§4.3/§5.3) — depends on aggregate-argument support in the native
  emitter.
- System V AMD64 (Linux/macOS) and AArch64 AAPCS64 ABIs and ELF/Mach-O object
  emission — depends on the cross-platform object-writer work already noted as
  deferred in the native contract.
- Capturing-closure callbacks via `user_data` trampolines (§7) — depends on the
  closure increment in [closures_design.md](closures_design.md).
- A narrow fixed-prototype variadic mode; automatic `bindgen`-style binding
  generation from C headers; `stdcall`/32-bit targets; macOS frameworks and
  `@rpath`/`install_name`.

## 11. Why these choices

- **Native-only, interpreter-rejecting FFI** keeps all interpreter backends at
  behavioral parity (they agree FFI is not executable and emit one deterministic
  diagnostic) instead of one backend faking a foreign call. This mirrors the
  delivered `extern`/`asm` model and preserves the parity harness.
- **The function name is the C symbol** (with an optional `symbol` override)
  keeps the common case zero-ceremony and matches how the delivered increments
  already surface COFF symbols, while `symbol`/`from`/`abi` clauses handle the
  irregular cases without new statement forms.
- **Explicit, exact-width scalar mapping** (no exposed `int`/`long`) removes the
  single biggest C-ABI footgun (platform-dependent integer widths) from the
  language surface and makes marshalling checkable.
- **`repr(C)` is opt-in** because Lullaby's default struct layout is an
  implementation detail; requiring the attribute makes every ABI-relevant layout
  intentional and lets the default layout stay free to optimize.
- **Ownership is stated per-binding, never inferred** (`cstr` borrow vs
  `owned_cstr` transfer) because "who frees this?" is a fact about the C API that
  the compiler cannot know; encoding it in the marshalling type makes it explicit
  and checkable rather than a comment.
- **Callbacks start with top-level functions** (address-taken, no trampoline, no
  lifetime management) because that is the correct, allocation-free base case;
  closures layer on via `user_data` trampolines only once closures exist, keeping
  the first increment small and sound.
- **Best-effort linking with an always-written object** matches the delivered
  native contract: the object is the reliable floor, and a missing FFI library
  degrades to a clear explanation with the target triple rather than a hard
  failure, which keeps native builds usable on machines without a full C
  toolchain.
