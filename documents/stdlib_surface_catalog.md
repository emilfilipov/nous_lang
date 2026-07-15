# Standard-Library / Builtin Surface Catalog

**Purpose:** an evidence-based enumeration of Lullaby's *currently-implemented*
builtin/prelude surface, read from the compiler source (not just the prose docs),
to give [road_to_1_0_stable.md](road_to_1_0_stable.md) item **B2** ("Concrete
stdlib contents") the concrete inventory it needs. It records each builtin's
signature, category, and backend coverage; flags doc-vs-code mismatches; and ends
with a **first-pass stable-vs-extended proposal** to inform — not pre-empt — the
eventual B2 freeze decision.

Canonical prose catalog: [standard_library.md](standard_library.md). API-stability
posture (freeze a small stable core, version the rest): B2 in
[road_to_1_0_stable.md](road_to_1_0_stable.md). Primitive-vs-battery boundary:
[stdlib_modules_design.md](stdlib_modules_design.md).

## Method and sources

The authoritative list of builtins is the semantic checker's builtin-signature
dispatch — the `match name { ... }` in
`crates/lullaby_semantics/src/semantics_checker_calls.rs` (`Checker::check_call`,
lines ~38–1717). Every name there is a compiler-provided builtin; anything falling
through to the `_ =>` arm is a user function. This was cross-checked against the
runtime interpreter dispatch (`crates/lullaby_runtime/src/interpreter.rs`
`call_builtin`, lines ~700–898), whose arms mirror the registry one-for-one, and
against the IR/bytecode interpreters
(`crates/lullaby_ir/src/ir_interpreter.rs`, `bytecode_vm.rs`) and the
native/WASM lowerers (`crates/lullaby_ir/src/native_object*.rs`,
`wasm_lowering*.rs`).

Not counted as "builtins" here (they are keywords / control forms / type
constructors, handled elsewhere in the pipeline, not in the call dispatch):
`throw` / `try` / `catch`, `match`, and the `option`/`result` constructors
`some` / `none` / `ok` / `err`.

**Total implemented builtins: ~209**, across the categories below.

### Backend-coverage legend

- **Interp** — all three interpreters (AST = runtime, IR, bytecode) at parity.
  Every builtin below is implemented here; the docs repeatedly assert bit-identical
  results across the three.
- **Native** — the hand-written native object emitter (`native_object*.rs`).
  Whole-function eligibility model: a function compiles to native **only if every
  operation in it is supported**; otherwise the *entire function* gracefully
  skips to the interpreters (surfaced as `L0339` when nothing is eligible). So
  "Native: subset" means the builtin is lowered when present; "interp-only" means a
  function using it always falls back.
- **WASM** — the WebAssembly backend (`wasm_lowering*.rs`), same whole-function
  subset model as native, plus three host-import builtins it lowers specially
  (`wasm_log`, `console_log`, `dom_set_text`).

Native/WASM coverage below is stated at **category granularity**, grounded in
directly-verified lowering code plus the per-builtin "interpreter-tier" / "native
and WASM backends are subsets" annotations in
[standard_library.md](standard_library.md). Where a category is a mix, the
interpreter-only members are called out. The machine-precise per-builtin
native/WASM table lives in the lowerers themselves; this catalog does not
reproduce it line-for-line.

---

## 1. Memory / heap intrinsics (4)

| Builtin | Signature | Backends |
|---|---|---|
| `alloc` | `alloc(value T) -> ptr<T>` | Interp; Native/WASM subset |
| `load` | `load(p ptr<T>) -> T` | Interp; Native/WASM subset |
| `store` | `store(p ptr<T>, value T) -> void` | Interp; Native/WASM subset |
| `dealloc` | `dealloc(p ptr<T>) -> void` | Interp; Native/WASM subset |

## 2. References / rc / raw pointers / layout (15)

| Builtin | Signature | Backends |
|---|---|---|
| `rc_new` | `rc_new(value T) -> rc<T>` | Interp; Native subset |
| `rc_clone` | `rc_clone(r rc<T>) -> rc<T>` | Interp; Native subset |
| `rc_release` | `rc_release(r rc<T>) -> void` | Interp; Native subset |
| `rc_get` | `rc_get(r rc<T>) -> T` | Interp; Native subset |
| `rc_borrow` | `rc_borrow(r rc<T>) -> ref<T>` | Interp; Native subset |
| `ref_get` | `ref_get(r ref<T>) -> T` | Interp; Native subset |
| `ptr_read` | `ptr_read(p ptr<T>) -> T` *(unsafe)* | Interp; Native subset |
| `ptr_write` | `ptr_write(p ptr<T>, value T) -> void` *(unsafe)* | Interp; Native subset |
| `size_of` | `size_of(x) -> i64` (compile-time layout query) | Interp (folds to const); Native subset |
| `align_of` | `align_of(x) -> i64` (compile-time layout query) | Interp (folds to const); Native subset |
| `offset_of` | `offset_of(x struct, field string-literal) -> i64` | Interp (folds to const); Native subset |
| `ptr_to_int` | `ptr_to_int(p ptr<T>) -> i64` *(unsafe)* | Interp; Native subset |
| `int_to_ptr` | `int_to_ptr(n i64) -> ptr<T>` *(unsafe)* | Interp; Native subset |
| `volatile_load` | `volatile_load(p ptr<T>) -> T` *(unsafe)* | Interp (as `load`); Native realizes volatility |
| `volatile_store` | `volatile_store(p ptr<T>, value T) -> void` *(unsafe)* | Interp (as `store`); Native realizes volatility |

Diagnostics: `L0310`/`L0311` (ptr type), `L0331` (ptr_write/volatile_store value),
`L0431` (size_of/align_of/offset_of layout).

## 3. Conversion / formatting (17)

| Builtin | Signature | Backends |
|---|---|---|
| `to_string` | `to_string(x scalar) -> string` | Interp; Native/WASM |
| `char_code` | `char_code(c char) -> i64` | Interp; Native |
| `char_from` | `char_from(i i64) -> char` | Interp; Native subset |
| `byte` | `byte(i i64) -> byte` (runtime-errors outside 0–255) | Interp; Native subset |
| `byte_val` | `byte_val(b byte) -> i64` | Interp; Native subset |
| `to_i8`/`to_i16`/`to_i32` | `to_i<N>(x i64) -> i<N>` (wrapping) | Interp; Native/WASM |
| `to_u8`/`to_u16`/`to_u32`/`to_u64` | `to_u<N>(x i64) -> u<N>` (wrapping) | Interp; Native/WASM |
| `to_isize`/`to_usize` | `to_isize/to_usize(x i64) -> isize/usize` | Interp; Native/WASM |
| `to_i64` | `to_i64(x fixed-width int) -> i64` (widen) | Interp; Native/WASM |
| `to_f32` | `to_f32(x f64) -> f32` | Interp; Native/WASM |
| `to_f64` | `to_f64(x f32) -> f64` | Interp; Native/WASM |

## 4. Overflow-aware arithmetic (11)

Each takes two operands of the *same* integer type `T` — `i64` or a fixed-width
kind. Integer arithmetic wraps by default (a conscious 1.0 decision, A4); these
builtins make overflow handling explicit. "Native/WASM" means the fixed-width
add/sub/mul forms are lowered directly; the `i64` forms and
`checked_div`/`checked_rem` are not yet lowered there and cleanly skip to the
interpreters (never miscompiled).

| Builtin family | Signature | On overflow | Backends |
|---|---|---|---|
| `checked_add`/`checked_sub`/`checked_mul` | `(T, T) -> option<T>` | `none` | Interp (all `T`); Native/WASM (fixed-width) |
| `checked_div`/`checked_rem` | `(T, T) -> option<T>` | `none` on zero divisor or signed `MIN / -1` div overflow | Interp; Native/WASM skip |
| `saturating_add`/`saturating_sub`/`saturating_mul` | `(T, T) -> T` | clamps | Interp (all `T`); Native/WASM (fixed-width) |
| `wrapping_add`/`wrapping_sub`/`wrapping_mul` | `(T, T) -> T` | wraps | Interp (all `T`); Native/WASM (fixed-width) |

## 5. Character classification (6)

Each `char -> bool`; a non-`char` argument is `L0389`.

`is_digit`, `is_alpha`, `is_alnum`, `is_whitespace`, `is_upper`, `is_lower`.
Backends: Interp; `is_digit` verified in native, the rest are native-subset.

## 6. Collections — list / array (20)

| Builtin | Signature | Backends |
|---|---|---|
| `len` | `len(x string\|array<T>\|list<T>) -> i64` | Interp; Native/WASM |
| `array_fill` | `array_fill(n i64, value T) -> array<T>` | **Interp only** (runtime length → skips native/WASM) |
| `list_new` | `list_new() -> list<T>` | Interp; Native/WASM |
| `push` | `push(l list<T>, x T) -> list<T>` | Interp; Native/WASM |
| `get` | `get(l list<T>, i i64) -> T` | Interp; Native/WASM |
| `set` | `set(l list<T>, i i64, x T) -> list<T>` | Interp; Native/WASM |
| `pop` | `pop(l list<T>) -> list<T>` | Interp; Native/WASM |
| `reverse` | `reverse(l list<T>) -> list<T>` | Interp; Native subset |
| `concat` | `concat(a list<T>, b list<T>) -> list<T>` | Interp; Native subset |
| `slice` | `slice(l list<T>, start i64, end i64) -> list<T>` | Interp; Native subset |
| `sort` | `sort(l list<T>) -> list<T>` (`T`=i64/f64/string) | Interp; Native subset |
| `sort_by` | `sort_by(l list<T>, cmp fn(T,T)->i64) -> list<T>` | **Interp only** (native/WASM fall back) |
| `list_index_of` | `list_index_of(l list<T>, x T) -> i64` | Interp; Native subset |
| `list_contains` | `list_contains(l list<T>, x T) -> bool` | Interp; Native subset |
| `list_sum` | `list_sum(l list<T>) -> T` (`T`=i64/f64) | Interp; Native subset |
| `list_min` | `list_min(l list<T>) -> option<T>` | Interp; Native subset |
| `list_max` | `list_max(l list<T>) -> option<T>` | Interp; Native subset |
| `list_map` | `list_map(l list<T>, f fn(T)->U) -> list<U>` | **Interp only** (native/WASM fall back) |
| `list_filter` | `list_filter(l list<T>, pred fn(T)->bool) -> list<T>` | **Interp only** (native/WASM fall back) |
| `list_reduce` | `list_reduce(l list<T>, init U, f fn(U,T)->U) -> U` | **Interp only** (native/WASM fall back) |

Diagnostics: `L0373` (len), `L0387` (element type), `L0433` (array_fill negative length).

## 7. Collections — map (8)

`map<K,V>` is insertion-ordered; keys are `i64`/`string`. Functions that *iterate*
a map are interpreter-tier (native i64-scalar backend skips them via `L0339`).

| Builtin | Signature | Backends |
|---|---|---|
| `map_new` | `map_new() -> map<K,V>` | Interp; Native subset |
| `map_set` | `map_set(m, k K, v V) -> map<K,V>` | Interp; Native subset |
| `map_get` | `map_get(m, k K) -> option<V>` | Interp; Native subset |
| `map_has` | `map_has(m, k K) -> bool` | Interp; Native subset |
| `map_len` | `map_len(m) -> i64` | Interp; Native subset |
| `map_keys` | `map_keys(m) -> list<K>` (insertion order) | **Interp only** |
| `map_values` | `map_values(m) -> list<V>` (insertion order) | **Interp only** |
| `map_del` | `map_del(m, k K) -> map<K,V>` | Interp; Native subset |

Diagnostic: `L0388` (map key/value type).

## 8. Strings (21)

| Builtin | Signature | Backends |
|---|---|---|
| `substring` | `substring(s, start i64, end i64) -> string` | Interp; Native/WASM |
| `find` | `find(s, needle) -> i64` (`-1` if absent) | Interp; Native/WASM |
| `contains` | `contains(s, needle) -> bool` | Interp; Native/WASM |
| `starts_with` | `starts_with(s, prefix) -> bool` | Interp; Native/WASM |
| `ends_with` | `ends_with(s, suffix) -> bool` | Interp; Native/WASM |
| `repeat` | `repeat(s, count i64) -> string` | Interp; Native subset |
| `split` | `split(s, sep) -> array<string>` | Interp; Native/WASM |
| `words` | `words(s) -> array<string>` *(shadowable)* | Interp; Native subset |
| `count` | `count(s, sub) -> i64` *(shadowable)* | Interp; Native subset |
| `join` | `join(parts array<string>, sep) -> string` | Interp; Native/WASM |
| `trim` | `trim(s) -> string` | Interp; Native/WASM |
| `replace` | `replace(s, from, to) -> string` | Interp; Native subset |
| `upper` | `upper(s) -> string` | Interp; Native/WASM |
| `lower` | `lower(s) -> string` | Interp; Native/WASM |
| `chars` | `chars(s) -> list<char>` | Interp; Native subset |
| `string_from_chars` | `string_from_chars(cs list<char>) -> string` | Interp; Native subset |
| `to_bytes` | `to_bytes(s) -> list<byte>` | Interp; Native subset |
| `from_bytes` | `from_bytes(b list<byte>) -> result<string,string>` | Interp; Native subset |
| `byte_len` | `byte_len(s) -> i64` | Interp; Native subset |
| `parse_i64` | `parse_i64(s) -> result<i64,string>` | Interp; Native/WASM |
| `parse_f64` | `parse_f64(s) -> result<f64,string>` | Interp; Native subset |

`words` and `count` are guarded: a user-defined `words`/`count` shadows the builtin
(the dispatch yields to the user-call path). String concat is the `+` operator, not
a builtin. Diagnostic: `L0375` (string-builtin family).

## 9. Math (19)

| Builtin | Signature | Backends |
|---|---|---|
| `abs` | `abs(x T) -> T` (`T`=i64/f64) | Interp; Native |
| `min` / `max` | `(T, T) -> T` (`T`=i64/f64) | Interp; Native |
| `pow` | `pow(base T, exp T) -> T` (int exp ≥ 0) | Interp; Native subset |
| `clamp` | `clamp(x T, lo T, hi T) -> T` | Interp; Native |
| `sign` | `sign(x T) -> i64` | Interp; Native |
| `gcd` | `gcd(a i64, b i64) -> i64` | Interp; Native |
| `sqrt` | `sqrt(x f64) -> f64` | Interp; Native/WASM |
| `floor` / `ceil` / `round` | `(f64) -> f64` | Interp; Native subset |
| `sin`/`cos`/`tan`/`atan`/`exp`/`ln`/`log10` | `(f64) -> f64` | **Interp only** (transcendentals) |
| `atan2` | `atan2(y f64, x f64) -> f64` | **Interp only** |

Diagnostics: `L0374` (semantic), `L0417` (runtime).

## 10. Bitwise intrinsics (6)

All on `i64 -> i64`. `rotate_left`, `rotate_right` (masked `& 63`); `count_ones`,
`leading_zeros`, `trailing_zeros`, `reverse_bytes`. Backends: Interp; native
subset. Diagnostics `L0374`/`L0417`.

## 11. Standard streams / host I/O (9)

| Builtin | Signature | Backends |
|---|---|---|
| `print` | `print(s string) -> void` | Interp; Native/WASM |
| `println` | `println(s string) -> void` | Interp; Native/WASM |
| `warn` | `warn(s string) -> void` (stderr) | Interp; Native subset |
| `flush` | `flush() -> void` | Interp; Native subset |
| `read_line` | `read_line() -> option<string>` (stdin) | **Interp only** |
| `read_all` | `read_all() -> string` (stdin) | **Interp only** |
| `wasm_log` | `wasm_log(x i64) -> void` | Interp; **WASM host import** (`env.log_i64`) |
| `console_log` | `console_log(s string) -> void` | Interp; **WASM host import** (`env.console_log`) |
| `dom_set_text` | `dom_set_text(id string, text string) -> void` | Interp; **WASM host import** (`env.dom_set_text`) |

## 12. Filesystem (14)

All one-shot (no stateful handles/seeking). **Interp only** (native i64-scalar
backend skips them like every heap/OS builtin).

`read_file(path) -> string`, `write_file(path, content) -> void`,
`append_file(path, content) -> void`, `file_exists(path) -> bool`,
`read_lines(path) -> list<string>`, `read_bytes(path) -> list<byte>`,
`write_bytes(path, data list<byte>) -> void`, `file_size(path) -> i64`,
`is_file(path) -> bool`, `is_dir(path) -> bool`, `list_dir(path) -> list<string>`,
`make_dir(path) -> void`, `remove_file(path) -> void`, `remove_dir(path) -> void`.

Diagnostics: `L0333` (arg), `L0414` (read/metadata fail), `L0415` (write/create/remove fail).

## 13. Process / environment / OS (10)

**Interp only.**

| Builtin | Signature |
|---|---|
| `sys_status` | `sys_status(program string, args array<string>) -> i64` |
| `sys_output` | `sys_output(program string, args array<string>) -> string` |
| `env` | `env(name string) -> option<string>` |
| `args` | `args() -> list<string>` |
| `os_random` | `os_random(len i64) -> result<list<byte>, string>` (CSPRNG) |
| `proc_spawn` | `proc_spawn(cmd string, args array<string>) -> result<process, string>` |
| `proc_wait` | `proc_wait(p process) -> result<i64, string>` |
| `proc_stdout` | `proc_stdout(p process) -> result<string, string>` |
| `proc_stderr` | `proc_stderr(p process) -> result<string, string>` |
| `proc_kill` | `proc_kill(p process) -> result<i64, string>` |

Diagnostics: `L0332` (env/args), `L0335` (proc_* / socket family).

## 14. Time / clocks (3)

**Interp only.** `mono_now() -> i64` (monotonic ns), `wall_now() -> i64`
(Unix-epoch ms), `sleep_millis(ms i64) -> void`. Diagnostics `L0312`/`L0313`.

## 15. Concurrency — threads / channels / mutex (11)

**Interp only.**

`parallel_map(f fn(i64)->i64, args list<i64>) -> list<i64>`;
`chan_new() -> Chan`, `send(ch Chan, v i64) -> void`, `recv(ch Chan) -> i64`,
`try_recv(ch Chan) -> option<i64>`,
`spawn(f fn(Chan,i64)->void, ch Chan, v i64) -> Task`,
`task_join(t Task) -> void`;
`mutex_new(v i64) -> Mutex`, `mutex_get(m Mutex) -> i64`,
`mutex_set(m Mutex, v i64) -> void`, `mutex_add(m Mutex, delta i64) -> i64`.

Diagnostics: `L0334` (parallel_map), `L0337` (channel/mutex/task family).

## 16. Atomics (20)

**Interp only.** `atomic_i64` is a distinct shared, lock-free handle type.

Bare (seq_cst): `atomic_new(v i64) -> atomic_i64`, `atomic_load`, `atomic_store`,
`atomic_swap`, `atomic_cas`, `atomic_add`, `atomic_sub`, `atomic_and`,
`atomic_or`, `atomic_xor`.
Ordered: `atomic_load_ordered`, `atomic_store_ordered`, `atomic_swap_ordered`,
`atomic_add_ordered`, `atomic_sub_ordered`, `atomic_and_ordered`,
`atomic_or_ordered`, `atomic_xor_ordered`, `atomic_cas_ordered`, and the
standalone `fence(order MemoryOrder) -> void`.

`MemoryOrder` is a compiler-provided enum (`relaxed`/`acquire`/`release`/
`acq_rel`/`seq_cst`). Diagnostics: `L0337` (arg), `L0432` (illegal ordering).

## 17. Networking — TCP (10)

**Interp only.** Each fallible one returns `result<T, string>`. `Socket` is an
opaque handle.

`tcp_connect(host, port) -> result<Socket,string>`,
`tcp_listen(host, port) -> result<Socket,string>`,
`tcp_accept(l Socket) -> result<Socket,string>`,
`tcp_accept_nb(l Socket) -> result<option<Socket>,string>`,
`tcp_read(c Socket) -> result<string,string>`,
`tcp_read_nb(c Socket, max i64) -> result<option<string>,string>`,
`tcp_write(c Socket, data string) -> result<i64,string>`,
`tcp_shutdown(c Socket) -> void`, `tcp_close(c Socket) -> void`,
`set_nonblocking(s Socket, enabled bool) -> result<i64,string>`.

Diagnostic: `L0335`.

## 18. Networking — UDP (4)

**Interp only.** `udp_bind(host, port) -> result<Socket,string>`,
`udp_send_to(s Socket, data string, host string, port i64) -> result<i64,string>`,
`udp_recv(s Socket) -> result<string,string>`,
`udp_recv_nb(s Socket) -> result<option<string>,string>`. Diagnostic `L0335`.

## 19. HTTP client (2)

**Interp only.** `http_get(url string) -> result<string,string>`,
`http_post(url string, body string) -> result<string,string>`. `http` scheme
only (HTTPS → `err`). Diagnostic `L0336`.

## 20. Testing (1)

`assert(cond bool) -> void` — raises a catchable runtime error (`assertion
failed`) when false. Interp; native subset. Diagnostic `L0342`. (The
`lullaby test` runner and `test_*` functions are a CLI feature, not a builtin.)

---

## Category counts

| # | Category | Count | Dominant backend reach |
|---|---|---:|---|
| 1 | Memory / heap intrinsics | 4 | Interp + native subset |
| 2 | References / rc / ptr / layout | 15 | Interp + native subset |
| 3 | Conversion / formatting | 17 | Interp + native/WASM |
| 4 | Overflow arithmetic | 9 | Interp + native/WASM |
| 5 | Character classification | 6 | Interp + native subset |
| 6 | Collections — list / array | 20 | Interp (5 interp-only) |
| 7 | Collections — map | 8 | Interp (2 interp-only) |
| 8 | Strings | 21 | Interp + native (core in native/WASM) |
| 9 | Math | 19 | Interp (8 transcendentals interp-only) |
| 10 | Bitwise intrinsics | 6 | Interp + native subset |
| 11 | Streams / host I/O | 9 | Interp (stdin interp-only; 3 WASM host imports) |
| 12 | Filesystem | 14 | Interp only |
| 13 | Process / env / OS | 10 | Interp only |
| 14 | Time / clocks | 3 | Interp only |
| 15 | Concurrency (threads/chan/mutex) | 11 | Interp only |
| 16 | Atomics | 20 | Interp only |
| 17 | Networking — TCP | 10 | Interp only |
| 18 | Networking — UDP | 4 | Interp only |
| 19 | HTTP client | 2 | Interp only |
| 20 | Testing | 1 | Interp + native subset |
| | **Total** | **~209** | |

Headline: the three interpreters implement **100%** of the surface at parity; the
native/WASM backends cover the scalar + string + `list`/`map` + core-math +
memory core (categories 1–10 largely) and *whole-function-skip* everything OS-,
concurrency-, and network-shaped (categories 12–19) plus the higher-order
list/map iteration and transcendental math.

---

## Doc-vs-code mismatches

### M1. `array_fill` is implemented but missing from the canonical prelude catalog
`array_fill(n i64, value T) -> array<T>` is fully implemented — semantic signature
(`semantics_checker_calls.rs` ~288), runtime (`interpreter.rs` ~746), its own
runtime diagnostic `L0433`, and it is documented in
[language_surface.md](language_surface.md) and
[lullaby_type_system.md](lullaby_type_system.md). **But it is absent from
[standard_library.md](standard_library.md)**, which is billed as *the* prelude
catalog (its Collections table lists `list_new`…`map_values` but not
`array_fill`). This is the clearest "implemented but under-documented in the
canonical catalog" gap. **Fix:** add an `array_fill` row to the Collections
section of `standard_library.md` (out of scope for this catalog task — flagged
for a follow-up doc change).

### M2. `lullaby_input_output.md` documents a large aspirational surface as if it were the I/O story
The IO doc states up front (line 11) that "dotted `io.*` syntax, stream handles,
memory mapping, async, threads, sockets, and IPC remain planned design material
below," and only the small flat builtin list at the top is real. Everything after
that — `io.memory_map(...)`, `async def` / `await` / `await_all`,
`response.parse_json()`, dotted `io.*` calls, Python-style `def` — uses
**non-Lullaby syntax** (`def`, keyword args) and is **not implemented**. It is
labeled as planned, so this is not a contradiction, but the volume of unimplemented
Python-shaped pseudocode risks being read as current capability. **No code
change** — noted so B2 does not mistake the IO doc's lower half for shipped surface.
(Real, shipped async/sockets differ substantially from what that doc sketches: the
actual socket surface is the flat `tcp_*`/`udp_*` builtins in §17–18, and there is
no `async`/`await` in the language today.)

### M3. `standard_library.md` accurately covers the rest — no documented-but-absent builtins found
Every builtin named in `standard_library.md` (streams, fs, time, os_random,
process, concurrency, atomics + orderings, TCP/UDP/HTTP, memory/rc/ptr, strings,
math, bitwise, conversions, overflow ops, char classification, collections) was
found implemented in both the semantic registry and the runtime dispatch. Aside
from `array_fill` (M1), the flat catalog and the code agree. The prose does slightly
*under-claim* native/WASM string/collection coverage in places (it emphasizes the
interpreter tier), but that is conservative, not wrong.

### M4. Naming carve-outs are real and worth freezing deliberately
`task_join` (not `join`, which is the string-list joiner) and the shadowable
`words`/`count` (a user function of that name wins) are implemented exactly as
documented. These are load-bearing naming decisions B2 should ratify explicitly so
they cannot silently change.

---

## First-pass stable-vs-extended proposal (input to B2, NOT a freeze)

> **This is a proposal to inform the eventual B2 decision, not a final freeze.**
> It applies the B2 posture ("freeze a small stable core, version the rest as
> extended/experimental") to the inventory above. It is deliberately conservative:
> a name in **Stable core** is a hard 1.0 API promise, so only high-confidence,
> unlikely-to-change surface goes there. This axis (API-stability) is related to
> but distinct from the prelude-vs-module axis in
> [stdlib_modules_design.md](stdlib_modules_design.md); the two should be
> reconciled when B2 is decided.

### Tier S — Stable 1.0 core (freeze; strong compatibility promise)

High-confidence, universal, backend-agreed, unlikely to change:

- **Scalars & conversions (cat 3, 4):** `to_string`, `char_code`, `char_from`,
  `byte`, `byte_val`, the `to_i*`/`to_u*`/`to_isize`/`to_usize`/`to_i64`,
  `to_f32`/`to_f64`, and the `checked_*`/`saturating_*`/`wrapping_*` families
  (ratifies A4's wrapping-default decision).
- **Character classification (cat 5):** all six `is_*` predicates.
- **Collections core (cat 6, 7):** `len`, `array_fill`, `list_new`, `push`, `get`,
  `set`, `pop`, `reverse`, `concat`, `slice`, `sort`, `list_index_of`,
  `list_contains`, `list_sum`, `list_min`, `list_max`; `map_new`, `map_set`,
  `map_get`, `map_has`, `map_len`, `map_keys`, `map_values`, `map_del`.
- **Strings core (cat 8):** `substring`, `find`, `contains`, `starts_with`,
  `ends_with`, `repeat`, `split`, `join`, `trim`, `replace`, `upper`, `lower`,
  `chars`, `string_from_chars`, `to_bytes`, `from_bytes`, `byte_len`, `parse_i64`,
  `parse_f64`. *(These are "battery → `strings` module" candidates in
  stdlib_modules_design; if that move happens they stay Stable but become
  import-gated — a namespace change, not an API-stability change.)*
- **Core math & bitwise (cat 9, 10):** `abs`, `min`, `max`, `pow`, `clamp`,
  `sign`, `gcd`, `sqrt`, `floor`, `ceil`, `round`; all six bitwise intrinsics.
- **Core I/O (cat 11, 12, 13-partial, 20):** `print`, `println`, `warn`, `flush`;
  the full one-shot filesystem set; `env`, `args`, `sys_status`, `sys_output`;
  `assert`. These are irreducible OS intrinsics every non-trivial program relies on.
- **Error/absence model:** `option`/`result` + `ok`/`err`/`some`/`none` + `match`
  and `throw`/`try`/`catch` (control forms, catalogued for completeness).
- **Memory model core (cat 1, 2):** `alloc`/`load`/`store`/`dealloc`;
  `rc_*`/`ref_get`; `size_of`/`align_of`/`offset_of`. *(These realize the
  arena-first + RC memory model — freeze the surface even as the model matures.)*

### Tier E — Extended (ship in 1.0, but versioned as extended; may evolve)

Useful and implemented, but either design-uncertain, backend-partial, or plausibly
reshaped before a hard freeze:

- **Higher-order collection ops (cat 6):** `sort_by`, `list_map`, `list_filter`,
  `list_reduce` — interpreter-only today; their final shape depends on generics
  (A1) and native closure codegen (B1). Freezing them now would lock an API before
  the backend story is complete.
- **Transcendental math (cat 9):** `sin`/`cos`/`tan`/`atan`/`atan2`/`exp`/`ln`/
  `log10` — the "battery → `math` module" set; interpreter-only.
- **Stdin (cat 11):** `read_line`, `read_all`.
- **Time/clocks (cat 14):** `mono_now`, `wall_now`, `sleep_millis`.
- **OS randomness:** `os_random`.
- **Raw-pointer / unsafe intrinsics (cat 2):** `ptr_read`, `ptr_write`,
  `ptr_to_int`, `int_to_ptr`, `volatile_load`, `volatile_store` — tied to the
  still-maturing unsafe/freestanding tier.
- **Live process handles (cat 13):** `proc_spawn`/`proc_wait`/`proc_stdout`/
  `proc_stderr`/`proc_kill`.
- **Host-interop (cat 11):** `wasm_log`, `console_log`, `dom_set_text` — coupled to
  the WASM host-import ABI.

### Tier X — Experimental (ship, but explicitly unstable; expect change)

Design is actively in flux (see the in-flight design docs) and the current surface
is an early increment:

- **Concurrency (cat 15):** `parallel_map`, `chan_new`/`send`/`recv`/`try_recv`,
  `spawn`/`task_join`, `mutex_*`. The fixed `spawn(Chan, i64)` worker shape,
  `i64`-only channels, and no `select`/`async` are known limitations; the
  actor-model direction (`concurrency_model_design.md`) will reshape this.
- **Atomics (cat 16):** the full `atomic_*` + `fence` set. Only `atomic_i64`
  exists; other widths, `atomic_bool`, and weak CAS are deferred; passing atomics
  across threads awaits closures.
- **Networking (cat 17, 18, 19):** all `tcp_*`, `udp_*`, `http_get`/`http_post`.
  Per `stdlib_modules_design.md`, HTTP is slated to *leave* the prelude for a
  source `http` module; TCP/UDP stay primitive but their higher-level shape (a
  readiness selector, connection abstractions) is post-1.0. Networking is the
  archetypal "version as extended/experimental" surface for B2.

### Headline recommendation

Freeze a **Stable core of roughly 110–120 builtins** — scalars/conversions,
`list`/`map`/`string`/array core, core math + bitwise, basic file/stream/process
I/O, and the memory-model + error-handling surface — where the three interpreters
agree, the native/WASM backends already cover most of it, and the design is
settled. Ship **everything else in 1.0 but tag it Extended or Experimental**:
higher-order/closure-dependent collection ops and transcendental math (Extended,
pending A1/B1), and the entire concurrency / atomics / networking surface
(Experimental, pending the concurrency-model and networking-selector designs).
This matches the B2 posture precisely: a small, honest, promise-grade core plus a
versioned periphery that can evolve without breaking the 1.0 stability guarantee.
