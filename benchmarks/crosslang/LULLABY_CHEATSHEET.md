# Lullaby cheat sheet (for writing benchmark corpus code)

Everything you need to write **valid, idiomatic Lullaby** for the cross-language
corpus. Lullaby is indentation-scoped (no braces, no semicolons). **Always verify
your `.lby` compiles** with `target/release/lullaby.exe check <file.lby>` before
finishing — fix every diagnostic.

## Functions

```
fn name p1 i64 p2 string -> i64        # params are `name type` pairs, space-separated
    let x i64 = p1 + 1                  # `let name type = expr`
    x                                   # last expression is the return value

fn add a i64 b i64                     # `-> T` is OPTIONAL: the return type is inferred
    a + b                              #   from the body. Prefer omitting it (fewer tokens).
```
Early return: `return expr`. Omit `-> T` to infer it (a recursive function must
still state `-> T`, else `L0439`). Void functions may omit it too.
Call: `name(a, b)`. There is **no** function overloading.

## Types

`i64` (the default integer), `f64`, `bool`, `char`, `string`,
`array<T>` (fixed, indexable, value-copied), `list<T>` (growable, **NOT
indexable** — use `array` when you need `xs[i]`), `map<K, V>`, plus your `struct`
and `enum` types. `option<T>` and `result<T>` are built in.

## Control flow

```
if cond
    ...
elif cond2
    ...
else
    ...

while cond
    ...

for i from 0 to n            # INCLUSIVE both ends; `for i from 0 to n-1` visits n items.
    ...                      # empty when start > end. `break` / `continue` work.

let s = "even" if n % 2 == 0 else "odd"   # inline conditional (ternary): THEN if COND else ELSE
return 1 if ok else 0                     # great for 1/0 returns — replaces a 4-line if/else block
let g = 1 if s >= 90 else 2 if s >= 80 else 3   # right-associative else-chain (no parens needed)

if c in "aeiou"              # membership: char/substring in a string, or element in a list<T>
    ...                      # `x in xs` on a list<T>; yields bool. Replaces contains(...) / long `or` chains.

let head = s[0:3]            # string slice s[start:end] (half-open); s[i:] to end, s[:j] from 0, s[:] whole.
```

Prefer `s[i:j]` over `substring(s, i, j)` (fewer tokens). Bounds are `i64`;
either may be omitted. Slicing is `string`-only (there is no array slice).

**Prefer the inline conditional over a block `if/else` when returning a value.**
`return 1 if cond else 0` is one line and far fewer tokens than the `if cond` /
`1` / `else` / `0` block. Condition must be `bool`; both branches must share a
type; the result must be a scalar or `string` (use a block `if` for aggregates).

## Operators & gotchas (IMPORTANT)

- Arithmetic `+ - * /` (integer `/` truncates toward zero, like C). **There is NO
  `%` operator** — write a helper `fn rem a i64 b i64 -> i64 \n    a - (a / b) * b`
  and call `rem(a, b)`. (This is a real language gap — note it, don't work around
  it silently.)
- Comparison `== != < <= > >=`; boolean `and or not`.
- `+=` `-=` `*=` `/=` compound assignment on a bound variable.
- No `++`/`--`. Inline conditional `A if C else B` IS available (see Control flow) — prefer it over a block `if/else` for value selection.
- String concatenation is `+`. A `char` concatenates onto a string directly —
  `s + c` and `s += c` work (the char becomes a one-char string); numbers still
  need `to_string(x)`. Interpolation `"n=${x} sq=${x * x}"` embeds any expression
  (sugar for `+`/`to_string`; `lullaby fmt` expands it back to concatenation).
- **String literals do NOT interpret escapes** (`"\n"` is backslash-n, not a
  newline). For control chars use `char_from(code)` (10 = LF, 13 = CR, 9 = tab);
  a `char` appends to a string directly (`s += char_from(10)`), no `to_string`.
- Empty array literal `[]` is rejected — an array literal needs ≥1 element.
- Params are mutable locals (you may reassign `n = n - 1`).

## Structs

```
struct Point
    x i64
    y i64

fn make -> Point
    Point(3, 4)                 # positional construction, field order

fn dx a Point b Point -> i64
    a.x - b.x                   # dotted field access
```

## Enums + match (this is where Lullaby is terse)

```
enum Shape
    Circle i64                  # variant with a payload
    Square i64
    Empty                       # no payload

fn area s Shape -> i64
    match s
        Circle(r) -> 3 * r * r
        Square(w) -> w * w
        Empty -> 0
```
Built-in `option<T>`: variants `some(x)` / `none`. Built-in `result<T>`:
`ok(x)` / `err(msg)`. Match them the same way:
```
match map_get(m, k)
    some(v) -> v
    none -> 0
```

## Collections & strings (builtins)

- Arrays: `let a array<i64> = [1, 2, 3]`, read `a[i]`, write `a[i] = v`, `len(a)`.
- Maps (functional — `map_set` returns a new map): `map_new()`,
  `map_set(m, k, v)`, `map_get(m, k) -> option`, `map_has(m, k)`, `map_len(m)`.
- Lists (growable, not indexable): `push(xs, v)`, `len(xs)`; iterate via builtins,
  not `xs[i]`.
- Strings: `len(s)`, `substring(s, start, end)`, `slice(...)`, `contains(s, sub)`,
  `starts_with(s, p)`, `ends_with(s, p)`, `split(s, sep) -> list<string>`,
  `join(parts, sep)`, `trim(s)`, `replace(s, from, to)`, `char_from(code)`,
  `to_string(x)`. There is no numeric-parse builtin — parse digits by hand if
  needed.
- I/O: `println(s)`, `print(s)`, `read_file(path)`, `write_file(path, s)`,
  `read_lines(path)`. TCP sockets exist (see `examples/valid/http_server/`).

## Reference implementations to imitate

- `benchmarks/crosslang/lullaby/scalar.lby` — 16 numeric functions.
- `examples/valid/points.lby` — struct + field access.
- `examples/valid/traffic_light.lby` — enum + match.
- `examples/valid/inventory.lby` — map + option/match + strings.
- `examples/valid/http_server/http.lby` — request parsing, routing, response
  building (real-world service logic on sockets).

Keep code **idiomatic and minimal** (natural, not code-golfed, not padded) — this
is a fair token comparison, so write how a competent Lullaby dev naturally would.
