# Alpha 1 Language Surface

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This document freezes the installable Alpha 1 surface. The implemented parser grammar is drafted in [formal_grammar.md](formal_grammar.md). If another design document describes a feature that is not listed here, treat that feature as planned design material, not implemented Alpha 1 behavior.

## Source And Blocks

- Source files use the `.lby` extension. It is the only accepted source extension; the original `.lullaby` extension has been retired.
- Scope is indentation-only.
- Curly braces are rejected as block delimiters.
- Semicolons are rejected as statement terminators.
- Comments begin with `#` and continue to the end of the line.

## Declarations

- Functions use `fn name param Type -> ReturnType`.
- Function parameters require explicit types.
- Non-void functions return the last reachable expression unless `return expression` exits earlier.
- Void functions use `-> void` and may use bare `return`.
- Executable source passed to `lullaby compile` or source `lullaby run` must define `fn main -> Type` with zero parameters. `lullaby check` can still validate helper/library-style functions that do not define `main`.
- Local bindings use `let name Type = expression` for explicit annotations or `let name = expression` when the initializer type is unambiguous.
- Existing local bindings can be updated with `=`, `+=`, `-=`, `*=`, and `/=` when the types are valid.

## Types

- Implemented scalar types: `i64`, `f64`, `bool`, `string`, `char`, `byte`, and `void`.
- Float literals contain a decimal point (e.g. `3.14`, `2.0`) and have type `f64`. `i64` and `f64` do not mix implicitly; combining them is a type error.
- `char` is a single Unicode scalar, written with single quotes (e.g. `'a'`, `'Z'`). A char literal must hold exactly one character; an empty (`''`), multi-character (`'ab'`), or unterminated literal is a lexer error (`L0105`). Chars compare with `==`/`!=` and order with `<`/`<=`/`>`/`>=` by code point. `char_code(c)` returns the Unicode scalar value as `i64`, and `char_from(i)` returns the `char` for a scalar value (a runtime error if `i` is not a valid Unicode scalar).
- `byte` is an 8-bit unsigned integer (0–255). There is no byte literal; construct one with `byte(i)` from an `i64` (a runtime error outside 0–255) and read it back with `byte_val(b)` as `i64`. Bytes compare with `==`/`!=` and order with `<`/`<=`/`>`/`>=` numerically. No byte arithmetic is implemented for Alpha 1 — conversions and comparisons only. Wrong argument types or arities to `char_code`/`char_from`/`byte`/`byte_val` report `L0389`. Other sized integer types (`i32`, `u32`, etc.) remain planned.
- Implemented array spelling: `array<T>`.
- Structs: `struct NAME` followed by indented `field type` lines declares a nominal record type (top level only). Construct positionally with call spelling — `Point(3, 4)` — read fields with `.` — `p.x` — and mutate fields with assignment — `p.x = 5`, `p.y += 1`, including nested `a.b.c = e`. Invalid declarations report `L0370`, bad field access `L0371`, and construction mismatches `L0372`; a field assignment with a wrong-type value reports `L0314`. Structs work across the AST, IR, and bytecode backends. Fields can also be set by name in any order — `Point(y: 4, x: 3)` — where every declared field must be supplied exactly once; duplicate, missing, or unknown named fields report `L0372`.
- Enums (tagged unions): `enum NAME` followed by indented `Variant type...` lines declares a nominal tagged-union type (top level only). Each variant is a name plus zero or more positional, unnamed payload types (e.g. `Circle f64`, `Rect f64 f64`, `Empty`). Construct with the existing spelling — a payload variant reads like a call (`Circle(2.0)`, `Rect(3.0, 4.0)`) and a unit variant reads like a bare name (`Empty`) — resolved semantically with no new syntax. Variant names are globally unique across all enums. Invalid declarations (duplicate enum, duplicate variant, or empty enum) report `L0380`, construction arity/payload-type mismatches report `L0381`, and a variant name colliding across enums reports `L0382`. Enums construct and pass identically across the AST, IR, and bytecode backends (including under optimization). Enum methods and user-defined generics over enums remain planned.
- `option<T>` and `result<T, E>`: two built-in generic enums for absence and fallible results. `option<T>` has variants `some(value)` and `none`; `result<T, E>` has variants `ok(value)` and `err(error)`. The variant names `some`/`none`/`ok`/`err` are reserved globally (a user enum declaring one reports `L0382`), and `option`/`result` may not be redeclared as user enums (`L0380`). Type spelling reuses the generic form with the canonical `", "`-joined argument list — `option<i64>`, `result<i64, string>`, `option<array<i64>>`. Construction is context-directed: `some(v)` synthesizes `option<typeof v>`, while `none`, `ok(v)`, and `err(v)` require an expected `option`/`result` type from a `let` annotation or a function return type; missing that context reports `L0386`, and a payload whose type disagrees reports `L0303`. `match` over an `option<U>` binds `some(x): x=U` and covers `some`+`none` (or `_`); over a `result<T, E>` it binds `ok(x): x=T` and `err(x): x=E` and covers `ok`+`err` (or `_`), reusing `L0384`/`L0385`. `option`/`result` construct and match identically across the AST, IR, and bytecode backends (including under optimization). Argument-position inference and `?`-style propagation remain planned.
- Array literals must be non-empty and homogeneous, such as `[1, 2, 3]`.
- Array indexing is bounds-checked at runtime and requires an `i64` index.
- Array elements are mutable through assignment — `xs[i] = e`, `xs[i] += 1`, and mixed struct/array targets such as `cells[0].value = e`. The root variable is what optimizers track, and out-of-bounds writes report `L0413`.
- `list<T>`: a growable, value-semantic list spelled `list<T>` (e.g. `list<i64>`), backed by the same runtime array representation. Its builtins are functional — a "mutating" call returns a new list, so the idiom is `l = push(l, x)`. `list_new()` builds an empty list and takes its element type from the expected `list<...>` type (a `let` annotation or function return type); with no such context it reports `L0387`. `push(l, x)` appends, `get(l, i)` reads (bounds-checked, `L0413`), `set(l, i, x)` replaces at an index (bounds-checked), `pop(l)` drops the last element (empty pop errors), and `len(l)` returns the `i64` length. A wrong argument type or arity to these builtins reports `L0387`. `list<T>` behaves identically across the AST, IR, and bytecode backends (including under optimization).
- `map<K, V>`: an insertion-ordered, value-semantic hash map spelled `map<K, V>` (e.g. `map<string, i64>`). Keys are restricted to `i64` or `string`. Its builtins are functional — a "mutating" call returns a new map, so the idiom is `m = map_set(m, k, v)`. `map_new()` builds an empty map and takes its key/value types from the expected `map<...>` type (a `let` annotation or function return type); with no such context, an unsupported key type, or a wrong argument/arity it reports `L0388`. `map_get(m, k)` returns `option<V>` — `some(v)` if present, else `none` — so reads are unwrapped with `match`. `map<K, V>` behaves identically across the AST, IR, and bytecode backends (including under optimization).
- Interim pointer type names use concrete spellings such as `ptr_i64`.
- Omitted local binding annotations are inferred from the initializer expression. Empty arrays and `void` initializers cannot supply an inferred local type.

