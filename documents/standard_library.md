# Lullaby Standard Library and Prelude

Canonical language rules: see [core_language_rules.md](core_language_rules.md).
Installable surface: see [alpha1_language_surface.md](alpha1_language_surface.md).

Everything in this document is **compiler-provided and always in scope** — this
is Lullaby's prelude. There is no `import` needed for any of it: the built-in
types and functions below are available in every module automatically. (User
modules and `import` cover *your own* multi-file code; see
[modules_design.md](modules_design.md).)

Signatures use the language's own spelling: `name param Type ... -> ReturnType`.

## Built-in types

| Type | Meaning |
|------|---------|
| `i64` | 64-bit signed integer (the default for integer literals) |
| `i8` / `i16` / `i32` | signed fixed-width integers (wrapping; from `to_i8`/`to_i16`/`to_i32`) |
| `u8` / `u16` / `u32` / `u64` | unsigned fixed-width integers (wrapping; from `to_u8`/`to_u16`/`to_u32`/`to_u64`) |
| `isize` / `usize` | pointer-sized integers (64-bit on current targets; from `to_isize`/`to_usize`) |
| `f64` | 64-bit IEEE-754 float (literals contain a `.`) |
| `f32` | 32-bit IEEE-754 float (rounds each op to single precision; from `to_f32`) |
| `bool` | `true` / `false` |
| `string` | UTF-8 text |
| `char` | a single Unicode scalar (literal `'a'`) |
| `byte` | an 8-bit unsigned integer, 0–255 |
| `void` | no value |
| `array<T>` | fixed homogeneous array; non-empty literal `[a, b, c]`; bounds-checked indexing |
| `list<T>` | growable list (functional; `l = push(l, x)`) |
| `map<K, V>` | hash map, keys `i64`/`string`; functional updates |
| `option<T>` | `some(value)` or `none` |
| `result<T, E>` | `ok(value)` or `err(error)` |
| `struct` | nominal record — `struct Name` with `field Type` lines |
| `enum` | nominal tagged union — `enum Name` with `Variant Type...` lines |
| `fn(T1, T2) -> R` | first-class function value |
| `rc<T>` / `ref<T>` / `ptr<T>` | reference-counted / borrowed / raw reference |

## Conversion and formatting

| Function | Signature | Notes |
|----------|-----------|-------|
| `to_string` | `to_string(x) -> string` | accepts any scalar: the numeric lattice (`i64`/`f64`/`i8`…`usize`/`f32`), `bool`, `string`, `char`, `byte` |
| `char_code` | `char_code(c char) -> i64` | Unicode scalar value |
| `char_from` | `char_from(i i64) -> char` | runtime error on an invalid scalar |
| `byte` | `byte(i i64) -> byte` | runtime error outside 0–255 |
| `byte_val` | `byte_val(b byte) -> i64` | |
| `to_i8` / `to_i16` / `to_i32` | `to_i<N>(x i64) -> i<N>` | wrapping reinterpret into signed width |
| `to_u8` / `to_u16` / `to_u32` / `to_u64` | `to_u<N>(x i64) -> u<N>` | wrapping reinterpret into unsigned width |
| `to_isize` / `to_usize` | `to_isize/to_usize(x i64) -> isize/usize` | pointer-sized (64-bit) reinterpret |
| `to_i64` | `to_i64(x) -> i64` | widen any fixed-width integer back to `i64` |
| `to_f32` | `to_f32(x f64) -> f32` | round an `f64` to single precision |
| `to_f64` | `to_f64(x f32) -> f64` | widen an `f32` to `f64` (exact) |

### Fixed-width integers

