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
`replace(s, from, to) -> string`, `upper(s) -> string`, `lower(s) -> string`.
Concatenate with `+` on two `string`s.

## Math

`abs`, `min`, `max`, `pow` are type-directed over `i64` and `f64` (matching
operands); `sqrt`, `floor`, `ceil`, `round` take and return `f64`. Integer
`pow` requires a non-negative exponent.

## Standard streams and I/O

- Streams: `print(text)`, `println(text)`, `warn(text)` (stderr), `flush()` — each `-> void`.
- Text files: `read_file(path) -> string`, `write_file(path, content) -> void`,
  `append_file(path, content) -> void`, `file_exists(path) -> bool`.
- System commands: `sys_status(program, args array<string>) -> i64`,
  `sys_output(program, args array<string>) -> string` (no shell).

## Memory and references

- Heap: `alloc(value)`, `load(ptr)`, `store(ptr, value)`, `dealloc(ptr)`.
- Reference counting: `rc_new(value)`, `rc_clone(rc<T>)`, `rc_release(rc<T>)`,
  `rc_get(rc<T>)`, `rc_borrow(rc<T>) -> ref<T>`, `ref_get(ref<T>)`.
- Raw pointers (inside `unsafe`): `ptr_read(ptr<T>)`, `ptr_write(ptr<T>, value)`.

## Error handling

- `throw EXPR` raises a `string` error; `try` / `catch NAME` recovers.
- `result<T, E>` with `ok`/`err` plus `match` is the value-based alternative;
  `option<T>` (`some`/`none`) models absence.

## Notes

- This prelude is a documentation artifact over the compiler's built-in surface;
  it has no separate package and needs no import. As native code generation and
  packaging land, parts of it (e.g. collections, string/math helpers) may move
  from compiler intrinsics into a shipped, importable standard-library package,
  documented alongside the package manifest.