## Expressions

- Implemented expressions include literals, variables, function calls with parentheses, grouped expressions, array literals, array indexing, arithmetic, equality, ordering, and logical operators.
- Method-call sugar: `receiver.name(args)` desugars to `name(receiver, args)` (UFCS) at parse time, so any function whose first parameter matches the receiver can be called in method position — e.g. `p.norm2()` calls `fn norm2 p Point -> i64`. Plain `receiver.name` (no parentheses) remains struct field access.
- Arithmetic `+`, `-`, `*`, and `/` operate on two `i64`s or two `f64`s (operands must match); `+` also concatenates two `string`s. Integer division by zero is a runtime error; `f64` division follows IEEE 754 (division by zero yields infinity/NaN).
- Equality requires matching operand types.
- Ordering comparisons `<`, `<=`, `>`, and `>=` require two `i64`s or two `f64`s.
- Logical operators are `and`, `or`, and `not`; `and` and `or` short-circuit.

## Control Flow

- Implemented branches: `if`, `elif`, and `else`.
- Implemented loops: `while`, inclusive range `for name from start to end`, optional `by step`, and `loop`.
- Implemented loop controls: `break` and `continue`.
- Loop control outside a loop is a semantic error.
- Pattern matching: `match SCRUTINEE` selects on an enum value with an indented arm list. Each arm is a pattern (`Variant(bind1, bind2, ...)` binding positional payloads, a bare `Variant` for a unit variant, or `_` wildcard), then `->`, then an inline expression or an indented block whose last expression is the arm's value. Like `if`/`try`, a `match` is an expression: when every arm yields the same type it produces that value (so it can be a function's final expression); otherwise it is a void statement. Matches must be exhaustive — every variant covered or a `_` present (`L0384`); a non-enum scrutinee reports `L0383`, and an unknown/duplicate variant arm or wrong binding arity reports `L0385`. Pattern matching runs identically on the AST, IR, and bytecode backends (including under optimization).

## Builtins