Alongside the default `i64`, Lullaby has the fixed-width integer types `i8`,
`i16`, `i32`, `u8`, `u16`, `u32`, `u64`, and the pointer-sized `isize`/`usize`
(64-bit on the current targets). `u8` is the 8-bit unsigned **arithmetic** type
(wrapping, like every width); it is distinct from `byte`, the raw-I/O octet
(the element type of `read_bytes`/`to_bytes`, constructed with `byte()`, which
*errors* outside 0–255 rather than wrapping). Bridge them with
`byte_val`/`byte` and `to_u8`/`to_i64`. There is **no implicit numeric coercion**: a
fixed-width integer never mixes with an `i64` or a different width in one
arithmetic or comparison expression (mixing widths is an `L0307` type error).
Move between widths explicitly with the `to_<T>` conversions (wrapping,
`i64 → T`) and `to_i64` (widening any width back to `i64`; the bit pattern is
reinterpreted, so a `u64` above `i64::MAX` reads as a negative `i64`).

Arithmetic (`+ - * /`) on a fixed-width integer **wraps** modulo the type width —
total and deterministic, never a trap. Division and comparison respect
signedness: the same bit pattern reads as a large magnitude for an unsigned type
but a negative value for a signed one — `to_u32(0 - 1)` is `4294967295` while
`to_i32(0 - 1)` is `-1`, and `to_u64(0 - 1) / to_u64(2)` divides on the unsigned
magnitude. Every backend normalizes at identical points, so results agree
bit-for-bit.

### 32-bit floats

`f32` is a single-precision float alongside the default `f64`. Like the integer
widths, it never mixes with `f64` in one expression without an explicit
conversion (`L0307`): `to_f32(x f64)` rounds to single precision and
`to_f64(x f32)` widens back exactly. Every `f32` operation is rounded to `f32`
precision, so it loses resolution an `f64` keeps — e.g. `2^24 + 1` rounds back
to `2^24` in `f32` but is exact in `f64`.

### Typed numeric-literal suffixes

A numeric literal can pin its type directly with a suffix instead of calling a
conversion: `100i32`, `4000000000u32`, `0xFFu16`, `120i8`, `2.5f32`. The `i64`
and `f64` suffixes are the defaults (a plain literal is already `i64`/`f64`).
The literal is **range-checked at parse time** — `256i8` and `100000u16` are
rejected — and a decimal point with an integer suffix (`1.5i32`) is an error. A
`u64`/`usize` literal is writable up to `i64::MAX`; larger (still valid) values
are built with `to_u64`. A suffix never applies to a base-prefixed float form:
`0xABF32` is the hex integer `0xABF32`, not `0xAB` with an `f32` suffix.

### Overflow-aware integer arithmetic

The `+ - *` operators on a fixed-width integer wrap by default. When wrapping is
the wrong behaviour, these builtins give explicit control over overflow. Each
takes two operands of the **same** fixed-width integer type `T` (not `i64`,
whose default arithmetic already traps on overflow):

| Family | Signature | On overflow |
|--------|-----------|-------------|
| `checked_add` / `checked_sub` / `checked_mul` | `(T, T) -> option<T>` | `none` |
| `saturating_add` / `saturating_sub` / `saturating_mul` | `(T, T) -> T` | clamps to `T`'s bounds |
| `wrapping_add` / `wrapping_sub` / `wrapping_mul` | `(T, T) -> T` | wraps modulo the width (explicit form of the default) |

For example, on `u32`: `checked_mul(to_u32(100000), to_u32(100000))` is `none`,
`saturating_mul(...)` is `4294967295`, and `wrapping_add(to_u32(4294967295), to_u32(1))`
is `0`. Every backend resolves overflow identically.

## Character classification

Deterministic `char -> bool` predicates for classifying a single Unicode scalar.
Each takes exactly one `char` argument and returns a `bool`; passing a non-`char`
argument is a compile-time `L0389` type error.

| Function | Signature | Notes |
|----------|-----------|-------|
| `is_digit` | `is_digit(c char) -> bool` | ASCII digit `0`–`9` |
| `is_alpha` | `is_alpha(c char) -> bool` | alphabetic (Unicode) |
| `is_alnum` | `is_alnum(c char) -> bool` | alphabetic or numeric (Unicode) |
| `is_whitespace` | `is_whitespace(c char) -> bool` | whitespace (Unicode) |
| `is_upper` | `is_upper(c char) -> bool` | uppercase (Unicode) |
| `is_lower` | `is_lower(c char) -> bool` | lowercase (Unicode) |

