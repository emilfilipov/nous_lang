# Control Structures and Operators for Lullaby (lullaby)

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

## Overview

Control structures in Lullaby are designed to be minimal, readable, and highly efficient for both compilation and LLM generation. Unlike traditional languages with nested braces and semicolons, lullaby uses a flat indentation-based structure that reduces token consumption while maintaining clarity.

## Core Design Principles

1. **Flat Syntax**: No nesting brackets; control flow determined by indentation alone
2. **Single Statement Per Line**: Each line contains exactly one operation or expression
3. **Implicit Blocks**: Code blocks defined purely through indentation levels
4. **Declarative Flow**: Control statements prefix operations rather than wrapping them

## Current Implementation

The current Rust toolchain implements these control forms:

```lullaby
if condition
    expression_or_statement
elif other_condition
    expression_or_statement
else
    expression_or_statement
```

```lullaby
while condition
    statement
```

```lullaby
for name from start to end
    statement

for name from start to end by step
    statement
```

```lullaby
loop
    if condition
        break
    continue
```

Current rules:

- `if`, `elif`, and `while` conditions must type-check as `bool`.
- `for` ranges use inclusive `i64` start/end expressions with an optional `i64` step. The default step is `1`; runtime rejects step `0`.
- `break` and `continue` are valid only inside `while`, `for`, or `loop`.
- `let` bindings introduced inside an indented block are scoped to that block.
- Assignments update an existing binding in the nearest enclosing scope.
- Numeric compound assignment supports `+=`, `-=`, `*=`, and `/=` for `i64`.
- Boolean logic supports `and`, `or`, and unary `not`. `and` and `or` short-circuit during runtime execution.

## Control Structure Keywords

### Conditional Statements (If-Else)

Instead of: `if condition / else if condition / else with indented bodies`

Lullaby uses:
```lullaby
# Basic conditional with indentation-based blocks

if condition_true:
    execute_if_true()
else:
    execute_if_false()

# Multiple conditions without nesting braces

if cond1:
    action_a()
elif cond2:
    action_b()
elif cond3:
    action_c()
else:
    default_action()
```

**Syntax Rules:**
- Single colon (`:`) terminates the condition
- Indentation (4 spaces or 1 tab) defines block scope
- No opening/closing brackets needed
- `elif` used for additional conditions after initial if
- `else` handles final fallback case

### Loop Structures

#### For Loops
```lullaby
# Simple range iteration

for i from start to end:
    process_item(i)

# Step control (default step is 1)

for i from 0 to 100 by 5:
    step(i, 5)

# While-style loop with explicit condition

while condition_active:
    perform_action()
    update_state()

# Infinite loop with break/continue

loop:
    check_state()
    if is_done:
        break
    elif needs_retry:
        continue
```

**Loop Variants:**
- `for X from Y to Z [by step]`: Range-based iteration
- `while condition:`: Condition-checked loop
- `repeat count:` or `until condition:`: Count/exit-condition loops (alias for while)
- `loop:`: Unconditional with explicit control statements

#### Loop Optimization Features
- Automatic unrolling for small fixed iterations
- SIMD vectorization when conditions allow
- Early exit optimization via break detection

### Switch Statements

```lullaby
switch value_type:
    case type_a:
        handle_as_a(value)
    case type_b:
        handle_as_b(value)
    case type_c:
        handle_as_c(value)
    default:
        handle_unknown(value)
    end_switch
```

**Features:**
- Type-based or value-based matching
- Fall-through behavior optional (default disabled for safety)
- Pattern matching support via `case X when condition:`
- Named regions with automatic cleanup (`end_switch`)

### Exception Handling

```lullaby
try:
    risky_operation()
catch ErrorType as error:
    handle_error(error, context)
catch:
    handle_unknown_errors()
finally:
    cleanup_resources()
end_try
```

**Exception Types:**
- Explicit type specification: `catch SpecificError:`
- Generic handler: `catch:` for all exceptions
- Context binding: Variables available within catch blocks
- Optional finally block guaranteed execution

### Coroutine Support (Lightweight)

```lullaby
def coroutine generator():
    yield token1, token2
    yield token3

async def async_task(data):
    result = await data.fetch()
    return process(result)
```

**Coroutines:**
- `yield` for generator-style iteration
- `await` for asynchronous operations (single keyword per line)
- No complex coroutine syntax; flat indentation maintained
- Lightweight implementation suitable for systems programming

## Operators

### Arithmetic Operators
```lullaby
+  addition, positive increment
-  subtraction, negative decrement, negation
*  multiplication
/  division (integer or floating-point based operand types)
%  modulo (remainder)
^  exponentiation
// integer_division (truncated division, no remainder)
abs(x)       absolute value
sqrt(x)      square root
log2(x)      base-2 logarithm
```

### Comparison Operators
```lullaby
==  equal
!=  not_equal
<   less than
>   greater than
<=  less or equal
>=  greater or equal
is_none     null check (no pointer dereferencing needed)
is_defined  existence check
```

### Logical Operators
```lullaby
and    logical conjunction
or     logical disjunction
not    logical negation
xor    exclusive or
```

### Assignment Operators
```lullaby
=       standard assignment
+=      add then assign (in-place modification)
-=      subtract then assign
*=      multiply then assign
/=      divide then assign
%=      modulo then assign
^=      exponentiate then assign

// Compound operators - all single-line operations
count += 1   # Increment counter
value *= 2.5  # Scale value
ratio /= total  # Calculate ratio
```

