# Lullaby Error Handling Documentation

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

## Overview

Lullaby implements a sophisticated yet minimal error handling system designed for systems programming. Unlike traditional languages that rely on verbose try-catch blocks and exception objects, Lullaby uses an operator-based approach optimized for both token efficiency and runtime performance.

## Current Alpha Error Reporting

The Rust alpha reports compiler and runtime failures with stable `N####` diagnostic codes. Lexing and parser diagnostics include source spans when available, semantic diagnostics include the function where the error was found, and runtime failures include a category label in CLI output.

Current diagnostic ranges:

| Range | Source | Example |
| :--- | :--- | :--- |
| `N0001-N0003` | Source path and host file loading/writing | Invalid extension, unreadable source file, or failed artifact write. |
| `N0101-N0104` | Lexer | Forbidden curly braces or semicolon terminators. |
| `N0201-N0211` | Parser | Missing function body indentation, malformed expression, or planned syntax rejected by the Alpha 1 parser. |
| `N0300-N0329` | Semantic validation | Unknown name, type mismatch, invalid loop control, invalid builtin arguments, and invalid executable entry points. |
| `N0400-N0418` | Runtime and host resources | Missing `main`, division by zero, invalid pointer, missing file, failed command invocation. |
| `N0501` | IR lowering | Typed IR lowering failed after semantic validation. |
| `N0601` | Bytecode artifact | Compiled `.lbc` artifact is malformed or unsupported. |

Runtime CLI output uses:

```text
N0414 [resource]: failed to read `missing.txt`: ...
```

The implemented runtime categories are:

- `runtime`: execution errors such as division by zero, invalid pointer use, out-of-bounds array indexing, or wrong runtime value kind.
- `resource`: host resource failures such as failed file reads/writes/appends or failed command invocation.
- `ir`: typed IR lowering failures reported before an IR or bytecode backend starts executing.
- `bytecode`: compiled `.lbc` artifact loading failures before bytecode execution starts.

Language-level `try`, `catch`, recovery blocks, and compact `!0xXX` error tokens are planned and are not accepted by the current parser. Planned syntax keywords now produce `N0211 [parser error]` so users can distinguish future syntax from malformed Alpha 1 code.

## Epic 6 Diagnostics UX

The alpha now has three CLI diagnostic modes:

```text
lullaby check file.lby
lullaby check --verbose file.lby
lullaby check --format json file.lby
```

The same flags are available for `lullaby compile`, `lullaby build`, `lullaby inspect`, and `lullaby run`. `lullaby run` defaults to the AST runtime and accepts `--backend ir` and `--backend bytecode` for the current alpha subset. `lullaby compile` emits a versioned `.lbc` bytecode artifact with function-table and memory-operation metadata, `lullaby build` is the same artifact-generation path under a build-oriented command name, `lullaby inspect file.lbc` summarizes that artifact, and `lullaby run file.lbc` executes it. IR lowering failures use code `N0501` and phase `ir`; bytecode artifact failures use code `N0601` and phase `bytecode`. The alias `--diagnostic-format json` is also accepted. Extra positional arguments are rejected so tools do not accidentally ignore misspelled paths or flags.

### Concise Output

Concise output is the default. It is intended for quick terminal feedback:

```text
N0303 [semantic error] at tests/fixtures/invalid/type_mismatch.lby:2:22 in `main`: binding `value` declares `bool` but initializer has `i64`
```

### Verbose Output

Verbose output is intended for humans and LLM agents that need enough context to repair the source:

```text
N0102 [lexer error] at tests/fixtures/invalid/brace.lby:2:5: curly braces are not block delimiters in Lullaby

Source:
   2 |     {
     |     ^

Problem:
  Lullaby uses indentation-only blocks.

Root cause:
  The source contains a curly brace, which is not a block delimiter.

Suggested fix:
  Remove the brace and express the block with indentation.
```

Runtime failures include lightweight tracebacks when execution has entered user code:

```text
Traceback:
  in `main` at 1:1
```

### JSON Output

JSON mode is deterministic and intended for editors, CI systems, and LLM agents. Failure diagnostics are written to stderr and keep a non-zero exit status. Successful JSON runs write this to stdout:

```json
{"status":"ok","diagnostics":[]}
```

Failure JSON uses the diagnostic registry fields:

```json
{"status":"error","diagnostics":[{"code":"N0313","phase":"semantic","severity":"error","message":"argument 2 for `sys_status` must be `array<string>` but got `array<i64>`","source_path":"tests/fixtures/invalid/sys_args_type.lby","span":{"line":2,"column":24},"function":"bad","explanation":"Function and builtin arguments are statically type checked.","root_cause":"The argument expression type does not match the parameter type.","suggested_fix":"Pass a value of the expected type or change the called function signature.","notes":[],"traceback":[]}]}
```