## Collections

| Function | Signature | Notes |
|----------|-----------|-------|
| `len` | `len(x) -> i64` | length of a `string`, `array<T>`, or `list<T>` |
| `list_new` | `list_new() -> list<T>` | element type inferred from context |
| `push` | `push(l list<T>, x T) -> list<T>` | append (returns a new list) |
| `get` | `get(l list<T>, i i64) -> T` | bounds-checked |
| `set` | `set(l list<T>, i i64, x T) -> list<T>` | bounds-checked (returns a new list) |
| `pop` | `pop(l list<T>) -> list<T>` | remove last (returns a new list) |
| `reverse` | `reverse(l list<T>) -> list<T>` | elements reversed (returns a new list) |
| `concat` | `concat(a list<T>, b list<T>) -> list<T>` | `b`'s elements appended to `a` (both lists must have the same element type; returns a new list) |
| `slice` | `slice(l list<T>, start i64, end i64) -> list<T>` | half-open range `[start, end)`; `start`/`end` are clamped into `[0, len]` and `start >= end` yields an empty list (returns a new list) |
| `sort` | `sort(l list<i64>) -> list<i64>` | elements sorted ascending (returns a new list; `list<i64>` only) |
| `list_index_of` | `list_index_of(l list<T>, x T) -> i64` | index of the first element equal to `x`, or `-1` if absent (`x` must match the element type `T`) |
| `list_contains` | `list_contains(l list<T>, x T) -> bool` | whether any element equals `x` (`x` must match the element type `T`) |
| `map_new` | `map_new() -> map<K, V>` | key/value types inferred from context |
| `map_set` | `map_set(m map<K, V>, k K, v V) -> map<K, V>` | insert/replace |
| `map_get` | `map_get(m map<K, V>, k K) -> option<V>` | `some`/`none` |
| `map_has` | `map_has(m map<K, V>, k K) -> bool` | |
| `map_len` | `map_len(m map<K, V>) -> i64` | |
| `map_del` | `map_del(m map<K, V>, k K) -> map<K, V>` | remove key |
| `map_keys` | `map_keys(m map<K, V>) -> list<K>` | keys in insertion order |
| `map_values` | `map_values(m map<K, V>) -> list<V>` | values in insertion order |

## Strings

`substring(s, start, end)`, `find(s, needle) -> i64` (`-1` if absent),
`contains(s, needle) -> bool`, `split(s, sep) -> array<string>`,
`join(parts array<string>, sep) -> string`, `trim(s) -> string`,
`replace(s, from, to) -> string`, `upper(s) -> string`, `lower(s) -> string`,
`starts_with(s, prefix) -> bool`, `ends_with(s, suffix) -> bool`,
`repeat(s, count i64) -> string` (`count <= 0` yields `""`).
Concatenate with `+` on two `string`s.

`chars(s string) -> list<char>` decomposes a string into its characters (Unicode
scalars) in order, and `string_from_chars(cs list<char>) -> string` recomposes
them (the inverse). Together with the `char` classification predicates
(`is_digit`/`is_alpha`/…) they let you write tokenizers and parsers in Lullaby.

### Bytes and UTF-8

- `to_bytes(s string) -> list<byte>` — the UTF-8 encoding of `s` as a list of
  `byte`s (the same `list<byte>` representation `read_bytes`/`write_bytes` use).
- `from_bytes(b list<byte>) -> result<string, string>` — decode the bytes as
  UTF-8, returning `ok(s)` on success and `err(message)` on invalid UTF-8. It
  never panics and never lossily replaces bad bytes — invalid input yields `err`,
  matched with `match` like any other `result`.
- `byte_len(s string) -> i64` — the number of UTF-8 bytes in `s`. This is
  distinct from `len`, which counts *characters* for a string, so `byte_len`
  exceeds `len` whenever `s` contains non-ASCII text (e.g. `byte_len("café")` is
  `5` while `len("café")` is `4`).