- Memory builtins: `alloc(value)`, `load(ptr)`, `store(ptr, value)`, and `dealloc(ptr)`.
- Text file builtins: `read_file(path)`, `write_file(path, content)`, `append_file(path, content)`, and `file_exists(path)`.
- System command builtins: `sys_status(program, args)` and `sys_output(program, args)`, where `args` is `array<string>`.
- System command builtins execute a program with an argv array directly and do not invoke a shell.
- Standard stream builtins: `print(text)` and `println(text)` write a `string` to stdout, `warn(text)` writes a `string` line to stderr, and `flush()` flushes stdout. Each returns `void`.
- String operations: `to_string(x)` converts an `i64`, `f64`, `bool`, `string`, `char`, or `byte` to a `string`; `+` concatenates when both operands are `string` (and still adds when both are `i64`). Mixed `string`/`i64` operands to `+` are a type error (`L0307`). This makes computed values printable, e.g. `println("answer: " + to_string(40 + 2))`.
- Char/byte conversions: `char_code(c char)` returns an `i64` Unicode scalar value and `char_from(i i64)` returns a `char` (runtime error on an invalid scalar); `byte(i i64)` returns a `byte` (runtime error outside 0–255) and `byte_val(b byte)` returns its `i64` value. Wrong argument types or arities report `L0389`.
- Collection length: `len(x)` returns the `i64` element count of an `array<T>` or `list<T>`, or the character count of a `string`; other argument types report `L0373`. Combine it with indexing for iteration, e.g. `for i from 0 to len(xs) - 1`.
- List library (functional, value-semantic): `list_new()` builds an empty `list<T>` (element type from the expected `list<...>` type; `L0387` otherwise), `push(l, x)` appends, `get(l, i)` reads (bounds-checked, `L0413`), `set(l, i, x)` replaces at an index, and `pop(l)` drops the last element. Each mutating call returns a new list, so the idiom is `l = push(l, x)`. Wrong argument types or arities report `L0387`.
- Map library (functional, value-semantic): `map_new()` builds an empty `map<K, V>` with `i64`/`string` keys (key/value types from the expected `map<...>` type; `L0388` otherwise), `map_set(m, k, v)` inserts or replaces, `map_get(m, k)` returns `option<V>` (`some(v)` or `none`), `map_has(m, k)` returns a `bool`, `map_len(m)` returns the `i64` entry count, and `map_del(m, k)` removes a key (no error if absent). Each mutating call returns a new map, so the idiom is `m = map_set(m, k, v)`. Wrong argument types, arities, or unsupported key types report `L0388`.
- String library (character-indexed): `substring(s, start, end)` slices the half-open `[start, end)` char range (out-of-range indices error at runtime); `find(s, needle)` returns the first char index or `-1`; `contains(s, needle)` returns a `bool`; `split(s, sep)` returns `array<string>` (empty `sep` errors); `join(parts, sep)` joins an `array<string>`; `trim(s)` removes leading/trailing ASCII whitespace; `replace(s, from, to)` replaces every occurrence (empty `from` errors); `upper(s)`/`lower(s)` change case. Wrong argument types report `L0375`.
- Math library: type-directed numeric builtins over `i64` and `f64`. `abs(x)` returns the magnitude (`i64->i64`, `f64->f64`); `min(a, b)` and `max(a, b)` pick the smaller/larger of two matching operands; `pow(base, exp)` raises to a power (integer form requires `exp >= 0`, a negative integer exponent is a runtime `L0417`); `sqrt(x f64)`, `floor(x f64)`, `ceil(x f64)`, and `round(x f64)` return `f64`. Wrong or mismatched operand types report `L0374`.
- Reference types: `rc<T>` is a reference-counted shared owner, `ref<T>` is a non-null borrowed reference, and `ptr<T>` (legacy spelling `ptr_T`) is a raw pointer.
- Reference builtins: `rc_new(value)` creates an `rc<T>`; `rc_clone(rc<T>)` shares ownership; `rc_release(rc<T>)` drops one owner and frees at zero; `rc_get(rc<T>)`/`ref_get(ref<T>)` read the referent; `rc_borrow(rc<T>)` yields a `ref<T>`.
- `unsafe` block: an indented block introduced by `unsafe` in which raw-pointer operations are permitted. `ptr_read(ptr<T>)` and `ptr_write(ptr<T>, value)` require an `unsafe` context (`L0330` otherwise); `unsafe` is a transparent scope, so bindings inside it remain visible afterward.
- Region declarations: `region NAME: size=N[, align=N][, kind=static|dynamic][, mutable=true|false]` declares a named memory region. Size must be positive, alignment (if present) must be a power of two, and kind must be `static` or `dynamic` (`L0340`); region names must be unique within a function (`L0341`). Regions are compile-time metadata in Alpha 1 (surfaced as `RegionCreate` in memory analysis) with no runtime allocation yet.
- Lifetime analysis: a conservative compile-time pass rejects straight-line use-after-free and double-free of resources freed by `dealloc`/`rc_release` (`L0350`), and rejects returning a borrowed `ref<T>` from a function (`L0351`). Freeing inside a branch is not tracked out of that branch; the runtime `L0406` guard remains as defense-in-depth. Deterministic per-block cleanup ordering is provided by `lullaby_ir::frame_layout`.
- Structured error handling: `throw EXPR` raises a `string` error value; `try` / `catch NAME` runs a protected block and, if it throws, binds the error message to `NAME` (a `string`) and runs the handler for recovery. Like `if`/`else`, a `try`/`catch` is an expression: when both arms yield the same type it produces that value (so it can be a function's final expression); otherwise it is a void statement. Uncaught throws surface as runtime `L0420`. Only user-thrown errors are caught; a `try` without `catch` reports `L0213`. Structured error handling runs identically on the AST, IR, and bytecode backends (including under optimization).
- Type aliases: `alias NAME = TYPE` declares a top-level type alias (e.g. `alias Count = i64`, `alias Numbers = array<i64>`). Aliases resolve structurally to their canonical target before type checking, so an alias and its target are interchangeable, and aliases carry no runtime representation. Aliases inside generic arguments (`array<Count>`) resolve too. Duplicate aliases report `L0360` and cyclic aliases report `L0361`. Struct/record and map generics remain planned until those types exist.

## CLI And Artifacts

- Development commands are available through `cargo run -p lullaby_cli -- ...`.
- The release package exposes `bin\lullaby.exe`.
- Supported commands:
  - `lullaby check [--verbose|--format json] <file.lby>`
  - `lullaby compile [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>`
  - `lullaby build [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>`
  - `lullaby inspect [--verbose|--format json] <file.lbc>`
  - `lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] [--verbose|--format json] <file.lby>`
  - `lullaby run [--verbose|--format json] <file.lbc>`
  - `lullaby fmt [--write|--check] <file.lby>`
  - `lullaby docs`
  - `lullaby examples`
  - `lullaby --version`
- `--diagnostic-format json` is accepted as a JSON diagnostics alias.
- `lullaby compile`, `lullaby build`, and source `lullaby run` require a zero-argument `main` entry point before lowering or execution. Invalid executable entry points report `L0329`.
- `lullaby compile` and its artifact-generation alias `lullaby build` write a versioned `.lbc` instruction-bytecode artifact with a format marker, artifact version, metadata, entry point, function table, ordered memory operation metadata, compatibility checks, and bytecode module instructions. `lullaby inspect` prints artifact metadata, function signatures, and memory operation counts without executing the program.

## Diagnostics

- Alpha diagnostics use stable `L####` codes documented in [diagnostic_registry.md](diagnostic_registry.md).
- Concise diagnostics are the default.
- `--verbose` adds source excerpts, caret markers, root-cause text, suggested fixes, related notes, and runtime traceback frames when available.
- `--format json` emits deterministic machine-readable diagnostics for tools, CI, editors, and LLM agents.

## Packaging

- `scripts/package_windows_portable.ps1` builds the Windows Alpha 1 portable package and zip archive under `dist/`.
- The package contains `bin\lullaby.exe`, `docs\index.html`, valid `.lby` examples, invalid diagnostic examples, optional PATH setup/cleanup helpers, README/VERSION metadata, a zip checksum, and a repository license file if one exists.
- `scripts/verify_release.ps1` is the Alpha 1 release gate for the packaged toolchain.
- `scripts/publish_github_release.ps1` verifies the package, tags the current commit, and creates a GitHub prerelease with the portable zip plus checksum asset.

## Planned Beyond Alpha 1

The following are not implemented Alpha 1 behavior:

- Native code generation for the full language, linking, and machine-code binary output (a COFF object prototype exists for a small subset).
- Modules, packages, imports, unions, traits, interfaces, and classes (structs and enums are implemented — structs including field mutation and positional/named-field construction, enums including unit/payload declaration and construction, `match` pattern matching over enums with payload binding, exhaustiveness, and `_` wildcard, and the built-in generic enums `option<T>`/`result<T, E>`; struct/enum methods and user-defined generics over enums are deferred).
- User-defined generics beyond `array<T>`, the built-in `option<T>`/`result<T, E>` enums, the `ptr<T>`/`ref<T>`/`rc<T>` reference types, and type aliases (struct/record and map generics, plus argument-position inference for `option`/`result` construction, remain planned).
- GC hooks and runtime region allocation (region *declarations* are analyzed as compile-time metadata; reference counting via `rc<T>` and conservative lifetime analysis are implemented).
- Binary I/O, memory mapping, async, sockets, IPC, and general syscall APIs (standard text streams and file/system builtins are implemented).
- A full installer; Alpha 1 uses a Windows portable archive with optional user PATH helper scripts.

When planned syntax keywords such as `import`, `module`, `switch`, or a bare `catch` appear as source syntax, the parser reports `L0211` instead of accepting a partial or ambiguous construct.
