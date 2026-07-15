# Diagnostic Registry

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This registry defines the stable diagnostic codes currently emitted by the Lullaby compiler. Diagnostics are designed for both human readers and LLM/tooling consumers.

## Output Modes

- Concise default: `lullaby check file.lby`, `lullaby compile file.lby`, `lullaby build file.lby`, and `lullaby run file.lby` print one line per diagnostic.
- Verbose: `lullaby check --verbose file.lby`, `lullaby compile --verbose file.lby`, `lullaby build --verbose file.lby`, and `lullaby run --verbose file.lby` print source excerpts, caret markers, root cause, suggested fix, notes, and runtime tracebacks when available.
- JSON: `lullaby check --format json file.lby`, `lullaby compile --format json file.lby`, `lullaby build --format json file.lby`, and `lullaby run --format json file.lby` print deterministic JSON. `--diagnostic-format json` is accepted as an alias.

JSON failures are written to stderr and keep a non-zero exit status. JSON successes are written to stdout as:

```json
{"status":"ok","diagnostics":[]}
```

## JSON Schema

Each diagnostic object uses stable field names:

```json
{
  "code": "L0313",
  "phase": "semantic",
  "severity": "error",
  "message": "argument 2 for `sys_status` must be `array<string>` but got `array<i64>`",
  "source_path": "tests/fixtures/invalid/sys_args_type.lby",
  "span": {"line": 2, "column": 24},
  "function": "bad",
  "explanation": "Function and builtin arguments are statically type checked.",
  "root_cause": "The argument expression type does not match the parameter type.",
  "suggested_fix": "Pass a value of the expected type or change the called function signature.",
  "notes": [],
  "traceback": []
}
```

Fields that are not known for a diagnostic are `null` or an empty array. Ordering is deterministic.

## Codes

