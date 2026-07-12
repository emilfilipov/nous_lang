#!/usr/bin/env python3
"""Turn a corpus category's lullaby.lby (a single-shot correctness driver) into a
HOT compute driver: keep every function definition, keep main's setup statements,
and wrap the work (the `println(...)` calls) in a repeat loop, folding each
printed string's length into an accumulator (no I/O). This exercises the
category's functions N times so the interpreters/native do real, timeable compute.

  python gen_perf_driver.py <category/lullaby.lby> <N> <out.lby>
"""
import re
import sys


def main() -> int:
    path, n, out_path = sys.argv[1], int(sys.argv[2]), sys.argv[3]
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
        out.append(f"        __acc = __acc + len({arg})")
    out.append("    __acc")
    with open(out_path, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(out) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