These three primitives are pure and deterministic, and produce identical results
(including the invalid-UTF-8 `err`) on the AST, IR, and bytecode backends. A
wrong argument type or arity reports the string-builtin family code `L0375`.

### Number parsing

- `parse_i64(s string) -> result<i64, string>` — parse `s` as a base-10 signed
  64-bit integer, returning `ok(n)` on success and `err(message)` on any failure
  (empty string, non-numeric text, or a value outside the `i64` range). It never
  panics. Leading/trailing whitespace is **not** trimmed — a padded string such
  as `" 42"` is an `err`; call `trim` first if you want to accept surrounding
  whitespace. An optional leading `+`/`-` sign is accepted.
- `parse_f64(s string) -> result<f64, string>` — parse `s` as an `f64`,
  returning `ok(x)` on success and `err(message)` on failure. It accepts the same
  forms Rust's float parser does, including `1.5`, `-2`, `1e3`, `inf`/`-inf`, and
  `nan`; whitespace is likewise not trimmed.

Both return a `result` unwrapped with `match` like any other, and the `err`
message is a fixed, backend-independent string (`` cannot parse `<s>` as i64 ``
or `` ... as f64 ``), so the `ok`/`err` outcome and the message text are
byte-for-byte identical on the AST, IR, and bytecode backends. A wrong argument
type or arity reports the string-builtin family code `L0375`.

## Math

`abs`, `min`, `max`, `pow` are type-directed over `i64` and `f64` (matching
operands); `sqrt`, `floor`, `ceil`, `round` take and return `f64`. Integer
`pow` requires a non-negative exponent.

The transcendental builtins take and return `f64`: `sin`, `cos`, `tan`, `atan`,
`exp` (e^x), `ln` (natural log), and `log10` are unary; `atan2(y, x)` takes two
`f64`s and returns the angle in radians. Undefined inputs follow platform `f64`
semantics (`NaN`/`inf`) and are bit-identical across the AST, IR, and bytecode
backends. A wrong argument type or arity reports `L0374` (semantic) or `L0417`
(runtime).

### Bitwise intrinsics

Six deterministic bit-manipulation builtins operate on `i64` (treating it as a
64-bit pattern) and return `i64`:

| Function | Signature | Notes |
|----------|-----------|-------|
| `rotate_left` | `rotate_left(x i64, n i64) -> i64` | rotate the 64 bits of `x` left by `(n & 63)` positions |
| `rotate_right` | `rotate_right(x i64, n i64) -> i64` | rotate the 64 bits of `x` right by `(n & 63)` positions |
| `count_ones` | `count_ones(x i64) -> i64` | population count (number of set bits), `0..=64` |
| `leading_zeros` | `leading_zeros(x i64) -> i64` | count of leading zero bits, `0..=64` |
| `trailing_zeros` | `trailing_zeros(x i64) -> i64` | count of trailing zero bits, `0..=64` |
| `reverse_bytes` | `reverse_bytes(x i64) -> i64` | byte swap (reverse the 8 bytes of `x`) |

The rotate builtins mask the shift amount with `& 63`, so any `n` — including
large or negative values — is total (e.g. `rotate_left(1, 68)` equals
`rotate_left(1, 4)`). For example `count_ones(255)` is `8`, `rotate_left(1, 4)`
is `16`, `trailing_zeros(16)` is `4`, `leading_zeros(1)` is `63`, and
`reverse_bytes(reverse_bytes(x))` round-trips back to `x`. These are pure and
deterministic and produce identical results on the AST, IR, and bytecode
backends. A wrong argument type or arity reports `L0374` (semantic) or `L0417`
(runtime).

## Standard streams and I/O