### Bitwise Operators
```lullaby
&     bitwise and
|     bitwise or
^     bitwise xor
<<    left shift
>>    right shift
inv(x) bitwise_not (bit inversion)
popcount(x) count set bits
leading_zeros(x) count leading zeros
trailing_zeros(x) count trailing zeros
```

### String Operators
```lullaby
concat(s1, s2)  string concatenation
len(str)        length calculation
substr(start, end) substring extraction
split(str, delimiter) tokenization
join(list, separator) aggregation to string
repeat(str, n) string repetition
trim(str)       remove whitespace
pad(str, width) left/right padding
```

### Array/Collection Operators
```lullaby
len(arr)        length/count of elements
index(arr, i)   element access (automatic bounds checking)
slice(arr, start, end) range extraction
append(arr, item) add to end
prepend(arr, item) add to beginning
contains(arr, item) membership test
sort(arr, key)  ordered arrangement
find(arr, predicate) element search
filter(arr, predicate) selective collection

// Range operations
range(start, end, step) generate sequence
repeat(item, count) expand single value
```

### Type Conversion Operators
```lullaby
int(x)    convert to integer
float(x)  convert to floating-point
bool(x)   convert to boolean (non-zero becomes true)
str(x)    convert to string representation
num(x)    unify number conversion
bytes(x)  convert to byte sequence

// Type casting with explicit notation
<type>(value)  cast value to type
as <type>      safe cast with checking
```

### Special Utility Operators
```lullaby
select_from(list, predicate) best match selection
min(val1, val2, ...) minimum value
max(val1, val2, ...) maximum value
avg(values) arithmetic mean
sum(values) total addition
mode(values) most frequent occurrence
variance(values) statistical variance
stdev(values) standard deviation

// Functional operators
map(function, collection) apply to each element
reduce(function, collection, initial) aggregate reduction
fold(function, collection, left/right) directional accumulation

// Memory operations
alloc(size/type) allocate memory
store(ptr, value) replace value in allocated memory
dealloc(ptr) free allocated memory
ref(ptr) create reference copy
ptr(type) allocate pointer
swap(a, b) exchange values
duplicate(value) deep copy

// Control flow helpers
assume(cond) assert condition (compile-time check)
assert(cond) runtime assertion with error on failure
loop_forever(body) infinite loop wrapper
defer(expression) delayed execution at scope exit
```

## Syntax Examples

### Complete Example: Processing System Data
```lullaby
region data_processing allocate

    struct Dataset
        array samples
        map metadata
        num batch_size

        def load(filepath):
            return io.read_file(filepath, self.batch_size)

        def process():
            stats = self.compute_statistics()

            if is_valid(stats.mean, std_dev):
                normalized_data = normalize(self.samples, stats)
                result = analyze(normalized_data)

                if result.p_significant:
                    return true
                else:
                    log_warning("non-significant results")
                    return false

            return null

        def compute_statistics():
            mean = avg(samples)
            variance = calc_variance(samples, mean)
            std_dev = sqrt(variance)

            return Stats(mean=mean, var=variance, stdev=std_dev)

    dataset = new Dataset()
    batch_size = 1024

    try:
        dataset.load("data/input.dat")

        iteration_num = 0

        while iteration_num < max_iterations:
            if can_process(dataset.samples):
                processed = dataset.process()

                if not processed:
                    iteration_num += 5  # Skip batches

                else:
                    results.append(processed)

                    if len(results) >= target_count:
                        break
            elif is_incomplete(dataset.samples):
                partial_progress()
                continue

        end_try

    finally:
        dataset.cleanup()
```

## Design Advantages Over Traditional Languages

| Feature | Traditional (C/C++/Java/etc.) | Lullaby |
|---------|------------------------------|-----------|
| Block syntax | Nested brace-delimited blocks | Indentation only |
| Semicolons required | Yes, after each statement | No semicolons needed |
| Multiple statements per line | Common practice | Disallowed (one per line) |
| Conditional nesting | Deep nesting common | Flat structure with elif |
| Loop syntax | Various styles (`for`, `while`, etc.) | Unified indentation-based |
| Token count per statement | High (braces, semicolons, parentheses) | Minimal (only keywords needed) |

## LLM Generation Optimization

The flat structure and reduced token requirements make lullaby particularly suitable for small language models:

1. **Reduced Complexity**: No nested brace tracking required
2. **Predictable Structure**: Indentation is universally understood
3. **Clear Semantics**: Single-line statements have unambiguous meaning
4. **Lower Token Budget**: Each control statement uses 1-3 tokens vs traditional languages' 3-6 tokens

### Example Comparison

Traditional C:
```c
if (x > 0)
    y = x * 2
else if (x == 0)
    y = 0
else
    y = -x
```
(3 levels of nesting, ~15 tokens)

Lullaby:
```lullaby
if x > 0:
    y = x * 2
elif x == 0:
    y = 0
else:
    y = -x
end_if
```
(Flat structure, ~8 tokens)

## Summary

The control structures and operators in lullaby provide:
- Minimal, readable syntax optimized for LLM comprehension
- Flat indentation-based structure eliminating brace complexity
- Unified loop types reducing learning curve
- Type-safe operations preventing runtime errors
- Efficient single-line statements minimizing token usage
- Comprehensive operator set covering all common programming needs

This design enables small language models to generate correct, efficient code while maintaining human readability and ease of maintenance.
