#!/usr/bin/env python3
"""Turn a corpus category's lullaby.lby (a single-shot correctness driver) into a
HOT compute driver: keep every function definition, keep main's setup statements,
and wrap the work (the `println(...)` calls) in a repeat loop, folding each
printed string's length into an accumulator (no I/O). This exercises the
category's functions N times so the interpreters/native do real, timeable compute.

  python gen_perf_driver.py <category/lullaby.lby> <N> <out.lby> [native]

With the optional `native` mode, the folded value is the i64 result of each
`to_string(EXPR)` call FOLDED DIRECTLY (`acc += EXPR`, no strings), so a
purely-i64-scalar category's `main` stays native-eligible and can be timed on the
native backend. Categories that build/return heap strings won't type-check in this
mode (the caller detects that and skips native for them) — a real language
boundary, not a harness limit.
"""
import re
import sys


def extract_to_string_arg(text: str) -> str | None:
    """Return the balanced-paren argument of the first `to_string(...)` in `text`."""
    i = text.find("to_string(")
    if i < 0:
        return None
    j = i + len("to_string(")
    depth, start = 1, j
    while j < len(text):
        if text[j] == "(":
            depth += 1
        elif text[j] == ")":
            depth -= 1
            if depth == 0:
                return text[start:j]
        j += 1
    return None


def main() -> int:
    path, n, out_path = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    native = len(sys.argv) > 4 and sys.argv[4] == "native"
    lines = open(path, encoding="utf-8").read().split("\n")

    # Find `fn main` and split the file into [pre-main] and [main body lines].
    main_idx = next((i for i, l in enumerate(lines)
                     if re.match(r"^fn main\b", l)), None)
    if main_idx is None:
        sys.stderr.write("no fn main\n")
        return 1

    pre = lines[:main_idx]
    body = []
    for l in lines[main_idx + 1:]:
        if l and not l.startswith((" ", "\t")) and l.strip():
            break  # next top-level item (shouldn't happen; main is usually last)
        body.append(l)

    setup, work = [], []
    for l in body:
        s = l.strip()
        if not s:
            continue
        m = re.match(r"println\((.*)\)$", s)
        if m:
            work.append(m.group(1))          # the string argument
        else:
            setup.append("    " + s)         # e.g. `let g array<i64> = [...]`

    out = list(pre)
    out.append("fn main -> i64")
    out.extend(setup)
    out.append("    let __acc = 0")
    out.append(f"    for __r from 1 to {n}")
    for arg in work:
        if native:
            # Fold the i64 result of each `to_string(EXPR)` directly (no strings),
            # so a purely-i64-scalar category stays native-eligible.
            expr = extract_to_string_arg(arg)
            if expr is not None:
                out.append(f"        __acc = __acc + ({expr})")
        else:
            out.append(f"        __acc = __acc + len({arg})")
    out.append("    __acc")
    with open(out_path, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(out) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