- Streams: `print(text)`, `println(text)`, `warn(text)` (stderr), `flush()` — each `-> void`.
- WebAssembly host log: `wasm_log(x i64) -> void`. On the interpreters it prints
  the value as a stdout line; on the WebAssembly backend (`lullaby wasm`) it
  lowers to a call of the imported host function `env.log_i64`, letting an
  eligible exported function report values to its host. Because it is understood
  by the WASM backend, a function that calls only `wasm_log` (and the scalar
  subset) still compiles to `.wasm`.
- JS/DOM host interop (the browser-host layer, built on the same import
  mechanism as `wasm_log`):
  - `console_log(s string) -> void` — on the interpreters it prints the string as
    a stdout line; on the WebAssembly backend it lowers to a call of the imported
    host function `env.console_log(ptr i32, len i32)`, passing the string's
    linear-memory pointer and length. A browser host implements it as
    `console.log`.
  - `dom_set_text(id string, text string) -> void` — on the interpreters it
    prints the deterministic line `id=text`; on the WebAssembly backend it lowers
    to a call of the imported host function
    `env.dom_set_text(id_ptr i32, id_len i32, text_ptr i32, text_len i32)`. A
    browser host implements it as
    `document.getElementById(id).textContent = text`.
  - Both are understood by the WASM backend, so a function that calls them (over
    the scalar/heap subset) still compiles to `.wasm`. The host decodes each
    string out of `memory` from the `[len i32][utf8 bytes]` layout at the pointer;
    the length passed is the header count (equal to the byte length for ASCII).
- Text files: `read_file(path) -> string`, `write_file(path, content) -> void`,
  `append_file(path, content) -> void`, `file_exists(path) -> bool`.
- Lines and bytes: `read_lines(path) -> list<string>` (lines with no trailing
  newline per element), `read_bytes(path) -> list<byte>`,
  `write_bytes(path, data list<byte>) -> void` (truncating), `file_size(path) -> i64`.
- Filesystem queries and directories: `is_file(path) -> bool`,
  `is_dir(path) -> bool`, `list_dir(path) -> list<string>` (entry names, not full
  paths), `make_dir(path) -> void` (creates parent directories),
  `remove_file(path) -> void`, `remove_dir(path) -> void` (empty directory only).
- These file-system builtins are one-shot; stateful file handles, seeking, and
  buffered streams are deferred. Wrong argument types or arities report `L0333`;
  a failed read/metadata query reports `L0414`; a failed write/create/remove
  reports `L0415`.
- System commands: `sys_status(program, args array<string>) -> i64`,
  `sys_output(program, args array<string>) -> string` (no shell).
- Time and clocks:
  - `mono_now() -> i64` — a **monotonic** clock reading in **nanoseconds** since a
    fixed per-process baseline (established on first use). It is guaranteed
    non-decreasing within a run and is unaffected by wall-clock adjustments, so it
    is the correct choice for measuring elapsed durations. The absolute value is
    meaningless; only differences between two readings are.
  - `wall_now() -> i64` — **wall-clock** time as **milliseconds since the Unix
    epoch** (1970-01-01 UTC). Use it for timestamps and calendar-facing values;
    do not use it to measure elapsed time, since the system clock can jump.
  - `sleep_millis(ms i64) -> void` — sleep the current thread for `ms`
    milliseconds. A negative `ms` is treated as `0` (no sleep, no error).
  - These are interpreter/runtime builtins; the native and WebAssembly backends
    are subsets that do not provide them. Wrong argument types or arities are
    compile-time semantic diagnostics (`L0312`/`L0313`).
- OS randomness:
  - `os_random(len i64) -> result<list<byte>, string>` — returns `len`
    **cryptographically-secure random bytes** drawn directly from the operating
    system's CSPRNG as `ok(list<byte>)`, or `err(message)` if the OS RNG fails.
    This is a **real OS randomness source** (`getrandom`/`getentropy` on
    Unix-likes, `BCryptGenRandom` on Windows, `/dev/urandom` as a fallback) — it
    is **never** a seeded or deterministic PRNG, so it is suitable for keys,
    nonces, tokens, and salts.
  - `len == 0` returns `ok([])` (an empty list, no syscall). `len < 0` returns
    `err("os_random length must be non-negative")` — it never panics.
  - The returned bytes are non-deterministic, so results differ from run to run
    and between two calls; only structural facts (such as the byte count) are
    reproducible across the AST, IR, and bytecode backends.
  - This is an interpreter/runtime builtin; the native and WebAssembly backends
    are subsets that do not provide it. Wrong argument types or arities are
    compile-time semantic diagnostics (`L0312`/`L0313`).

