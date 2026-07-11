# Cross-language function suite

A fixed set of functions implemented identically in **C, C++, Rust, Python, and
Lullaby** to measure two things Lullaby claims:

1. **Token efficiency** — Lullaby's terse, indentation-only syntax should cost an
   LLM meaningfully fewer tokens to generate for equivalent logic (the headline:
   *"close to C/C++ performance, ~half the tokens"*).
2. **Native performance** — Lullaby's `native` backend should run each function
   close to C/C++/Rust and far ahead of Python.

## Method

- **Scope:** every function is pure, deterministic, and expressible in all five
  languages using only `i64` integers, fixed-size integer arrays, and (where
  noted) ASCII strings/`bytes`. No I/O, allocation-heavy generics, or language-
  specific library sugar — the point is to compare *the same algorithm*.
- **Signatures are uniform** across languages (same parameters, same return), so
  the only differences are syntax and codegen.
- **Token count:** the function body (declaration through end), formatted in each
  language's idiomatic-but-minimal style, tokenized with the modern GPT tokenizer
  (`o200k_base`). Reported per function and aggregated. This is the marketability
  number.
- **Performance:** each function has a fixed benchmark driver input chosen to run
  ~0.1–1 s; the driver calls it in a measured loop (re-initializing mutable inputs
  each iteration) and prints a checksum. C/C++/Rust/Lullaby-native compiled at
  `-O2`/release; Python via CPython. Every language must print the **same
  checksum** (correctness gate) before its timing counts.
- **DoD (every function):** implemented in all 5 languages; identical checksum
  across all 5; token count recorded; where it has a numeric workload, timed in
  all 5 (Python may be skipped for the heaviest if it would run minutes — noted).

## Functions

### Tier 1 — trivial (a few lines)

| # | Name | Signature | Description | Acceptance criteria |
|---|---|---|---|---|
| 1 | `add` | `(i64,i64)->i64` | Sum two integers. | `add(2,3)=5`; wraps like two's-complement on overflow. |
| 2 | `max2` | `(i64,i64)->i64` | Larger of two. | `max2(3,7)=7`; `max2(5,5)=5`. |
| 3 | `abs_val` | `(i64)->i64` | Absolute value. | `abs_val(-4)=4`; `abs_val(0)=0`. |
| 4 | `is_even` | `(i64)->i64` | 1 if even else 0. | `is_even(4)=1`; `is_even(7)=0`; handles negatives. |
| 5 | `clamp` | `(i64,i64,i64)->i64` | Clamp x to [lo,hi]. | `clamp(5,0,3)=3`; `clamp(-1,0,3)=0`; `clamp(2,0,3)=2`. |
| 6 | `sign` | `(i64)->i64` | -1/0/1. | `sign(-9)=-1`; `sign(0)=0`; `sign(9)=1`. |

### Tier 2 — simple (loop or recursion, ~5–15 lines)

| # | Name | Signature | Description | Acceptance criteria |
|---|---|---|---|---|
| 7 | `factorial` | `(i64)->i64` | n! iteratively. | `factorial(5)=120`; `factorial(0)=1`. |
| 8 | `gcd` | `(i64,i64)->i64` | Euclid's GCD. | `gcd(48,18)=6`; `gcd(7,0)=7`. |
| 9 | `fib_iter` | `(i64)->i64` | nth Fibonacci, iterative. | `fib_iter(10)=55`; `fib_iter(0)=0`. |
| 10 | `is_prime` | `(i64)->i64` | 1 if prime. | `is_prime(1)=0`; `is_prime(2)=1`; `is_prime(97)=1`; `is_prime(100)=0`. |
| 11 | `int_pow` | `(i64,i64)->i64` | base^exp, exp≥0. | `int_pow(2,10)=1024`; `int_pow(5,0)=1`. |
| 12 | `count_digits` | `(i64)->i64` | # decimal digits of `abs(n)`. | `count_digits(0)=1`; `count_digits(-405)=3`. |
| 13 | `reverse_number` | `(i64)->i64` | Reverse the digits (keep sign). | `reverse_number(1230)=321`; `reverse_number(-45)=-54`. |
| 14 | `collatz_len` | `(i64)->i64` | Steps to reach 1. | `collatz_len(1)=0`; `collatz_len(6)=8`. |
| 15 | `sum_range` | `(i64)->i64` | Σ 0..n-1. | `sum_range(5)=10`; `sum_range(0)=0`. |
| 16 | `digit_sum` | `(i64)->i64` | Sum of decimal digits of `abs(n)`. | `digit_sum(1234)=10`; `digit_sum(-90)=9`. |