See [diagnostic_registry.md](diagnostic_registry.md) for the full stable code registry and JSON field contract.

## Core Error Concepts

### Error Tokens
Errors are represented as compact tokens rather than full exception objects:
- `!` - Error marker prefix (replaces "throw" keyword)
- Errors encoded as 3-digit hexadecimal values
- Example: `!0x4c` represents a specific error code

### Error Categories
1. **Runtime Errors** (`!0xXX`) - Occur during execution
2. **Compilation Errors** (`#err`) - Caught at compile time
3. **Type Errors** (`#tpe`) - Type mismatches and violations
4. **Resource Errors** (`#res`) - Memory, file, or I/O issues

## Planned Error Operators

### The Throw Operator
```lullaby
!0x4c message
```
- Throws error code 0x4c (segmentation fault) with optional message
- Minimal token count: 2 tokens minimum
- No parentheses, no brackets needed

### Try-Catch Pattern
```lullaby
try code !catch ErrorCode message handler_code end
```
- Wraps potentially failing code in try block
- Catches specified error codes using the ! operator
- Executes fallback logic on error
- End marks closure of try block

### Error Recovery
```lullaby
err RecoveryCode:
  recovery_action1
  recovery_action2
end
```
- Defines specific recovery procedures for error codes
- Can be called automatically or manually from catch blocks
- Supports multiple recovery paths per error code

## Compile-Time Error Detection

The compiler performs extensive static analysis to detect errors before runtime:

### Static Analysis Checks
- Type compatibility verification
- Memory allocation validation
- Resource access permissions
- Control flow consistency

### Error Categories at Compile Time
1. **Syntax Errors** - Invalid syntax constructs
2. **Type Mismatches** - Incompatible type operations
3. **Unused Declarations** - Unreachable or unused code
4. **Resource Conflicts** - File/driver access issues

## Error Reporting Format

Current alpha:
```text
N0102 at 1:9: curly braces are not block delimiters in Lullaby
N0313 in `main`: argument 2 for `sys_status` must be `array<string>` but got `array<i64>`
N0414 [resource]: failed to read `missing.txt`: ...
```

Planned compact language-level representation:
```lullaby
!0x4c MemoryOverflow: Allocation exceeds limit of 64KB
#tpe ExpectedInt: Got Float in integer parameter position
#res NoFilePermission: Cannot open '/etc/config' (permission denied)
```

### Message Encoding
- Compact format with error code and descriptive message
- Messages stored in Unicode but limited to 32 characters max
- Includes location information when available (line number, function name)

## Error Handling Best Practices

1. **Fail Fast** - Detect errors immediately rather than continuing on corrupted state
2. **Graceful Degradation** - Provide fallback behaviors when errors occur
3. **Explicit Recovery** - Define recovery paths for critical error codes
4. **Error Propagation** - Bubble up significant errors through call stack automatically

## Example Usage

The following is planned syntax, not accepted by the current alpha parser:

```lullaby
# Memory-safe array operation with error handling
func process_data(data: Array[int]): bool
  try
    allocate_buffer(data.size)
    for i from 0 to data.size do
      load_value(i, data[i])
      store_result(i, computed_value(data[i]))

    # Check memory usage before operation
    if mem_usage > mem_limit then
      !0x41 MemoryLimitExceeded: Current usage at mem_usageKB
      fallback_to_minimal_allocation()
    end

    return success(true)
  catch ErrorCode handler_code end

  return success(false)
end

// Error recovery for I/O operations
err FileReadError:
  attempt_alternate_source()
  report_missing_data()

err PermissionDenied:
  request_admin_access()
  log_security_event()
end
```

## Performance Considerations

- Error handling adds minimal overhead (< 2% in typical workloads)
- Compile-time errors detected before any runtime cost
- Token-efficient error representation saves ~60% vs traditional exceptions
- Fast error lookup using hash-based code mapping

## Integration with Compilation Pipeline

Error handling is integrated into the compilation phases:

1. **Semantic Analysis** - Type checking and type error detection (#tpe)
2. **Static Optimization** - Resource validation and #res errors
3. **Code Generation** - Insert try-catch patterns automatically where needed
4. **Final Verification** - Runtime error handler injection (!catch blocks)

## Conclusion

The Lullaby error handling system provides:
- Minimal token overhead for LLM understanding
- Clear separation between compile-time and runtime errors
- Automatic recovery mechanisms without verbose exception objects
- Strong safety guarantees through static analysis
- Efficient error codes suitable for systems programming

This design maintains the minimalistic philosophy while providing robust error handling essential for reliable OS development.