## Process and environment

- `env(name string) -> option<string>` reads an environment variable, returning
  `some(value)` when set and `none` otherwise.
- `args() -> list<string>` returns the running program's CLI arguments (an empty
  list when none were passed). `lullaby run <file.lby> [args...]` supplies the
  trailing tokens after the source path as those program arguments.
- Wrong argument types or arities to `env`/`args` report `L0332`.

### Live external processes

`sys_status`/`sys_output` above are **one-shot**: they run a command to
completion and hand back just the exit code or captured stdout. The `process`
handle extends that with a **live** child you can spawn, wait on, read both
streams from, and kill. A `process` is an opaque handle to an OS child process,
backed by a per-runtime handle table exactly like a `Socket`. Every process
builtin is fallible and returns a `result<T, string>`, so failures are ordinary
runtime values matched with `match` (`err(message)`) rather than panics.
Processes run identically on the AST, IR, and bytecode backends.

- `proc_spawn(cmd string, args array<string>) -> result<process, string>` —
  spawn `cmd` with `args`, capturing stdout and stderr through pipes. `ok(handle)`
  on success; `err(message)` if the process cannot be started (e.g. the command
  is not found). No shell is invoked. The `args` array must be non-empty in the
  current alpha (array literals require at least one element); pass a placeholder
  argument when a command takes none.
- `proc_wait(p process) -> result<i64, string>` — block until the child exits and
  return its exit code as an `i64`. On Unix a child terminated by a signal has no
  exit code; by convention that is reported as `128 + signal` (the shell
  convention), so the result is always a total `i64`. `err` if the handle is
  invalid or the wait fails.
- `proc_stdout(p process) -> result<string, string>` — read the child's captured
  stdout to end as a UTF-8 string. Call after `proc_wait`, or it returns whatever
  is available up to EOF. The pipe is drained on the first read, so a second call
  returns an empty string.
- `proc_stderr(p process) -> result<string, string>` — like `proc_stdout`, for
  the child's captured stderr.
- `proc_kill(p process) -> result<i64, string>` — kill the child; `ok(0)` on
  success (killing an already-exited child still succeeds).
- Wrong argument types or arities to the `proc_*` builtins report `L0335` (the
  shared socket/network handle diagnostic family).

## Concurrency

- `parallel_map(f fn(i64) -> i64, args list<i64>) -> list<i64>` evaluates
  `f(arg)` for every element of `args` concurrently on separate OS threads and
  returns the results in the **same order as `args`** (deterministic regardless
  of scheduling). Each thread runs a fresh interpreter over the shared program,
  so heaps are per-thread with no shared mutable state; `f` must be an ordinary
  top-level `fn(i64) -> i64`.
- Wrong arity, a non-`fn(i64) -> i64` first argument, or a non-`list<i64>`
  second argument report `L0334`.

Detached threads, channels, and a shared mutex (message passing):

- `chan_new() -> Chan` creates an unbounded `i64` channel. A `Chan` is a shared
  handle: cloning the value shares the same underlying queue.
- `send(ch Chan, v i64) -> void` enqueues a value (never blocks).
- `recv(ch Chan) -> i64` dequeues, blocking until a value is available.
- `try_recv(ch Chan) -> option<i64>` dequeues without blocking (`some(v)`/`none`).
- `spawn(f fn(Chan, i64) -> void, ch Chan, v i64) -> Task` runs `f(ch, v)` on a
  detached OS thread and returns a `Task` handle. The argument shape is fixed to
  `(Chan, i64)` in this increment.
