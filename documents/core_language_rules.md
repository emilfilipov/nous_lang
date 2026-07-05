# Lullaby Core Language Rules

This file is the canonical location for global Lullaby rules that apply across all subsystem documents and implementation work.

For the installable Alpha 1 feature freeze, see [alpha1_language_surface.md](alpha1_language_surface.md). Examples in this file may mention planned scoped constructs to show that indentation remains the universal block rule; they are not automatically implemented Alpha 1 syntax.

## Canonical Source Extension

Lullaby source files use the `.lby` file extension. The original alpha
extension `.lullaby` remains accepted as a backward-compatible legacy alias, so
existing sources keep compiling, but new files and examples should use `.lby`.

Examples:

```text
main.lby
kernel.lby
memory.lby
driver.lby
allocator.lby
```

The compiler, installer, tests, examples, CLI, diagnostics, generated project templates, and documentation should use `.lby` as the canonical extension unless the language specification is intentionally changed; `.lullaby` is accepted only for backward compatibility.

## Indentation-Only Scope

Lullaby / lullaby uses indentation-only scope.

Curly brace characters are forbidden as block delimiters in:

- functions
- conditionals
- loops
- structs and unions
- regions
- unsafe and unchecked blocks
- classes
- modules
- try and catch error-handling blocks
- every other scoped language construct

Canonical block form:

```lullaby
fn add x y
    x + y

fn max a b
    if a > b
        a
    else
        b

fn count_to n
    mut i = 0
    while i < n
        out i
        i = i + 1

struct Point
    x f64
    y f64

region temp size 1024 align 8
    buf = alloc temp
```

Rules:

- indentation is the only block delimiter
- no semicolons terminate statements
- a block begins after a line that introduces scope
- a block ends when indentation returns to the previous level

## Documentation Rule

Do not duplicate this block in subsystem documents. Link to this file instead.
