#!/usr/bin/env python3
"""Token count of the cross-language scalar suite (o200k_base, the modern GPT
tokenizer). Counts the *function definitions only* — comments, includes/imports,
attributes, and the verification `main`/driver are stripped — so it compares the
logic each language needs, not per-file boilerplate. This is the marketability
number: how many tokens an LLM must generate for the same 16 functions.

Run with a Python that has tiktoken installed, e.g.:
  C:/Users/emil/AppData/Local/Programs/Python/Python314/python.exe benchmarks/crosslang/tokens.py
"""
import re
from pathlib import Path

import tiktoken

ENC = tiktoken.get_encoding("o200k_base")
ROOT = Path(__file__).resolve().parent

# lang -> (relative file, kind, driver-start marker)
FILES = {
    "Lullaby": ("lullaby/scalar.lby", "lullaby", "fn main"),
    "C": ("c/scalar.c", "clike", "int main"),
    "C++": ("cpp/scalar.cpp", "clike", "int main"),
    "Rust": ("rust/scalar.rs", "rust", "fn main"),
    "Python": ("python/scalar.py", "python", 'if __name__'),
}
NAMES = [
    "add", "max2", "abs_val", "is_even", "clamp", "sign", "factorial", "gcd",
    "fib_iter", "is_prime", "int_pow", "collatz_len", "digit_sum",
    "count_primes_below", "power_mod", "ackermann",
]


def strip_comments(text, kind):
    if kind in ("clike", "rust"):
        text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
        text = re.sub(r"//[^\n]*", "", text)
    else:  # python, lullaby
        text = re.sub(r"#[^\n]*", "", text)
    return text


def strip_boilerplate(text, kind):
    out = []
    for ln in text.splitlines():
        s = ln.strip()
        if kind in ("clike", "rust") and (s.startswith("#") or s.startswith("using ") or s.startswith("use ")):
            continue
        if kind == "python" and (s.startswith("import ") or s.startswith("from ")):
            continue
        out.append(ln)
    return "\n".join(out)


def region_before_driver(text, marker):
    i = text.find(marker)
    return text[:i] if i >= 0 else text


def split_clike(region):
    funcs, depth, start = [], 0, 0
    for i, ch in enumerate(region):
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                funcs.append(region[start:i + 1].strip())
                start = i + 1
    return [f for f in funcs if f]


def split_prefix(region, kw):
    funcs, cur = [], []
    for ln in region.splitlines():
        if ln.lstrip().startswith(kw + " ") and ln[:1] not in (" ", "\t") and cur:
            funcs.append("\n".join(cur))
            cur = [ln]
        else:
            cur.append(ln)
    if cur:
        funcs.append("\n".join(cur))
    return [f.strip() for f in funcs if f.strip()]


def name_of(sig):
    for n in NAMES:
        if re.search(r"\b" + re.escape(n) + r"\s*\(", sig) or re.search(r"\bfn\s+" + re.escape(n) + r"\b", sig) or re.search(r"\bdef\s+" + re.escape(n) + r"\b", sig):
            return n
    return None


def tok(s):
    return len(ENC.encode(s.strip()))


per_lang = {}
per_func = {n: {} for n in NAMES}
helper = {}

for lang, (rel, kind, marker) in FILES.items():
    raw = (ROOT / rel).read_text(encoding="utf-8")
    region = strip_boilerplate(strip_comments(region_before_driver(raw, marker), kind), kind)
    region = re.sub(r"\n\s*\n+", "\n", region).strip()
    per_lang[lang] = tok(region)
    if kind in ("clike", "rust"):
        chunks = split_clike(region)
    elif kind == "python":
        chunks = split_prefix(region, "def")
    else:
        chunks = split_prefix(region, "fn")
    matched = 0
    for ch in chunks:
        n = name_of(ch.split("{")[0].split(":")[0] if kind != "lullaby" else ch.splitlines()[0])
        if n:
            per_func[n][lang] = tok(ch)
            matched += 1
        elif "rem" in ch.split("(")[0]:
            helper[lang] = tok(ch)  # Lullaby's rem helper (no % operator)

langs = list(FILES.keys())
print("== function definitions only, o200k_base tokens ==\n")
header = f"{'function':<20}" + "".join(f"{l:>9}" for l in langs)
print(header)
print("-" * len(header))
for n in NAMES:
    row = f"{n:<20}" + "".join(f"{per_func[n].get(l, '-'):>9}" for l in langs)
    print(row)
print("-" * len(header))
print(f"{'TOTAL (16 fns)':<20}" + "".join(f"{per_lang[l]:>9}" for l in langs))
print()
base = per_lang["Lullaby"]
print("vs Lullaby (tokens ÷ Lullaby's):")
for l in langs:
    print(f"  {l:<8} {per_lang[l]:>5}  ({per_lang[l] / base:.2f}x)")