- `task_join(t Task) -> void` waits for a spawned thread (a second `task_join` is
  a no-op). It is named `task_join`, not `join`, because `join` is the
  string-list joiner.
- `mutex_new(v i64) -> Mutex` creates a shared mutex over one `i64` (shared on
  clone); `mutex_get(m Mutex) -> i64` reads, `mutex_set(m Mutex, v i64) -> void`
  writes, and `mutex_add(m Mutex, delta i64) -> i64` atomically adds `delta` and
  returns the new value.
- Wrong arity or a wrong-typed argument to any of these reports `L0337`.
- Generic channels, `select`, `async`/`await`, passing a `Mutex`/`Socket` into a
  worker, and cross-thread socket sharing remain deferred; see
  [concurrency_design.md](concurrency_design.md).

Lock-free shared atomics:

- `atomic_i64` is a shared, lock-free `i64` cell — a distinct nominal handle
  type (not the same as `i64`; you cannot do arithmetic on it directly). Like
  `Chan`/`Mutex` it has reference semantics: cloning the value shares the same
  underlying cell, so two threads holding copies see each other's writes. It is
  backed by `std::sync::atomic::AtomicI64`, so cross-thread updates are wait-free
  with no lost increments. Every operation below uses **sequentially consistent
  (`SeqCst`) ordering** — the always-correct default; weaker orderings
  (`relaxed`/`acquire`/`release`/`acqrel`) are a documented future optimization
  and are not part of this increment (see
  [concurrency_design.md](concurrency_design.md)).
- `atomic_new(v i64) -> atomic_i64` allocates a fresh atomic cell initialized to
  `v`.
- `atomic_load(a atomic_i64) -> i64` reads the current value.
- `atomic_store(a atomic_i64, v i64) -> void` overwrites the value.
- `atomic_swap(a atomic_i64, v i64) -> i64` stores `v` and returns the previous
  value.
- `atomic_cas(a atomic_i64, expected i64, new i64) -> i64` is a strong
  compare-and-swap: if the cell equals `expected` it stores `new`; either way it
  returns the value that was in the cell (so `returned == expected` means it
  succeeded). Lullaby has no out-parameters, so the observed value is returned
  rather than a `bool`.
- `atomic_add`, `atomic_sub`, `atomic_and`, `atomic_or`, and `atomic_xor`, each
  `(a atomic_i64, v i64) -> i64`, are fetch-and-op: they atomically apply the
  operation to the cell and return the **previous** value (the new value is a
  local op away). `add`/`sub` wrap on overflow.
- Wrong arity or a wrong-typed argument (a non-`atomic_i64` handle, or a
  non-`i64` operand) to any atomic builtin reports `L0337`.
- Atomics run identically on the AST, IR, and bytecode backends. Because the
  fixed `spawn(Chan, i64)` worker shape cannot yet hand an atomic to a Lullaby
  worker, cross-thread atomicity is exercised through runtime tests that share
  one cell across OS threads (proving no lost updates); passing an atomic into a
  worker waits on capturing closures, like `Mutex`. Weaker memory orderings,
  fences, the other atomic widths (`atomic_i32`/`atomic_u64`/…), `atomic_bool`,
  and weak CAS remain deferred; see
  [concurrency_design.md](concurrency_design.md).

## Networking

A `Socket` is an opaque handle to an OS network resource (TCP listener, TCP
stream, or UDP socket), backed by a per-runtime handle table (like a heap
pointer). Every fallible socket builtin returns a `result<T, string>`, so
network errors are ordinary runtime values matched with `match` (`err(message)`
on failure) rather than panics. Sockets run identically on the AST, IR, and
bytecode backends.