| Code | Phase | Meaning | Likely cause | Suggested fix |
| :--- | :--- | :--- | :--- | :--- |
| `L0001` | source | Unsupported source extension. | File path does not end in `.lby`. | Rename the source file or pass a `.lby` file. |
| `L0002` | resource | CLI could not read a source file. | Missing file or unreadable path. | Check path and permissions. |
| `L0003` | resource | CLI could not write a compiled artifact. | Output path is unwritable or its directory is missing. | Choose a writable `-o` path or create the parent directory. |
| `L0101` | lexer | Indentation does not match an active block. | A line dedented to a column not on the indent stack. | Align with an existing block level. |
| `L0102` | lexer | Curly braces are forbidden. | Source uses `{` or `}` as a block delimiter. | Remove braces and use indentation. |
| `L0103` | lexer | Semicolons are forbidden. | Source uses `;` as a statement terminator. | Remove semicolons and use one statement per line. |
| `L0104` | lexer | Unterminated string literal. | A closing quote is missing. | Add the closing quote. |
| `L0105` | lexer | Invalid char literal. | A `'...'` literal was empty, held more than one character, or was never closed. | Write a single-character char literal such as `'a'`, or use a string literal for text. |
| `L0201` | parser | Expected top-level function declaration. | Non-function syntax appears at top level. | Start top-level code with `fn`. |
| `L0202` | parser | Missing return arrow before function return type. | Function signature lacks `->`. | Add `-> ReturnType`. |
| `L0203` | parser | Expected type syntax. | Type annotation is missing or malformed. | Use current type spelling such as `i64`, `bool`, `string`, or `array<T>`. |
| `L0204` | parser | Expected identifier. | A name is missing where required. | Add a valid identifier. |
| `L0205` | parser | Expected structural token. | Missing newline, indent, or dedent. | Check indentation and required block bodies. |
| `L0206` | parser | Expected `=` in a let binding. | A `let` statement lacks an initializer separator. | Use `let name Type = expression` or `let name = expression`. |
| `L0207` | parser | Invalid expression. | Unsupported or malformed expression syntax. | Use supported expression forms and matching delimiters. |
| `L0210` | parser | Malformed region declaration. | The `region NAME: size=N[, ...]` form has a missing colon, `=`, field value, or unknown field. | Write `region NAME: size=N` with optional `align`, `kind`, and `mutable` fields. |
| `L0208` | parser | Expected assignment operator. | Assignment statement has malformed operator. | Use `=`, `+=`, `-=`, `*=`, or `/=`. |
| `L0209` | parser | Expected `from` in for loop. | Range loop header is malformed. | Use `for name from start to end`. |
| `L0210` | parser | Expected `to` in for loop. | Range loop header is missing its end marker. | Add `to end`. |
| `L0211` | parser | Planned syntax is not supported yet. | Source uses future constructs such as modules, imports, structs, pattern matching, or try/catch. | Remove the planned construct or rewrite with the current surface. |
| `L0212` | parser | Malformed type alias declaration. | An `alias NAME = TYPE` declaration lacks `=` or a target type. | Write `alias NAME = TYPE`, e.g. `alias Count = i64`. |
| `L0213` | parser | `try` block missing its `catch`. | A `try` block was not followed by a `catch NAME` handler. | Add a `catch NAME` block after the `try` body. |
| `L0216` | parser | Malformed `trait`/`impl` declaration. | A `trait` method signature or `impl Trait for Type` block is missing `fn`, the `self` receiver, `for`, `->`, or a method body. | Write `trait NAME` with `fn method self ... -> Ret` signatures, and `impl Trait for Type` with `fn method self ... -> Ret` bodies. |
| `L0300` | semantic | Duplicate function. | Two functions share a name. | Rename or remove one function. |
| `L0301` | semantic | Non-void function has no final value of declared type. | Control reaches the end without the expected value. | Add a final expression or return the declared type. |
| `L0302` | semantic | Duplicate parameter. | Function has repeated parameter names. | Rename one parameter. |
| `L0303` | semantic | Binding initializer type mismatch. | Declared local type differs from initializer type, or an inferred binding cannot get a concrete local type. | Match the declared type and initializer, or add an explicit usable annotation. |
| `L0304` | semantic | Return type mismatch. | Return expression differs from function return type. | Return the declared type or change the signature. |
| `L0305` | semantic | Condition is not bool. | An `if`, `while`, or inline-conditional (`THEN if COND else ELSE`) condition has non-bool type. | Use a bool expression. |
| `L0306` | semantic | Unknown variable. | Name is not visible in current scope. | Add a `let`, parameter, or fix the name. |
| `L0307` | semantic | Arithmetic operands are not both the same numeric type (`i64`, `f64`, `i32`, or `u32`). | Mixed numeric widths, or arithmetic on non-numeric values. | Convert to a common width (`to_i32`/`to_u32`/`to_i64`) so both operands share one numeric type. |
| `L0308` | semantic | Equality operands have different types. | `==` or `!=` compares mismatched types. | Compare values of the same type. |
| `L0309` | semantic | Unknown function. | Called function is not declared or builtin. | Define the function or fix the name. |
| `L0310` | semantic | Pointer builtin expected pointer. | `load` or `store` got a non-pointer. | Pass a `ptr_*` value. |
| `L0311` | semantic | `dealloc` expected pointer. | `dealloc` got a non-pointer. | Pass a valid pointer. |
| `L0312` | semantic | Function argument count mismatch. | Call has too many or too few arguments. | Match the function or builtin arity. |
| `L0313` | semantic | Function argument type mismatch. | Argument type differs from parameter type. | Pass a value with the expected type. |
| `L0314` | semantic | Assignment type mismatch. | Assigned value differs from local type. | Assign a value of the local type. |
| `L0315` | semantic | Compound assignment requires `i64`. | `+=`, `-=`, `*=`, or `/=` used on non-`i64`. | Use numeric locals and values. |
| `L0316` | semantic | Assignment target not declared. | Assignment names an unknown local. | Declare the local first. |
| `L0317` | semantic | `break` outside loop. | `break` appears outside loop context. | Move it into a loop or remove it. |
| `L0318` | semantic | `continue` outside loop. | `continue` appears outside loop context. | Move it into a loop or remove it. |
| `L0319` | semantic | `not` operand is not bool. | Logical negation used on non-bool. | Use a bool operand. |
| `L0320` | semantic | Logical operands are not both bool. | `and` or `or` used on non-bool values. | Use bool operands. |
| `L0321` | semantic | For-loop bound is not `i64`. | `from` or `to` expression is non-`i64`. | Use `i64` bounds. |
| `L0322` | semantic | For-loop step is not `i64`. | `by` expression is non-`i64`. | Use an `i64` step. |
| `L0323` | semantic | Empty arrays unsupported. | Array literal contains no values. | Provide at least one value. |
| `L0324` | semantic | Array literal type mismatch. | Array values are not homogeneous. | Use values of one type. |
| `L0325` | semantic | Index target is not an array. | Index syntax used on a non-array value. | Index only `array<T>` values. |
| `L0326` | semantic | Array index is not `i64`. | Index expression has wrong type. | Use an `i64` index. |
| `L0327` | semantic | Ordering operands are not both the same orderable scalar (`i64`, `f64`, `i32`, `u32`, `char`, or `byte`). | `<`, `<=`, `>`, or `>=` used on mismatched or non-orderable operands. | Use two operands of the same orderable scalar type. |
| `L0328` | semantic | `store` value type mismatch. | Stored value does not match pointer element type. | Store a value matching the pointer type. |
| `L0329` | semantic | Executable entry point is missing or has parameters. | Source passed to `compile`, `build`, or source `run` lacks a zero-argument `main`. | Add `fn main -> Type` with no parameters and call helpers from there. |
| `L0330` | semantic | Raw pointer operation used outside `unsafe`. | `ptr_read`/`ptr_write` were called outside an `unsafe` block. | Wrap the operation in `unsafe`, or use safe `rc<T>`/`ref<T>` references. |
| `L0331` | semantic | Reference builtin received the wrong type. | An `rc`/`ref`/raw-pointer builtin was called with a value of a different kind. | Pass an `rc<T>` to rc builtins, a `ref<T>` to `ref_get`, or a raw pointer to `ptr_read`/`ptr_write`. |
| `L0332` | semantic | Process builtin argument type or arity mismatch. | `env`/`args` was called with the wrong number of arguments, or `env` with a non-`string` argument. | Call `env(name string)` with one `string` and `args()` with no arguments. |
| `L0333` | semantic | File-system builtin argument type or arity mismatch. | `read_lines`/`read_bytes`/`write_bytes`/`file_size`/`is_file`/`is_dir`/`list_dir`/`make_dir`/`remove_file`/`remove_dir` had the wrong arity, a non-`string` path, or a non-`list<byte>` data argument. | Pass a `string` path (and a `list<byte>` for `write_bytes`) and match the builtin arity. |
| `L0334` | semantic | `parallel_map` argument type or arity mismatch. | A `parallel_map` call did not receive exactly a `fn(i64) -> i64` first argument and a `list<i64>` second argument. | Pass a `fn(i64) -> i64` function value and a `list<i64>` of arguments to `parallel_map`. |
| `L0335` | semantic | TCP/UDP socket builtin argument type or arity mismatch. | A `tcp_connect`/`tcp_listen`/`tcp_accept`/`tcp_read`/`tcp_write`/`tcp_close`/`udp_bind`/`udp_send_to`/`udp_recv` call had the wrong arity or an argument of the wrong type (non-`string` host/data, non-`i64` port, or non-`Socket` handle). | Pass a `string` host/data, an `i64` port, and a `Socket` handle from a socket builtin, matching the builtin's arity. |
| `L0336` | semantic | HTTP client builtin argument type or arity mismatch. | An `http_get`/`http_post` call had the wrong arity or a non-`string` argument (`http_get` takes one `string` url; `http_post` takes a `string` url and a `string` body). | Pass a `string` url to `http_get`, and a `string` url plus a `string` body to `http_post`. |
| `L0337` | semantic | Concurrency builtin argument type or arity mismatch. | A `chan_new`/`send`/`recv`/`try_recv`/`spawn`/`task_join`/`mutex_new`/`mutex_get`/`mutex_set`/`mutex_add` call had the wrong arity or an argument of the wrong type (a non-`Chan`/`Task`/`Mutex` handle, a non-`i64` value, or a `spawn` first argument that is not a `fn(Chan, i64) -> void`). | Match the builtin's arity and pass a `Chan`/`Task`/`Mutex` handle where required, an `i64` value/delta, and a `fn(Chan, i64) -> void` function to `spawn`. |
| `L0338` | ir | No functions were eligible for the WebAssembly scalar subset. | `lullaby wasm` found no top-level function whose parameters and return type are all scalars (`i64`/`f64`/`bool`/`char`/`byte`); every function uses a non-scalar type, heap value, `match`, or a builtin. | Add or expose a scalar function, or keep running the program on the interpreters until the linear-memory WASM phase lands. |
| `L0340` | semantic | Invalid region size, alignment, or kind. | Size is not positive, alignment is not a power of two, or kind is not `static`/`dynamic`. | Use a positive size, power-of-two alignment, and a `static` or `dynamic` kind. |
| `L0341` | semantic | Duplicate region name. | A region name was declared more than once in the same function. | Give each region a unique name. |
| `L0342` | semantic | `assert` argument is not `bool`. | The `assert` builtin was called with an argument whose type is not `bool`. | Pass a single `bool` condition to `assert`. |
| `L0343` | loader | Invalid project manifest (`lullaby.json`). | The manifest is missing, is not valid JSON, carries a malformed `version` (not a semver-shaped `MAJOR.MINOR.PATCH` with an optional `-<prerelease>` suffix), names a `src` directory that does not exist, or names a local path dependency whose project root or `lullaby.json` is missing (or a `run`/`build`/`compile` target has no `entry`). | Add a `lullaby.json` at the project root, fix its JSON, give `version` (if present) a well-formed value like `"0.1.0"` or `"1.0.0-preview"`, and make sure every `src` directory and every dependency path points to an existing project directory that contains its own `lullaby.json`. |
| `L0344` | semantic | `async`/`await` used incorrectly. | `await` was applied to a value that is not a `Future<T>` (an ordinary synchronous call or a plain value), or an `await` expression could not resolve the awaited future type. | Await only the `Future<T>` returned by calling an `async fn`, and call `async fn`s to obtain something awaitable. |
| `L0347` | ir | Unknown or unsupported native target triple. | `lullaby native --target <triple>` named a triple the backend cannot emit. Only the x86-64 triples with an implemented object writer are supported: `x86_64-pc-windows-msvc` (COFF), `x86_64-unknown-linux-gnu` (ELF64), and `x86_64-apple-darwin` (Mach-O). `aarch64-apple-darwin` has no code generator yet. | Pass one of the supported x86-64 triples, or omit `--target` for the default `x86_64-pc-windows-msvc`. |
| `L0350` | semantic | Use-after-free / double-free. | A binding is used or freed again after a straight-line `dealloc`/`rc_release`. | Remove the later use, or reallocate/rebind first. |
| `L0351` | semantic | Borrowed `ref<T>` escapes its owner. | A function declares a `ref<T>` return type. | Return an owning `rc<T>` or a value instead. |
| `L0360` | semantic | Duplicate type alias. | Two `alias` declarations share a name. | Give each type alias a unique name. |
| `L0361` | semantic | Cyclic type alias. | An alias chain refers back to itself. | Break the cycle so each alias resolves to a concrete type. |
| `L0370` | semantic | Invalid struct declaration. | Duplicate struct name or duplicate field. | Give each struct and each field a unique name. |
| `L0371` | semantic | Invalid field access. | Value is not a struct, or the field does not exist. | Access an existing field on a struct value. |
| `L0372` | semantic | Struct construction mismatch. | Argument count or a type differs from the struct's fields. | Pass one argument per field, in order, with matching types. |
| `L0373` | semantic | `len` argument is not a string or array. | `len` was called on a value that is not `array<T>` or `string`. | Pass a string or array value to `len`. |
| `L0375` | semantic | String-library builtin argument has the wrong type. | An argument to a string builtin is not `string`, `i64`, or `array<string>`. | Pass strings for text, `i64` for char indices, and `array<string>` for `join`. |
| `L0374` | semantic | Math builtin received wrong or mismatched operand types. | A math builtin (`abs`, `min`, `max`, `pow`, `sqrt`, `floor`, `ceil`, `round`) got an argument of the wrong type or mismatched operands. | Pass matching numeric operands: `abs`/`min`/`max`/`pow` accept two `i64` or two `f64`; `sqrt`/`floor`/`ceil`/`round` require an `f64`. |
| `L0380` | semantic | Invalid enum declaration. | An enum name is duplicated, a variant name is duplicated within one enum, or an enum declares no variants. | Give each enum a unique name, each variant a unique name within its enum, and at least one variant. |
| `L0381` | semantic | Enum construction mismatch. | A variant was constructed with the wrong payload arity or a payload argument type differs from the variant's declaration (including using a payload variant as a unit variant). | Pass one argument per declared payload type, in order, with matching types. |
| `L0382` | semantic | Variant name collides across enums. | The same variant name is declared in more than one enum, so construction cannot resolve unambiguously. | Rename one of the colliding variants so every variant name is globally unique. |
| `L0383` | semantic | Match scrutinee is not an enum. | The value matched by a `match` is not an enum type. | Match on an enum value, or use `if`/comparisons for non-enum types. |
| `L0384` | semantic | Non-exhaustive match. | One or more variants of the scrutinee's enum have no arm and there is no `_` wildcard. | Add an arm for each missing variant, or add a `_` wildcard arm. |
| `L0385` | semantic | Invalid match arm. | An arm names an unknown variant, repeats a variant, or binds the wrong number of payload values. | Use each enum variant at most once and bind exactly one name per declared payload value. |
| `L0386` | semantic | Cannot infer `none`/`ok`/`err` type. | The type of a `none`/`ok`/`err` constructor could not be inferred from its payload and no expected `option`/`result` type was available from context. | Add an `option<...>`/`result<...>` type annotation on the `let`, or use it where a function return type of that shape supplies the expected type. |
| `L0387` | semantic | Invalid `list<T>` builtin call or uninferable element type. | A `list_new`/`push`/`get`/`set`/`pop` call had the wrong argument type or arity, or `list_new` had no expected `list<...>` type to fix its element type. | Pass a `list<T>` and a matching `T` element, and give `list_new()` a `list<...>` annotation or return type so its element type is known. |
| `L0388` | semantic | Invalid `map<K, V>` builtin call, unsupported key type, or uninferable key/value type. | A `map_new`/`map_set`/`map_get`/`map_has`/`map_len`/`map_del` call had the wrong argument type or arity, used a key type other than `i64`/`string`, or `map_new` had no expected `map<...>` type to fix its key/value types. | Pass a `map<K, V>` with `i64`/`string` keys and matching `K`/`V` arguments, and give `map_new()` a `map<...>` annotation or return type so its key/value types are known. |
| `L0389` | semantic | Invalid `char`/`byte` builtin call. | A `char_code`/`char_from`/`byte`/`byte_val` call had the wrong argument type or arity. | Call `char_code(c char)`/`char_from(i i64)`/`byte(i i64)`/`byte_val(b byte)` with a single argument of the required type. |
| `L0390` | semantic | Call through a non-function local, or a function value with the wrong `fn(...)` signature. | A local of a non-function type was called like a function, or a function value whose `fn(...)` signature does not match the expected function type was used. | Call only locals of a function type `fn(T) -> R`, and make sure a passed function value's signature matches the expected function type exactly. |
| `L0391` | loader | Duplicate name across modules. | A `fn`/`struct`/`enum`/`alias` name is declared in more than one loaded module; the merged namespace is flat with no shadowing. | Rename one of the colliding declarations so every top-level name is unique across all imported modules. |
| `L0392` | loader | Cross-module use of a non-visible name. | A module referenced a name that is declared private (no `pub`) in another module, or referenced a `pub` name from a module it did not `import`. | Mark the referenced declaration `pub` in its own module and `import` that module, or keep the private item's use inside its own file. |
| `L0393` | loader | Import cycle. | A cycle of `import` statements was found while loading modules (for example `a` imports `b` and `b` imports `a`). | Break the import cycle, or factor the shared declarations into a third module. |
| `L0394` | parser | Invalid function type-parameter list. | A `<T, U>` list on a `fn` declaration repeated a name or used a built-in type name (a primitive or a generic-type constructor such as `list`/`option`). | Give each type parameter a distinct name that is not a built-in type. |
| `L0395` | semantic | Conflicting inference for a generic type parameter. | Two arguments bound the same type parameter to different concrete types — either a generic function call (for example `same(1, "x")` for `fn same<T> a T b T`) or a generic struct construction whose fields disagree on `T` (for example `Pair(1, true)` for `struct Pair<T>`). | Pass arguments whose types agree for every occurrence of the same type parameter. |
| `L0396` | semantic | Uninferable generic type parameter. | A type parameter appears only in the return type with no argument to pin it, and explicit type arguments are not yet supported. | Give the function a parameter that uses the type parameter so it can be inferred. |
| `L0397` | loader | Missing module file. | `import NAME` could not find `NAME.lby` in the entry file's directory. | Create `NAME.lby` next to the entry file, or fix the import name to match an existing module file. |
| `L0398` | semantic | Trait implementation does not satisfy the trait. | An `impl Trait for Type` is missing a required method, declares an extra method, has a signature that does not match the trait (with `Self` = the implementing type), duplicates a trait/method name, or collides with a free function. | Provide exactly the trait's methods with matching `self`-first signatures, and keep trait-method and free-function names disjoint. |
| `L0399` | semantic | Duplicate trait implementation. | The same `impl Trait for Type` pair is declared more than once. | Remove the duplicate `impl` block; a trait may be implemented for a type only once. |
| `L0400` | semantic | Trait bound not satisfied. | A type used where a trait bound is required — a bounded generic call `f<T: Trait>`, a direct trait-method call, or a type argument in a generic-type instantiation whose parameter carries a bound (`Sorted<i64>` for `struct Sorted<T: Ord>`) — does not implement that trait. | Add an `impl Trait for Type` for the type, or pass/instantiate with a type that implements the bound. |
| `L0501` | ir | IR lowering failed. | A checked program did not match the current IR lowering contract. | Treat this as a compiler bug and retry with `--backend ast` as a workaround. |
| `L0502` | optimizer | Optimizer mode is incompatible with the selected backend. | `--optimize` was requested with the default AST backend. | Add `--backend ir` or `--backend bytecode`, or use `--optimize none`. |
| `L0601` | bytecode | Bytecode artifact failed to load. | The `.lbc` artifact is malformed, has an unsupported format/version/metadata target or payload, names an unsupported or missing entry point, contains duplicate functions or parameters, has a mismatched function table, or contains an invalid instruction contract such as `break`/`continue` outside a loop. | Recompile the source with the current `lullaby compile` or `lullaby build` command. |
| `L0422` | runtime | Missing `main`. | Runtime cannot find an entrypoint. | Define `fn main`. |
| `L0401` | runtime | Unknown function at runtime. | Runtime call target was not found. | Check semantic validation and function names. |
| `L0402` | runtime | Runtime function arity mismatch. | Runtime call has wrong argument count. | Match the function signature. |
| `L0403` | runtime | Unknown variable at runtime. | Runtime scope lookup failed. | Check variable declaration and scope. |
| `L0404` | runtime | Division by zero. | Divisor evaluated to zero. | Guard or change the divisor. |
| `L0405` | runtime | Builtin arity mismatch. | Builtin received wrong argument count. | Match builtin arity. |
| `L0406` | runtime | Invalid pointer. | Pointer was deallocated or never allocated. | Avoid use after deallocation. |
| `L0407` | runtime | Expected `i64`. | Runtime value kind was wrong. | Fix static typing or runtime expression. |
| `L0408` | runtime | Expected bool. | Runtime value kind was wrong. | Fix static typing or runtime expression. |
| `L0409` | runtime | Expected pointer. | Runtime value kind was wrong. | Pass a pointer value. |
| `L0410` | runtime | Loop control escaped function. | `break` or `continue` reached function boundary. | Keep loop control inside loops. |
| `L0411` | runtime | For-loop step is zero. | Step expression evaluated to 0. | Use non-zero step. |
| `L0412` | runtime | Runtime index target is not array. | Index target evaluated to non-array. | Index only arrays. |
| `L0413` | runtime | Array index out of bounds. | Computed index is negative or too large. | Check index before indexing. |
| `L0414` | resource | File read failed. | Missing, unreadable, or unsupported text file. | Check path, working directory, and permissions. |
| `L0415` | resource | File write or append failed. | Destination or parent directory is unavailable. | Use a writable path. |
| `L0416` | resource | Command launch failed. | Program not found or not executable. | Pass a valid executable and argv array. |
| `L0417` | runtime | Expected string. | Runtime value kind was wrong. | Pass a string value. |
| `L0418` | runtime | Expected `array<string>`. | Runtime value kind was wrong. | Pass an array of strings. |
| `L0419` | resource | Standard stream read, write, or flush failed. | stdout/stderr was closed or the pipe was broken, or stdin could not be read (e.g. non-UTF-8 bytes). | Keep the stream open, redirect output to a writable destination, or feed valid UTF-8 on stdin. |
| `L0420` | runtime | Uncaught thrown error. | A `throw` propagated past every enclosing `try`/`catch`. | Wrap the throwing code in `try` / `catch NAME`, or avoid the throwing condition. |
| `L0421` | runtime | Expected `f64`. | An f64 operation received a non-float value. | Ensure the operand is an `f64`; the type checker normally prevents this. |
| `L0423` | runtime | Cannot call an `extern fn` (C-ABI) function on an interpreter. | The AST/IR/bytecode interpreters cannot execute real C FFI; an `extern fn` only has meaning after native codegen + linking. | Compile with `lullaby native`, link against the C runtime, and run the resulting `.exe`. |
| `L0424` | semantic | Unsupported `extern`/`export fn` (C-ABI) signature. | An FFI signature used a type outside the marshalling set. An `export fn` covers only the Win64 scalar set (`i64`/`f64`/`f32`, non-generic). An `extern fn` parameter must be a C scalar (`i8`…`u64`/`isize`/`usize`/`bool`/`char`/`byte`/`f32`/`f64`), a raw pointer `ptr<T>`, `cstr`, or a **callback** `fn(A...) -> R` whose own parameters are C scalars/raw pointers and whose return is `void`/a C scalar/a raw pointer (a Lullaby top-level function passed to C as a C-ABI function pointer); its return must be `void`, a C scalar, or `ptr<T>`. Structs by value, a callback whose own signature is *not* C-marshallable (e.g. it takes a `string`/`list`/struct), a callback used as an extern *return*, `string`, `list`/`map`, and a `cstr` return are not yet marshallable, and neither an `extern` nor an `export` may be generic. | Restrict the exported function to the `i64`/`f64`/`f32` scalar set; for an extern, use a C scalar, a raw pointer `ptr<T>`, `cstr` (for a `string` argument), or a callback whose own parameters/return are C scalars or raw pointers, and receive an inbound C string as `ptr<byte>`. |
| `L0425` | semantic / runtime | Invalid or interpreter-run `asm` inline assembly. | An `asm` byte was outside `0..=255` or the statement was empty (semantic), or an `asm` statement was run on the AST/IR/bytecode interpreter, which cannot execute raw machine code (runtime). | Keep every `asm` byte in `0..=255` inside an `unsafe` block, and compile with `lullaby native` to emit and link the machine code rather than running it on an interpreter. |
| `L0426` | ir | Freestanding native build conflicts with the C runtime. | `lullaby native --freestanding` guarantees the emitted executable links no C runtime (only `kernel32!ExitProcess`), but the program declares an `extern fn` that must be linked against the C runtime import library (`ucrt.lib`). | Remove the `extern fn` (and its calls) for a freestanding build, or drop `--freestanding` to link the C runtime for the extern call. |
| `L0427` | semantic | `?` used where the enclosing function cannot propagate. | The postfix `?` operator requires the enclosing function to return a compatible `option`/`result`, but this function returns neither (or an incompatible kind). | Change the function's return type to `option<...>` (for an `option` operand) or `result<..., E>` (for a `result<..., E>` operand), or handle the value with `match` instead of `?`. |
| `L0428` | semantic | `?` applied to a non-`option`/`result` value. | The operand of `?` must be an `option<T>` or `result<T, E>`; it was some other type. | Apply `?` only to an `option`/`result` value, or unwrap the value with `match`. |
| `L0429` | semantic | `?` error type does not match the enclosing `result`. | `expr?` on a `result<T, E>` propagates `err(e)` unchanged, so the enclosing function must return `result<U, E>` with the SAME error type `E`; the error types differ. | Make the enclosing function return `result<U, E>` with the operand's error type, or convert the error explicitly before returning. |
| `L0430` | runtime | Internal `?` early-return sentinel (should never surface). | Used by the AST interpreter to unwind a `?` propagation to the enclosing function boundary; it is always caught internally. Seeing this code indicates an interpreter bug. | Report it — a `?` propagation escaped its function boundary. |
| `L0431` | semantic / runtime | Raw-memory layout builtin received an unsupported type or field. | `size_of`/`align_of` were applied to a type with no defined C-natural layout (a `string`, `list`, `map`, enum, closure, or other non-sized value), or `offset_of` was applied to a non-struct value, a struct with a non-sized field, or a missing or non-literal field name. | Query `size_of`/`align_of` only on scalars, pointer/reference handles, structs, and fixed `array<T>` values, and call `offset_of(x, "field")` with a struct `x` and a string-literal field name. |
| `L0432` | semantic / runtime | Invalid memory ordering for an atomic operation or fence. | An ordering-taking atomic builtin (`atomic_*_ordered`) or `fence` was given a `MemoryOrder` it does not permit: a load or a compare-and-swap failure ordering used `release`/`acq_rel`, a store used `acquire`/`acq_rel`, or a `fence` used `relaxed`. A literal ordering is rejected statically; a dynamically chosen ordering is guarded at runtime. | Use `relaxed`/`acquire`/`seq_cst` for a load and a CAS failure ordering, `relaxed`/`release`/`seq_cst` for a store, any of the five for a read-modify-write or a CAS success ordering, and `acquire`/`release`/`acq_rel`/`seq_cst` for a `fence`. |
| `L0433` | runtime | `array_fill` length is negative. | The count argument to `array_fill(n, value)` evaluated to a negative number. | Pass a non-negative length (`0` yields an empty array). |
| `L0434` | semantic | `for … in` target is not iterable. | The collection in a `for x in collection` loop is not an `array<T>`, `list<T>`, or `string`. | Iterate an array, list, or string, or use the numeric `for i from a to b` form. |
| `L0435` | semantic | Inline-conditional branches disagree. | The `then` and `else` branches of a `THEN if COND else ELSE` expression have different types. | Make both branches produce the same type. |
| `L0436` | semantic | Inline conditional over an unsupported type. | An inline conditional's branches produce an aggregate/heap type (struct, `array`, `list`, `map`, or enum). | Inline conditionals support scalar and `string` results; use an `if` statement to select an aggregate value. |
| `L0437` | semantic | `in` operand types are incompatible. | The `VALUE in COLLECTION` collection is not a `string`/`list<T>`, or the value does not match (a `string` needs a `char`/`string` value; a `list<T>` needs a `T`). | Test membership against a `string` (char/substring) or a `list<T>` element of the matching type. |
| `L0438` | semantic | Slice operand types are incompatible. | A `target[start:end]` slice has a non-`string` target, or a bound that is not `i64`. | Slice a `string` with `i64` bounds (either bound may be omitted). |
| `L0439` | semantic | Return type cannot be inferred. | A function declared without a `-> T` clause reaches its own return type through a (mutually) recursive call, so it cannot be inferred. | Add an explicit `-> T` return type to the recursive function. |
| `L0440` | semantic | `match` arms have incompatible types in value position. | A `match` is used where a value is required (a `let`/assignment right-hand side, a `return`, or a tail expression), but its arms do not all yield the same type, so the `match` has no value. | Make every arm yield the same type, or use a statement `match` if the value is not needed. |
| `L0450` | semantic | Invalid constant expression. | A `const NAME type = <expr>` initializer is not a *constant expression*: it references a non-constant name, calls a function, uses a non-const construct (array/index/field/struct/enum/match/closure/conditional/etc.), mixes or uses invalid operand types, or divides/takes a remainder by zero. | Build the initializer from literals and arithmetic/logical/bitwise/comparison/unary operators over literals and other already-defined constants; avoid calls, runtime values, and division by zero. |
| `L0451` | semantic | Constant type mismatch. | The value a constant evaluates to does not match its declared type (e.g. `const X i64 = "hi"`, or `const X f64 = 5` with no implicit int→float widening). | Give the constant a declared type that matches its value, or write the value at the declared type (e.g. `3.0` for an `f64`). |
| `L0452` | semantic | Cyclic constant reference. | A constant is defined in terms of itself, directly (`const A i64 = A + 1`) or through a cycle of other constants (`A = B + 1`, `B = A + 1`). | Break the cycle so each constant resolves to a concrete value without referring back to itself. |
| `L0453` | semantic | Duplicate or colliding constant. | A constant name is declared more than once, or collides with another top-level declaration (a function, struct, enum, or enum variant) in the flat, no-shadowing namespace. | Give each constant a unique name that does not collide with another top-level declaration. |
| `L0454` | semantic | Wrong generic-type argument arity. | A generic user struct or enum is spelled with the wrong number of type arguments — too many, too few, or none — for its declared type-parameter list (for example `Box<i64, bool>`, a bare `Box` for `struct Box<T>`, or `Opt<i64, bool>` for `enum Opt<T>`). | Spell the generic type with exactly as many type arguments as it declares type parameters (`Box<i64>` for `struct Box<T>`, `Opt<i64>` for `enum Opt<T>`). |
| `L0455` | semantic | Uninferable generic type parameter. | Constructing a generic user struct or enum leaves a type parameter unpinned: the arguments do not determine it (for example a struct parameter that appears in no field, or a generic enum's unit variant such as `absent` that carries no payload) and no `Name<...>` annotation supplies it. | Add a type annotation such as `Box<i64>` or `Opt<i64>` on the binding, parameter, or return so the type argument is fixed. |
| `L0456` | semantic | Infinitely-sized recursive generic enum. | A generic enum recurses on itself by value — a variant payload is the enum itself (`node Tree<T>` inside `enum Tree<T>`), or is nested only through the by-value tagged unions `option`/`result` — so the type has no finite size. | Route the recursion through a heap/pointer indirection: `rc<Tree<...>>`, `ref<...>`, `ptr<...>`, `list<Tree<...>>`, `map<...>`, or `array<...>`. |
| `L0457` | semantic | Method not found on the receiver's type. | A method call `recv.m(...)` names a method `m` declared by an inherent `impl` block on a *different* type than the receiver's — the receiver's type (for example `Opt<i64>`) declares no method `m` — or is made with no receiver. | Call the method on a value of the type whose `impl` block declares it, or add the method to the receiver type's `impl` block. |
