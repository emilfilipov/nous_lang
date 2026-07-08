# WebAssembly Heap (Linear-Memory) Phase Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

Builds directly on the delivered scalar/heap-lite backend in
[[wasm_backend_design.md]] and reuses the runtime representations documented in
[[enum_and_match_design.md]], [[option_result_design.md]], [[struct_design.md]],
and [[standard_library.md]]. ClickUp list: **"16 Web and WASM"**.

This note designs the **full heap phase** of the WebAssembly backend: the phase
that lets *non-scalar* Lullaby programs — programs that build enums, `option`/
`result`, growable `list`/`map`, and runtime strings — compile to a real `.wasm`
module and run in a browser or a server-side WASM runtime. The scalar subset,
the `[len i32][utf8]` string literal pool, fixed struct/array layout, the bump
`__alloc` helper, and the `env.log_i64`/`env.console_log`/`env.dom_set_text`
host imports are already **DELIVERED** (see [[wasm_backend_design.md]] "Heap
types (landed)"). This document specifies what remains: a production allocator
with real reclamation, the enum/`option`/`result`/`list`/`map` memory layouts,
runtime string construction, the widened lowering, the JS loader/glue that makes
web frontends technically possible, the CLI surface, and the parity testing
story.

The interpreters (AST/IR/bytecode) remain the correctness ground truth. Every
heap program compiled to WASM must produce results identical to
`lullaby run`; the parity harness enforces this wherever a WASM runtime is
available.

## Status

**PLANNED / design only.** No code in this increment. The existing emitter lives
in `crates/lullaby_ir/src/wasm.rs` (`emit_wasm_module`); this phase extends that
same module (no new crate) and the `lullaby wasm` CLI command. The delivered
constants this phase builds on are `RESERVED_BASE` (16), `SLOT_SIZE` (8),
`LEN_HEADER` (4), `IMPORT_FUNC_COUNT` (3), `BUMP_GLOBAL_INDEX` (0), and
`ALLOC_HELPER_NAME` (`__alloc`).

## Design constraints

- **No external crate.** The encoder stays std-only, exactly as delivered. All
  runtime support (allocator, string builder, `list`/`map` growth, `match`
  dispatch) is emitted as Lullaby-invisible internal WASM functions, not linked
  from a runtime library.
- **Interpreter parity is the contract.** A heap type's WASM layout is an
  implementation detail; the only externally observable behavior is the value a
  function returns and the ordered host-import side effects. The layout must
  reproduce interpreter semantics exactly (e.g. `map` preserves insertion order,
  matching interpreter `Value::Map(Vec<(K,V)>)`; `list` is an ordered sequence,
  matching `Value::Array`).
- **`i32` addresses, single linear memory.** Every heap value is an `i32`
  pointer into the one exported `"memory"`. WASM32 is the target; 64-bit values
  (`i64`, `f64`) are stored in 8-byte slots and remain naturally aligned because
  every slot is 8 bytes (`SLOT_SIZE`) and every allocation is 8-byte aligned.
- **Trap on unrecoverable failure, diagnostic on un-lowerable source.** A source
  construct the backend cannot lower is *skipped* (the function still runs on the
  interpreters); a runtime condition that cannot be represented safely (OOM,
  out-of-bounds) *traps* the WASM instance, which the JS glue surfaces as an
  error. WASM already traps on OOB `memory` access, so bounds checks are only
  emitted where a trap would otherwise corrupt memory.

## 1. Linear memory layout

### 1.1 The Memory section, pages, and growth

The module keeps the delivered **Memory section (id 5)** — one linear memory,
min 1 page (64 KiB). This phase raises the declared **maximum to 32768 pages**
(2 GiB, the WASM32 ceiling) so the allocator can `memory.grow` under load:

```
limits: flag 0x01 (min+max), min = 1, max = 32768
```

Growth is driven by the allocator (§2), which calls the `memory.grow` opcode
(`0x40 0x00`) when the bump/free-list cannot satisfy a request. `memory.grow`
returns the previous size in pages, or `-1` on failure; the allocator branches
on `-1` to the OOM path (§2.4). The exported `"memory"` lets the JS host both
read strings out and (rarely) grow memory itself.

### 1.2 Address space map

Linear memory is partitioned (offsets fixed at emit time except the heap top,
which is the mutable global):

```
[0, RESERVED_BASE=16)      reserved / null guard (zeroed by the Data segment)
[RESERVED_BASE, POOL_END)  interned string-literal pool  [len i32][utf8]...
[POOL_END, HEAP_BASE)      free-list root + allocator bookkeeping (§2.1)
[HEAP_BASE, brk)           the managed heap (bump frontier is the global)
[brk, size*64KiB)          committed-but-unused; grown pages append here
```

`POOL_END = RESERVED_BASE + pool.bytes.len()` (already computed by
`StringPool::heap_base`). This phase inserts a small **allocator control block**
between the pool and the first allocation, so the delivered
`pool.heap_base()` becomes the base of the control block; `HEAP_BASE` is the
control block's end. The bump global (`BUMP_GLOBAL_INDEX`) is re-initialized to
`HEAP_BASE`.

### 1.3 In-memory representation of each heap type

All multi-byte scalars are little-endian (WASM's native order). Every allocation
begins with an **8-byte object header** (§2.1); the *payload* layouts below are
what the pointer handed to Lullaby code points at (i.e. header is at
`ptr - 8`).

**`string`** — an immutable UTF-8 byte buffer with a cached character count.
Identical to the delivered literal layout so literals and runtime-built strings
are indistinguishable to consumers:

```
offset  size  field
0       4     char_count : i32   (Unicode scalar count; == len(s))
4       4     byte_len   : i32   (UTF-8 byte length)
8       N     utf8 bytes         (N == byte_len)
```

The delivered literal pool stores only `[char_count i32][utf8]`; this phase
extends the literal layout to include `byte_len` too (a one-field widening of
`StringPool::intern`) so `len`, indexing, and concatenation share one code path
for literals and runtime strings. `len(s)` still loads `char_count` and extends
to `i64`.

**`array<T>` (fixed) and `list<T>` (growable)** — both are ordered sequences of
8-byte element slots. A `list<T>` adds a capacity word so it can grow in place
without reallocating on every `push`:

```
array<T>:                       list<T>:
0   4   len : i32               0   4   len  : i32   (element count)
4   .   elem slots (len*8)      4   4   cap  : i32   (slot capacity)
                                8   4   data : i32   (ptr to the slot buffer)
                                12  4   (pad to 8-align)
```

The `list` header is a small fixed handle; the growable slot buffer is a
separately allocated `[cap*8]` block referenced by `data`. `push` writes at
`data + len*8` and increments `len`; when `len == cap` it allocates a
`max(cap*2, 4)`-slot buffer, copies, and updates `data`/`cap` (§3.5). A fixed
`array` keeps the delivered inline `[len][slots]` form (no capacity, no
indirection) because its length is compile-time fixed. Element slots hold a
scalar by its WASM type or an `i32` pointer for a nested heap value.

**`map<K, V>`** — an insertion-ordered association list, matching the
interpreter's `Value::Map(Vec<(K, V)>)`. A hash table is deferred (§8); the
first increment mirrors the interpreter's linear structure exactly so ordering
and semantics are guaranteed identical:

```
0   4   len  : i32     (entry count)
4   4   cap  : i32     (entry capacity)
8   4   data : i32     (ptr to entry buffer: cap * 16 bytes)
12  4   (pad)

entry (16 bytes):
0   8   key   slot     (scalar or i32 ptr)
8   8   value slot     (scalar or i32 ptr)
```

`map_set` scans `[0, len)` comparing keys (scalar `eq`, or string `eq` via the
§3.6 string-compare helper); on hit it overwrites the value slot, else it
appends (growing like `list`). `map_get` returns `option<V>` (§1.3 enum),
`map_has` returns `bool`, `map_del` compacts the entry buffer left by one.

**`struct`** — unchanged from the delivered layout: a pointer to one 8-byte slot
per field in declared order, offset `field_index * 8`. Alignment is automatic
(uniform 8-byte slots). Positional construction `__alloc`s `field_count * 8`
bytes and stores each field; `.field` loads a slot; `p.field = v` stores one.
Nested struct/array/string/enum fields are `i32` pointers.

**`enum` / `option` / `result`** — a **tag + inline payload** record. The tag is
a `u32` variant index (declaration order for user enums; `some=0/none=1` for
`option`, `ok=0/err=1` for `result`). The payload is a fixed run of slots sized
to the enum's **widest variant** so every value of one enum type has the same
size (uniform size simplifies `match`-arm layout and lets an enum sit in a
single 8-byte parent slot as a pointer):

```
0   4   tag : i32                 (variant index)
4   4   (pad to 8-align)
8   .   payload slots             (max over variants of payload_count) * 8
```

A unit variant (`none`, `Empty`, `Red`) stores only the tag; its payload slots
are present but unused. `option<T>` is `[tag][one T-or-ptr slot]`;
`result<T, E>` is `[tag][one slot]` (the single payload slot holds either the
`T` of `ok` or the `E` of `err` — they never coexist). Payload slot `i` for a
variant lives at offset `8 + i*8`. Construction `__alloc`s
`8 + max_payload_slots*8`, stores the tag, and stores each payload; `match`
loads the tag, branches, and binds each arm's payload slot to a local (§3.3).

## 2. The allocator

The delivered `__alloc` is a pure bump pointer that never frees. This phase
replaces it with a **production allocator**: bump for fast-path allocation, a
**segregated free list** for reclaiming freed blocks, `memory.grow` for
expansion, and a defined OOM trap. It is emitted as internal WASM functions
(`__alloc`, `__free`, `__realloc`) invisible to Lullaby source.

### 2.1 Object header and the control block

Every allocation is prefixed by an 8-byte header so `__free`/`__realloc` know
the block size and free-list membership:

```
object header (8 bytes, at ptr-8):
0   4   size  : i32    (usable payload bytes, 8-aligned, header excluded)
4   4   flags : i32    (bit0 = free; bit1..: size-class index / next-free ptr)
```

The **control block** (between the literal pool and `HEAP_BASE`, §1.2) holds:

```
0   4   brk        : i32   (mirror of the bump global; kept in the global too)
4   4   free_lists : i32[N]  (heads of N segregated free lists, 0 = empty)
```

`N` size classes cover 16, 24, 32, 48, 64, 96, 128, 192, 256, … bytes (rounding
up powers-of-two with half steps) plus one "large" list for anything bigger.
Each free block reuses its own payload to store a `next` pointer (intrusive
list), so the free list costs no extra memory.

### 2.2 `__alloc(size i32) -> i32` (allocation)

```
size'      = round_up_8(size) + HEADER (8)
class      = size_class(size')
if free_lists[class] != 0:
    block  = free_lists[class]           # pop the head
    free_lists[class] = block.next
    block.flags &= ~FREE
    return block + HEADER
# bump path
new_brk    = brk + size'
if new_brk > memory_size_bytes:
    need_pages = ceil((new_brk - memory_size_bytes) / 65536)
    if memory.grow(need_pages) == -1:
        __oom()                          # traps (§2.4)
p          = brk
brk        = new_brk                     # write global + control block
p.size     = size' - HEADER
p.flags    = class << 2
return p + HEADER
```

Rounding to 8 keeps every returned pointer 8-aligned so `i64`/`f64` slot
load/stores are aligned. The size class is stored in the header so `__free` can
push onto the right list in O(1).

### 2.3 `__free(ptr i32)` and `__realloc(ptr, new_size)`

`__free(ptr)` reads `header = ptr - 8`, sets the FREE flag, and pushes the block
onto `free_lists[header.class]` (writing the old head into the block's first
payload word as `next`). No coalescing in the first increment (segregated lists
recycle same-class blocks, which covers the dominant churn: repeated
`push`/`map_set` buffer turnover and short-lived enum/`option` values).
`__realloc(ptr, new)` fast-paths when the existing class already fits (returns
`ptr`), else `__alloc`s a new block, copies `min(old,new)` bytes with a
`memory.copy` (bulk-memory opcode `0xFC 0x0A`), and `__free`s the old one — used
by `list`/`map`/string-builder growth (§3.5).

### 2.4 Failure handling

`__oom()` executes the `unreachable` opcode (`0x00`), which traps the instance.
The JS glue (§4.3) catches the trap and reports it as
`RuntimeError: out of memory` rather than a silent corruption. This matches the
interpreters' behavior of aborting on allocation failure. Out-of-bounds
`list`/`array`/`string` index likewise traps (WASM native OOB trap for the
inline `array`; an explicit `i32.ge_u` bounds check + `unreachable` for `list`
because its `data` buffer is separately sized). Integer divide-by-zero already
traps via `i64.div_s`.

### 2.5 Drop / reclamation policy

The first increment frees **eagerly at end of scope for values proven
non-escaping** by the existing IR frame-layout analysis
(`frame_layout` already computes per-scope reverse-order cleanup plans — see
[[repository_map.md]] `crates/lullaby_ir`). A `let` binding of heap type whose
value does not flow to a return, an outer binding, or a call argument is
`__free`d at scope exit (reverse declaration order), mirroring the native
backend's cleanup sequencing. Values that escape are not freed this increment
(they leak until instance teardown, which is acceptable for the bounded,
short-lived programs the parity harness runs and for request-scoped web
handlers). A tracing GC or reference counting is deferred (§8); the free
list + escape-local frees give real reclamation without changing observable
semantics.

## 3. Lowering

Lowering extends the delivered walk in `wasm.rs` (`lower_expr`/`lower_stmt`).
The eligibility gate (`eligibility`, `slot_val_type`, `value_val_type`) widens
to accept enum/`option`/`result`/`list`/`map` types and the runtime string and
collection builtins; a function only stays skipped if it uses a construct still
outside the widened set.

### 3.1 Construction

- **struct / fixed array** — unchanged (delivered).
- **enum variant** — `Variant(args)` (or bare unit `Variant`) `__alloc`s
  `8 + max_payload_slots*8`, `i32.store`s the tag at offset 0, and stores each
  payload arg at `8 + i*8` by its slot type; leaves the pointer.
- **`option`/`result`** — `some(v)`/`ok(v)`/`err(e)` lower exactly as one-payload
  enums with the fixed tags; `none` stores only the tag. The IR already carries
  the concrete `option<T>`/`result<T,E>` type (from the context inference in
  [[option_result_design.md]]), so the payload slot type is known at lowering.
- **`list`** — `list_new()` `__alloc`s the 16-byte handle with `len=0`,
  `cap=0`, `data=0` (a first `push` allocates the buffer). The element slot type
  comes from the `list<T>` annotation.
- **`map`** — `map_new()` `__alloc`s the 16-byte handle (`len=0`, `cap=0`,
  `data=0`). Key/value slot types come from `map<K,V>`.
- **string literal** — unchanged (interned pool), now with the added `byte_len`
  field.

### 3.2 Field / element access

- **`.field`** — unchanged: `target` pointer + `field_index*8` offset, typed
  load.
- **fixed `array[i]`** — unchanged: `base + 4 + i*8`, WASM OOB trap.
- **`list` `get(xs, i)`** — load `xs.data`, compute `data + i*8`, bounds-check
  `i <u len` then load (traps on OOB via explicit check because `data` is a
  separate buffer). Returns the element slot value.
- **`list` `set(xs, i, v)` / `push` / `pop`** — mutate through the handle (§3.5).
- **`map` `map_get`** — linear scan (§3.6), returns `option<V>`.

### 3.3 `match` / `option` / `result`

`match scrutinee` lowers to: evaluate the scrutinee pointer into a scratch `i32`
local, `i32.load` the tag, then a chain of `if tag == k` blocks (one per arm) in
arm order, with the wildcard arm as the final `else`. Each arm, before its body,
binds its payload slots: for arm pattern `Variant(a, b)` it loads
`ptr + 8 + 0*8` into `a`'s local and `ptr + 8 + 1*8` into `b`'s local (typed by
the variant's declared payload types). A `match` used as an expression uses a
typed WASM `if` result (the arm value type) instead of a void block, exactly as
the delivered `and`/`or` short-circuit uses a typed `if`. Exhaustiveness is
already guaranteed by semantics, so no default trap is needed unless a wildcard
is absent and the tag is somehow out of range (defensively, a trailing
`unreachable`). `option`/`result` `match` is the same path with the fixed tags.

### 3.4 Scalar/heap boundary

A value crossing between a scalar slot and a heap slot is always an `i32`
pointer on the WASM side; no boxing is needed because scalars stay scalars and
heap values stay pointers. The one real boundary is **host imports**: a
`string`-typed argument to `console_log`/`dom_set_text` is lowered by the
delivered `lower_string_ptr_len` to `(ptr, len)`; this phase updates it to pass
`byte_len` (offset 4) rather than `char_count` when the host needs byte length
for `TextDecoder` (the loader decodes `memory[ptr+8 .. ptr+8+byte_len]`).
`to_string(x)` (§3.6) is the scalar→heap direction: an `i64`/`f64`/`bool` scalar
is formatted into a freshly allocated string.

### 3.5 Collection growth (`push`, `set`, `pop`, `map_set`, `map_del`)

These lower to calls to emitted internal helpers (one per element/key/value slot
shape, monomorphized on slot type, so an `i64` list and a pointer list get
distinct but structurally identical helpers):

- `__list_push(handle, v)` — if `len == cap`, `__realloc` `data` to
  `max(cap*2,4)*8` and update `cap`/`data`; store `v` at `data+len*8`;
  `len += 1`. Returns the handle (the builtins are value-returning; the
  interpreter returns the grown container, so lowering returns the same handle
  pointer to stay at parity).
- `__list_pop(handle)` — `len -= 1`; return the removed slot as `option<T>`
  (matching the interpreter's `pop -> option`).
- `__map_set(handle, k, v)` — scan for `k`; overwrite or append+grow.
- `__map_del(handle, k)` — scan; compact left; return the grown/updated handle.

Because the interpreter treats these as returning a (logically new) container
but the WASM layout mutates in place and returns the same pointer, **aliasing
must match the interpreter**. The interpreter's `Value::Array`/`Value::Map` are
value types (cloned on assignment); to preserve that, the lowering of a
container `let`/assignment/argument that the escape analysis marks as
potentially aliased emits a `__clone` (deep copy of the handle + buffer). Where
the escape analysis proves single ownership (the common `push`-in-a-loop case),
the clone is elided. This keeps observable results identical while avoiding a
copy on every mutation.

### 3.6 String builtins in linear memory

The delivered backend skips any function that builds strings at runtime. This
phase lowers them to emitted internal helpers over the `[char_count][byte_len]
[utf8]` layout:

- **`+` concat (`a + b`)** — `__str_concat(a, b)`: `__alloc`
  `8 + a.byte_len + b.byte_len`, `memory.copy` both byte runs, sum the char
  counts and byte lengths into the header. Returns the new string.
- **`to_string(x)`** — `__i64_to_str` / `__f64_to_str` / `__bool_to_str`: format
  into a small stack scratch buffer in memory, then `__alloc` the exact string.
  Integer formatting is a standard divide-by-10 loop emitted once.
- **`substring(s, start, count)`** — `__substring`: char-walk the UTF-8 to find
  the byte range (respecting multi-byte scalars so char indices match the
  interpreter), `__alloc` + `memory.copy` the slice, recount.
- **`split(s, sep)`** — `__split`: returns a `list<string>`; scan for `sep`,
  emit each piece as a `__substring`, `__list_push` into a fresh list.
- **string `eq`** (used by `map` keys and `==`) — `__str_eq`: compare
  `byte_len` then `memory`-compare the bytes.

All helpers are emitted once per module (deduped) and only when referenced, so a
string-free program's bytes are unchanged.

## 4. Host imports and JS glue

### 4.1 The import interface

The delivered `env` imports (`log_i64`, `console_log`, `dom_set_text`) stay at
their fixed low indices. This phase reserves a **stable, versioned import table**
so adding imports never renumbers existing ones (append-only), driven by
`IMPORT_FUNC_COUNT`:

Console + core (delivered): `env.log_i64(i64)`, `env.console_log(ptr, len)`,
`env.dom_set_text(id_ptr, id_len, text_ptr, text_len)`.

Minimal DOM surface (this phase, append-only after index 2):

```
3  env.dom_get_text(id_ptr, id_len) -> (ret_ptr i32)   # returns a heap string
4  env.dom_set_html(id_ptr,id_len, html_ptr,html_len)
5  env.dom_add_class(id_ptr,id_len, cls_ptr,cls_len)
6  env.dom_on_click(id_ptr,id_len, handler_index i32)   # register export as cb
7  env.now() -> f64                                      # monotonic ms
```

`dom_get_text` returns an `i32` pointer to a string the host **allocates in the
module's memory** by calling the exported `__alloc` (§4.2), so the returned
value is an ordinary Lullaby `string`. `dom_on_click` takes the WASM function
index of an exported handler (`export fn on_click() -> void`); the host installs
a DOM listener that calls it. These are the smallest surface that makes an
interactive frontend possible; a richer surface (attributes, event payloads,
fetch) is deferred (§8) but slots in append-only.

Every import remains **optional at the semantic layer**: a function only uses an
import if it calls the corresponding builtin, and the builtin is only in scope
when documented in [[standard_library.md]]. On the interpreters each new DOM
builtin prints a deterministic line (like the delivered `id=text`) so parity
holds without a browser.

### 4.2 Exports for the host

The module exports, in addition to `"memory"` and every compiled function:

- `__alloc(size i32) -> i32` — so the host can allocate a buffer *inside* the
  module's memory to hand a string in (e.g. `dom_get_text`, or marshalling a
  JS string argument into a Lullaby `string`).
- `__free(ptr i32)` — so the host can release a buffer it asked `__alloc` for.

Exporting the allocator is the standard pattern (matches how wasm-bindgen and
Emscripten expose `malloc`/`free`) and is what makes host→module string passing
possible.

### 4.3 The generated JS loader

`lullaby wasm` optionally emits a companion `<stem>.js` (and, with `--html`, a
self-contained `<stem>.html`) that instantiates the module, wires the imports,
and marshals strings. The loader is dependency-free ES module code:

```js
// generated <stem>.js — no bundler, no CDN, open locally
const HEADER = 8; // object header bytes

export async function load(url, hooks = {}) {
  let mem, alloc, free, exports;
  const dec = new TextDecoder("utf-8");
  const enc = new TextEncoder();

  // Read a Lullaby string (ptr -> {char_count,byte_len,utf8}) out of memory.
  function readStr(ptr) {
    const u32 = new Uint32Array(mem.buffer, ptr, 2);
    const byteLen = u32[1];
    const bytes = new Uint8Array(mem.buffer, ptr + HEADER, byteLen);
    return dec.decode(bytes);
  }
  // Marshal a JS string INTO module memory as a Lullaby string; return ptr.
  function writeStr(s) {
    const utf8 = enc.encode(s);
    const ptr = alloc(HEADER + utf8.length); // usable payload from exported __alloc
    const u32 = new Uint32Array(mem.buffer, ptr, 2);
    u32[0] = [...s].length;       // char_count (code points)
    u32[1] = utf8.length;         // byte_len
    new Uint8Array(mem.buffer, ptr + HEADER, utf8.length).set(utf8);
    return ptr;
  }

  const env = {
    log_i64: (x) => (hooks.log ?? console.log)(x),
    console_log: (ptr, len) => console.log(readStr(ptr)),
    dom_set_text: (idP, idL, txtP, txtL) => {
      const el = document.getElementById(readStr(idP));
      if (el) el.textContent = readStr(txtP);
    },
    dom_get_text: (idP, idL) => {
      const el = document.getElementById(readStr(idP));
      return writeStr(el ? el.textContent : "");
    },
    dom_set_html: (idP, idL, hP, hL) => {
      const el = document.getElementById(readStr(idP));
      if (el) el.innerHTML = readStr(hP);
    },
    dom_add_class: (idP, idL, cP, cL) => {
      const el = document.getElementById(readStr(idP));
      if (el) el.classList.add(readStr(cP));
    },
    dom_on_click: (idP, idL, fnIndex) => {
      const el = document.getElementById(readStr(idP));
      if (el) el.addEventListener("click", () => table.get(fnIndex)());
    },
    now: () => performance.now(),
  };

  const { instance } = await WebAssembly.instantiateStreaming(fetch(url), { env });
  exports = instance.exports;
  mem = exports.memory;
  alloc = exports.__alloc;
  free = exports.__free;
  return exports;
}
```

`readStr` decodes using `byte_len` (§3.4), so multi-byte UTF-8 round-trips.
`writeStr` uses the exported `__alloc` (§4.2) to place a JS string into module
memory. `instantiateStreaming` needs no bundler; the `--html` variant inlines
the same script and a relative `fetch("<stem>.wasm")`, matching the delivered
`examples/valid/fullstack/index.html` (no CDN, no remote assets) so a frontend
opens directly from disk. Traps (OOM §2.4, OOB) surface as a rejected promise /
`WebAssembly.RuntimeError`, which the loader may catch and report.

## 5. CLI

`lullaby wasm [--verbose] [--js] [--html] [-o out.wasm] <file.lby>` extends the
delivered command:

- Validation and IR lowering are unchanged (identical to `compile`).
- The heap-widened `emit_wasm_module` now compiles **most** functions: any
  function whose signature and body use only supported types (scalars, string,
  struct, array, list, map, enum, option, result) and supported builtins is
  eligible. Functions using still-unsupported constructs (threads/channels,
  sockets, file I/O, `async`, inline `asm`) remain skipped with a reason.
- `--js` also writes `<stem>.js` (§4.3); `--html` also writes a self-contained
  `<stem>.html`. `--verbose` lists compiled/skipped functions and, for skips,
  the reason.
- `L0338` (no eligible function) is now rare — it fires only when every function
  uses an unsupported construct.

Eligibility widening is the headline user-visible change: programs that were
"scalar-only" become largely compilable, so the WASM path stops being a toy
subset and becomes a real target for the language's data-structure-using code
(the classification/scoring domain modules in `examples/valid/fullstack/`, for
instance, can drop their scalar-only restriction).

## 6. Testing (execution parity for heap programs)

The delivered strategy stands: the interpreter is ground truth, and a
node/wasmtime-gated test asserts the WASM result matches; when no runtime is
found the execution test skips gracefully while the structural encoder tests
always run. This phase adds:

- **Heap fixtures** under `tests/fixtures/valid/`, each running identically on
  all interpreters and, under node, on WASM:
  - `wasm_enum.lby` — user enum + `match` (tag dispatch, payload binding).
  - `wasm_option_result.lby` — `option`/`result` construction + `match` + `map_get`.
  - `wasm_list.lby` — `list_new`/`push`/`get`/`pop`/`len`, growth past initial cap.
  - `wasm_map.lby` — `map_new`/`map_set`/`map_get`/`map_has`/`map_del`, insertion order.
  - `wasm_strings.lby` — `+` concat, `to_string`, `substring`, `split`.
- **Layout decode tests** (structural, always run): assert an enum value in
  `memory` decodes to `[tag][payload]`, a `list` handle to `[len][cap][data]`,
  and a runtime-concatenated string to `[char_count][byte_len][utf8]` — the same
  approach the delivered `wasm_heap_types_execution_parity_with_node` uses to
  decode the interned string layout.
- **Allocator tests** (structural + node): a fixture that `push`es enough to
  force `memory.grow`, asserting the node result matches and the grown module
  still returns correct values; a fixture that frees and re-allocates
  (same-class reuse) asserting no unbounded growth.
- **JS-glue round-trip** (node, gated): instantiate with a stub `env` DOM,
  call `dom_get_text`-driven code, assert the marshalled string round-trips
  through `writeStr`/`readStr`.
- **wasmtime alternative**: the same fixtures run under `wasmtime` if present
  (for non-DOM ones), covering environments without node.

Order-independence is not needed here (single-threaded), so parity is exact
equality, unlike the concurrency harness (see [[concurrency_design.md]]).

## 7. Diagnostics

- **`L0338`** (unchanged) — no function eligible for WASM; now rare.
- A **skip reason** is emitted per skipped function under `--verbose` (existing
  `SkippedFunction`), e.g. "uses channels (not supported by the WASM backend)".
- No new *hard* diagnostic is required for the heap types themselves — an
  eligible function compiles; an ineligible one skips. If `--js`/`--html` is
  requested but no function is exported (nothing to call), emit a warning rather
  than an error.
- Runtime traps (OOM, OOB) are not compile-time diagnostics; they surface at run
  time as WASM `RuntimeError`, documented in the offline docs' limitations
  section and reported by the JS loader (§4.3).

## 8. Scope and sequencing

**First increment (production-complete):**

1. Allocator upgrade — object header, segregated free lists, `memory.grow`, OOM
   trap, `__free`/`__realloc`, escape-local eager free (§2). Exported
   `__alloc`/`__free` (§4.2).
2. `enum`/`match` lowering — tag+payload layout, tag dispatch, payload binding,
   `match`-as-expression (§1.3, §3.3).
3. `option`/`result` on the enum path (§3.1, §3.3).
4. `list<T>` — handle + growable buffer, `list_new`/`push`/`get`/`set`/`pop`/
   `len` (§1.3, §3.5).
5. `map<K, V>` — insertion-ordered entry buffer, full builtin set (§1.3, §3.5,
   §3.6 string-key compare).
6. Runtime strings — `byte_len` header widening, `+`/`to_string`/`substring`/
   `split`/`eq` helpers (§1.3, §3.6).
7. Widened eligibility + `L0338`-rare (§5).
8. JS loader + minimal DOM imports + `--js`/`--html` (§4).
9. Fixtures + node/wasmtime parity + allocator/layout structural tests (§6).

**Deferred (later increments, all append-only / non-breaking):**

- Block **coalescing** and a compacting or generational GC; reference counting
  for shared containers (the free list + escape frees suffice first).
- A **hash-table `map`** (the ordered association list is correct but O(n); a
  hash layout is a drop-in once profiling justifies it and can preserve order
  with an auxiliary index).
- **Richer DOM/event surface** — event payloads, attribute get/set, `fetch`,
  timers — layered on §4.1's append-only import table.
- **Threads/channels, sockets, file I/O, `async`** on WASM (need WASI or a
  host-provided async surface; skipped for now).
- **`i32`-slot packing** for small scalars (the uniform 8-byte slot is simple
  and correct; packing is a size optimization).
- A **`.wat` text emitter** for debugging.

## 9. Why these choices

- **Extend `wasm.rs`, not a new crate.** The delivered encoder already owns
  section encoding, LEB128, the import fix-up, the string pool, and `__alloc`;
  the heap phase is a widening of the same walk, keeping one std-only encoder.
- **Uniform 8-byte slots + 8-aligned allocations.** Removes all per-field
  alignment computation, keeps `i64`/`f64` naturally aligned, and makes offset
  math a constant multiply — the delivered struct/array layout already proved
  this out.
- **Tag + widest-payload enum layout.** Fixed per-type size lets an enum live in
  one parent slot as a pointer and makes `match` a constant-offset load per
  payload; it mirrors how the native/interpreter representations already treat a
  variant as `{tag, payload}`.
- **Insertion-ordered `map` first.** Guarantees byte-for-byte parity with the
  interpreter's `Value::Map(Vec<(K,V)>)` (ordering is observable), deferring the
  hash table until it is measured to matter — correctness before speed.
- **Segregated free list, no coalescing.** Reclaims the dominant allocation
  churn (container growth, short-lived enums/options) in O(1) without the
  complexity or fragmentation-analysis burden of a coalescing allocator; a real
  GC can layer on later without changing observable semantics.
- **Escape-analysis-driven eager free + copy-on-alias.** Reuses the IR's
  existing frame-layout cleanup plans, gives real reclamation for the common
  case, and preserves the interpreter's value semantics for containers without a
  clone on every mutation.
- **Exported `__alloc`/`__free` + a std-only JS loader.** The standard wasm
  interop pattern; makes host→module string marshalling (and thus a real,
  locally-openable web frontend) possible with zero third-party tooling, exactly
  the offline, no-CDN constraint the project already holds itself to.
- **Interpreter as ground truth, node/wasmtime-gated parity.** Unchanged from
  the delivered story; every heap layout is validated against the language's
  existing correctness anchor.