### Tier 3 — moderate (arrays/strings, ~15–40 lines)

| # | Name | Signature | Description | Acceptance criteria |
|---|---|---|---|---|
| 17 | `array_sum` | `(arr,len)->i64` | Sum of an i64 array. | Empty→0; `[1,2,3]→6`. |
| 18 | `array_max` | `(arr,len)->i64` | Max element (len≥1). | `[3,9,2]→9`; single→itself. |
| 19 | `linear_search` | `(arr,len,x)->i64` | First index of x, else -1. | found→index; absent→-1. |
| 20 | `binary_search` | `(sorted,len,x)->i64` | Index of x in sorted array, else -1. | found→index; absent→-1; empty→-1. |
| 21 | `reverse_array` | `(arr,len)->void` | Reverse in place. | `[1,2,3]→[3,2,1]`; checksum via sum-of-index*value. |
| 22 | `bubble_sort` | `(arr,len)->void` | Ascending in place. | sorts; stable not required; checksum = ordered dot-product. |
| 23 | `count_vowels` | `(bytes,len)->i64` | Count aeiou (ASCII, lower). | `"hello"→2`; `""→0`. |
| 24 | `is_palindrome` | `(bytes,len)->i64` | 1 if the byte slice reads the same both ways. | `"racecar"→1`; `"abc"→0`. |
| 25 | `caesar_shift` | `(bytes,len,k)->void` | Shift ASCII lowercase letters by k (mod 26), in place. | `"abc",1→"bcd"`; wraps z→a; non-letters unchanged. |
| 26 | `run_length` | `(bytes,len,out)->i64` | RLE: write `[char,count]` pairs to `out`, return pair count. | `"aaab"→(a,3)(b,1)`, returns 2. |
| 27 | `merge_sorted` | `(a,la,b,lb,out)->void` | Merge two ascending arrays into `out`. | classic merge; checksum = ordered dot-product. |

### Tier 4 — complex (algorithms, ~40–100 lines)

| # | Name | Signature | Description | Acceptance criteria |
|---|---|---|---|---|
| 28 | `quicksort` | `(arr,len)->void` | In-place quicksort (Lomuto/Hoare). | sorts; checksum = ordered dot-product. |
| 29 | `sieve_count` | `(n)->i64` | # primes < n via Sieve of Eratosthenes. | `sieve_count(10)=4`; `sieve_count(100)=25`. |
| 30 | `matrix_mul_trace` | `(a,b,n)->i64` | Multiply two n×n i64 matrices (row-major), return the trace of the product. | verified against a hand result for n=3. |
| 31 | `levenshtein` | `(a,la,b,lb)->i64` | Edit distance (DP table). | `("kitten","sitting")=3`; `("","abc")=3`. |
| 32 | `lcs_length` | `(a,la,b,lb)->i64` | Longest common subsequence length (DP). | `("ABCBDAB","BDCAB")=4`. |
| 33 | `knapsack01` | `(w,v,n,cap)->i64` | 0/1 knapsack max value (DP). | matches a hand result for a small fixed set. |
| 34 | `nqueens_count` | `(n)->i64` | # solutions to the n-queens problem (backtracking). | `nqueens_count(4)=2`; `nqueens_count(8)=92`. |
| 35 | `dot_product` | `(a,b,len)->i64` | Σ a[i]*b[i]. | classic; the SIMD/vectorization bellwether. |

## Deliverables

- `benchmarks/crosslang/{c,cpp,rust,python,lullaby}/` — one implementation set per
  language (a file per tier or per function group).
- `benchmarks/crosslang/tokens.py` — tokenizes each language's function bodies
  (`o200k_base`) and emits a per-function + aggregate token table.
- `benchmarks/crosslang/run_all.ps1` — compiles/runs each language's driver,
  checks the shared checksums, and records timings → `results/`.
- A summary table: **tokens per language** (the marketability headline) and
  **native vs C/C++/Rust/Python** per function.
