# Diagnostic Registry

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This registry defines the stable diagnostic codes currently emitted by the Lullaby alpha. Diagnostics are designed for both human readers and LLM/tooling consumers.

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
| `L0211` | parser | Planned syntax is not supported in Alpha 1. | Source uses future constructs such as modules, imports, structs, pattern matching, or try/catch. | Remove the planned construct or rewrite with the current Alpha 1 surface. |
| `L0212` | parser | Malformed type alias declaration. | An `alias NAME = TYPE` declaration lacks `=` or a target type. | Write `alias NAME = TYPE`, e.g. `alias Count = i64`. |
| `L0213` | parser | `try` block missing its `catch`. | A `try` block was not followed by a `catch NAME` handler. | Add a `catch NAME` block after the `try` body. |
| `L0300` | semantic | Duplicate function. | Two functions share a name. | Rename or remove one function. |
| `L0301` | semantic | Non-void function has no final value of declared type. | Control reaches the end without the expected value. | Add a final expression or return the declared type. |
| `L0302` | semantic | Duplicate parameter. | Function has repeated parameter names. | Rename one parameter. |
| `L0303` | semantic | Binding initializer type mismatch. | Declared local type differs from initializer type, or an inferred binding cannot get a concrete local type. | Match the declared type and initializer, or add an explicit usable annotation. |
| `L0304` | semantic | Return type mismatch. | Return expression differs from function return type. | Return the declared type or change the signature. |
| `L0305` | semantic | Condition is not bool. | `if` or `while` condition has non-bool type. | Use a bool expression. |
| `L0306` | semantic | Unknown variable. | Name is not visible in current scope. | Add a `let`, parameter, or fix the name. |
| `L0307` | semantic | Arithmetic operands are not both `i64`. | Arithmetic used with non-numeric values. | Use `i64` operands. |
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
| `L0323` | semantic | Empty arrays unsupported in alpha. | Array literal contains no values. | Provide at least one value. |
| `L0324` | semantic | Array literal type mismatch. | Array values are not homogeneous. | Use values of one type. |
| `L0325` | semantic | Index target is not an array. | Index syntax used on a non-array value. | Index only `array<T>` values. |
| `L0326` | semantic | Array index is not `i64`. | Index expression has wrong type. | Use an `i64` index. |
| `L0327` | semantic | Ordering operands are not both `i64`. | `<`, `<=`, `>`, or `>=` used on non-`i64`. | Use `i64` operands. |
| `L0328` | semantic | `store` value type mismatch. | Stored value does not match pointer element type. | Store a value matching the pointer type. |
| `L0329` | semantic | Executable entry point is missing or has parameters. | Source passed to `compile`, `build`, or source `run` lacks a zero-argument `main`. | Add `fn main -> Type` with no parameters and call helpers from there. |
| `L0330` | semantic | Raw pointer operation used outside `unsafe`. | `ptr_read`/`ptr_write` were called outside an `unsafe` block. | Wrap the operation in `unsafe`, or use safe `rc<T>`/`ref<T>` references. |
| `L0331` | semantic | Reference builtin received the wrong type. | An `rc`/`ref`/raw-pointer builtin was called with a value of a different kind. | Pass an `rc<T>` to rc builtins, a `ref<T>` to `ref_get`, or a raw pointer to `ptr_read`/`ptr_write`. |
| `L0340` | semantic | Invalid region size, alignment, or kind. | Size is not positive, alignment is not a power of two, or kind is not `static`/`dynamic`. | Use a positive size, power-of-two alignment, and a `static` or `dynamic` kind. |
| `L0341` | semantic | Duplicate region name. | A region name was declared more than once in the same function. | Give each region a unique name. |
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
| `L0501` | ir | IR lowering failed. | A checked program did not match the current IR lowering contract. | Treat this as a compiler bug and retry with `--backend ast` as a workaround. |
| `L0502` | optimizer | Optimizer mode is incompatible with the selected backend. | `--optimize` was requested with the default AST backend. | Add `--backend ir` or `--backend bytecode`, or use `--optimize none`. |
| `L0601` | bytecode | Bytecode artifact failed to load. | The `.lbc` artifact is malformed, has an unsupported format/version/metadata target or payload, names an unsupported or missing entry point, contains duplicate functions or parameters, has a mismatched function table, or contains an invalid instruction contract such as `break`/`continue` outside a loop. | Recompile the source with the current `lullaby compile` or `lullaby build` command. |
| `L0400` | runtime | Missing `main`. | Runtime cannot find an entrypoint. | Define `fn main`. |
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
| `L0419` | resource | Standard stream write or flush failed. | stdout/stderr was closed or the pipe was broken. | Keep the output stream open or redirect it to a writable destination. |
| `L0420` | runtime | Uncaught thrown error. | A `throw` propagated past every enclosing `try`/`catch`. | Wrap the throwing code in `try` / `catch NAME`, or avoid the throwing condition. |
| `L0421` | runtime | Expected `f64`. | An f64 operation received a non-float value. | Ensure the operand is an `f64`; the type checker normally prevents this. |
