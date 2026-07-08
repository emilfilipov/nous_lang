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
| `i64` | 64-bit signed integer |
| `f64` | 64-bit IEEE-754 float (literals contain a `.`) |
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
| `to_string` | `to_string(x) -> string` | accepts `i64`, `f64`, `bool`, `string`, `char`, `byte` |
| `char_code` | `char_code(c char) -> i64` | Unicode scalar value |
| `char_from` | `char_from(i i64) -> char` | runtime error on an invalid scalar |
| `byte` | `byte(i i64) -> byte` | runtime error outside 0–255 |
| `byte_val` | `byte_val(b byte) -> i64` | |

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
| `map_new` | `map_new() -> map<K, V>` | key/value types inferred from context |
| `map_set` | `map_set(m map<K, V>, k K, v V) -> map<K, V>` | insert/replace |
| `map_get` | `map_get(m map<K, V>, k K) -> option<V>` | `some`/`none` |
| `map_has` | `map_has(m map<K, V>, k K) -> bool` | |
| `map_len` | `map_len(m map<K, V>) -> i64` | |
| `map_del` | `map_del(m map<K, V>, k K) -> map<K, V>` | remove key |

## Strings

`substring(s, start, end)`, `find(s, needle) -> i64` (`-1` if absent),
`contains(s, needle) -> bool`, `split(s, sep) -> array<string>`,
`join(parts array<string>, sep) -> string`, `trim(s) -> string`,
`replace(s, from, to) -> string`, `upper(s) -> string`, `lower(s) -> string`,
`starts_with(s, prefix) -> bool`, `ends_with(s, suffix) -> bool`,
`repeat(s, count i64) -> string` (`count <= 0` yields `""`).
Concatenate with `+` on two `string`s.

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
