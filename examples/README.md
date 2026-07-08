# Lullaby Examples

These examples are intended for users of the packaged `lullaby` tool.

- `valid/`: programs that should pass `lullaby check` and `lullaby run`.
- `invalid/`: small programs that intentionally fail so diagnostic output can be inspected.

From the repository root:

```powershell
cargo run -p lullaby_cli -- run examples/valid/calculator.lby
cargo run -p lullaby_cli -- check examples/invalid/type_mismatch.lby
```

From the portable package root:

```powershell
.\bin\lullaby.exe run .\examples\valid\calculator.lby
.\bin\lullaby.exe check .\examples\invalid\type_mismatch.lby
```

## Selected examples

- `valid/primes.lby`: counts the prime numbers below 50 with trial division
  (defines `rem` and `is_prime` helpers, a `while` loop, and early `return`).
- `valid/collatz.lby`: prints Collatz stopping times for a few integers, using
  only integer arithmetic and an even/odd `rem` check.
- `valid/traffic_light.lby`: a small state machine over an `enum` of lights,
  with a `match`-based `next` transition and `name` renderer stepped through
  several cycles.
- `valid/word_count.lby`: splits a sentence with `split`, counts the words, and
  derives letter and longest-word stats with a loop.
- `valid/inventory.lby`: a `map<string, i64>` stock ledger that adjusts a count
  and looks items up via `match` on the `option` returned by `map_get`.
- `valid/permissions.lby`: Unix-style permission bits built with the `i64`
  bitwise operators — shifts to define flags, `|` to combine, `&` to test,
  `& ~flag` to clear, and `^` to toggle, rendered as an `rwx` string.
- `valid/bits.lby`: bit tricks with the bitwise operators — population count,
  power-of-two test (`n & (n - 1)`), and byte extraction (`(x >> 8) & 255`).
- `valid/utf8_bytes.lby`: encodes strings to UTF-8 bytes with `to_bytes`,
  round-trips them back with `from_bytes` (matching on the `result`), and shows
  the char-count vs `byte_len` distinction for non-ASCII text.
- `valid/stopwatch.lby`: times a small computation with the monotonic clock
  (`mono_now`), sleeps with `sleep_millis`, and reads the wall clock
  (`wall_now`).
- `valid/random_bytes.lby`: draws cryptographically-secure random bytes from the
  OS with `os_random`, matching on the `result` and inspecting the byte count
  and first value.
- `invalid/int_float_mismatch.lby`: mixes `i64` and `f64` in one expression
  (`let x i64 = 1 + 2.0`); `lullaby check` reports diagnostic `L0307` (operands
  of `+` must share a type) and exits non-zero.