- TCP:
  - `tcp_connect(host string, port i64) -> result<Socket, string>` — open a
    client stream.
  - `tcp_listen(host string, port i64) -> result<Socket, string>` — bind and
    listen.
  - `tcp_accept(listener Socket) -> result<Socket, string>` — block for a
    connection and return a stream `Socket`.
  - `tcp_read(conn Socket) -> result<string, string>` — read up to 4096 bytes as
    a UTF-8 string (empty string on clean EOF).
  - `tcp_write(conn Socket, data string) -> result<i64, string>` — write the
    string's full byte buffer (short writes are retried) and flush; return the
    byte count.
  - `tcp_shutdown(conn Socket) -> void` — gracefully shut down the write half of
    the connection (signal EOF to the peer) so a buffered response is delivered
    before the socket is dropped. The graceful teardown order for a server is
    `tcp_write` then `tcp_shutdown` then `tcp_close`.
  - `tcp_close(conn Socket) -> void` — free the handle.
- UDP:
  - `udp_bind(host string, port i64) -> result<Socket, string>` — bind a datagram
    socket.
  - `udp_send_to(sock Socket, data string, host string, port i64) -> result<i64, string>`
    — send one datagram; return the byte count.
  - `udp_recv(sock Socket) -> result<string, string>` — receive one datagram as a
    string (the sender address is dropped in this increment).
- Wrong argument types or arities report `L0335`.
- HTTP/1.1 client (built over `TcpStream`, no TLS):
  - `http_get(url string) -> result<string, string>` — perform an HTTP/1.1 GET
    and return the response body on a 2xx/3xx response, or `err(message)` on a
    connection/parse/HTTP error.
  - `http_post(url string, body string) -> result<string, string>` — HTTP/1.1
    POST with a `text/plain` request body (correct `Content-Length`); return the
    response body or `err(message)`.
  - Only the `http` scheme is supported; an HTTPS URL returns
    `err("https not supported")`. Chunked transfer decoding is not implemented —
    responses are read to EOF via `Connection: close`. A 4xx/5xx status returns
    `err("http {code}: {first-body-line}")`. A 10-second read timeout surfaces a
    hung server as `err`. Wrong argument types or arities report `L0336`.
- A complete HTTP/1.1 **server** can be written in pure Lullaby on these socket
  builtins — request-line parsing (`split`), path routing, and response building
  are ordinary `pub` functions, with the per-connection teardown
  `tcp_write` then `tcp_shutdown` then `tcp_close`. See
  `examples/valid/http_server/` (`server.lby` plus the reusable `http.lby`
  module).

## Memory and references

- Heap: `alloc(value)`, `load(ptr)`, `store(ptr, value)`, `dealloc(ptr)`.
- Reference counting: `rc_new(value)`, `rc_clone(rc<T>)`, `rc_release(rc<T>)`,
  `rc_get(rc<T>)`, `rc_borrow(rc<T>) -> ref<T>`, `ref_get(ref<T>)`.
- Raw pointers (inside `unsafe`): `ptr_read(ptr<T>)`, `ptr_write(ptr<T>, value)`.

## Error handling

- `throw EXPR` raises a `string` error; `try` / `catch NAME` recovers.
- `result<T, E>` with `ok`/`err` plus `match` is the value-based alternative;
  `option<T>` (`some`/`none`) models absence.

## Testing

- `assert(cond bool) -> void` raises a catchable runtime error with the message
  `assertion failed` (the same error path `throw` uses, so `try`/`catch` recovers
  it) when `cond` is false, and returns `void` when true. A non-`bool` argument is
  a type error (`L0342`).
- `lullaby test <file.lby>` is the test runner: it validates the file as a library
  (no `main` needed) and runs every zero-parameter `test_*` function through the
  interpreter, reporting `PASS`/`FAIL` per test plus a `N passed, M failed`
  summary and a non-zero exit when any test fails. A test fails if it produces a
  runtime error, so `assert` is the natural way to write test bodies.

## Notes

- This prelude is a documentation artifact over the compiler's built-in surface;
  it has no separate package and needs no import. As native code generation and
  packaging land, parts of it (e.g. collections, string/math helpers) may move
  from compiler intrinsics into a shipped, importable standard-library package,
  documented alongside the package manifest.
