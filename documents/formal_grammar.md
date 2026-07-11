# Lullaby Formal Grammar

Canonical language rules: see [core_language_rules.md](core_language_rules.md).
Earliest installable surface: see [language_surface.md](language_surface.md).

This document is the formal grammar draft for the current implemented parser. It describes accepted source structure after lexical scanning has removed comments and produced indentation tokens. It does not describe the full planned systems language.

## Scope

This grammar covers:

- `.lby` source files.
- Top-level function declarations.
- Indentation-only blocks.
- `let` bindings, assignment, return, break, continue, expressions, branches, while loops, range-for loops, and infinite loops.
- Scalar type names, concrete pointer-style type names such as `ptr_i64`, and nested `array<T>` type spelling.
- Literals, variables, calls, arrays, indexing, unary `not`, unary negative integers as `0 - expr`, arithmetic, comparison, equality, and boolean operators.

This grammar does not cover:

- Semantic requirements such as type compatibility, duplicate names, zero-argument executable `main`, valid loop-control placement, homogeneous arrays, or builtin argument types.
- Runtime behavior such as bounds checks, file I/O, memory slots, process execution, or bytecode execution.
- Planned syntax such as imports, modules, structs, traits, pattern matching, try/catch, async, streams, regions, native-code output, or user-defined generics beyond `array<T>`.

Planned syntax keywords are recognized by the lexer so the parser can reject them with `L0211` instead of accepting ambiguous partial syntax.

## Notation

- Grammar is written in EBNF-like notation.
- Quoted text such as `"fn"` is a literal token.
- `IDENT`, `NUMBER`, and `STRING` are lexer tokens.
- `NEWLINE`, `INDENT`, `DEDENT`, and `EOF` are structural tokens from the indentation scanner.
- `*` means zero or more.
- `+` means one or more.
- `?` means optional.
- Parentheses group grammar alternatives.

## Lexical Rules

Comments begin with `#` and continue to the end of the line. The lexer removes comments before parsing tokens. Blank physical lines produce no parser-level statement.

Curly braces are not block delimiters and are rejected by the lexer. Semicolons are not statement terminators and are rejected by the lexer.

```ebnf
IDENT =
    alpha_or_underscore { alpha_or_digit_or_underscore } ;

NUMBER =
      base_prefixed_integer
    | digit { digit_or_separator } [ "." digit { digit_or_separator } ] ;

base_prefixed_integer =
      ( "0x" | "0X" ) hex_digit { hex_digit | "_" }
    | ( "0b" | "0B" ) bin_digit { bin_digit | "_" }
    | ( "0o" | "0O" ) oct_digit { oct_digit | "_" } ;

digit_or_separator =
    digit | "_" ;

STRING =
    '"' { string_character } '"' ;
```

The lexer emits the entire number token as raw text (its scan includes ASCII alphanumerics, `.`, and `_`), so a base-prefixed literal such as `0xFF` arrives as one token. The parser decides the shape: a `0x`/`0X`, `0b`/`0B`, or `0o`/`0O` prefix (matched case-insensitively) yields a base-prefixed **integer** literal parsed via `i64::from_str_radix`; anything else is a decimal integer (no `.`) or an `f64` literal (a `.` marks the float). A `_` may appear between digits as a cosmetic separator — decimal `1_000_000`/`3.141_592`, or between two valid radix digits in a base-prefixed literal (`0xFF_FF`, `0b1010_0101`); the parser validates placement and strips it, rejecting a leading, trailing, doubled, prefix-adjacent, or (decimal) `.`-adjacent underscore. A base-prefixed literal is integer-only, so a `.`, empty digits after the prefix (`0x`), an out-of-radix digit (`0xG`, `0b2`, `0o8`), or an `i64` overflow is a malformed-literal error. Negative numbers are parsed as unary minus over an expression and represented in the AST as `0 - expression`, so `-0xFF` desugars to `0 - 0xFF`.

## Indentation Tokens

Blocks are represented by indentation tokens:

```ebnf
block =
    INDENT { statement NEWLINE } DEDENT ;
```

A block begins only after a grammar production explicitly expects a nested block, such as a function signature, branch header, loop header, or `else`.

## Program

```ebnf
program =
    { NEWLINE } { function_decl { NEWLINE } } EOF ;
```

Only functions are valid top-level declarations in this grammar.

## Functions

```ebnf
function_decl =
    "fn" IDENT { parameter } "->" type NEWLINE block ;

parameter =
    IDENT type ;
```

Examples:

```lullaby
fn add x i64 y i64 -> i64
    x + y

fn main -> i64
    add(40, 2)
```

Executable commands such as `lullaby run`, `lullaby compile`, and `lullaby build` require a zero-argument `main`, but that is a semantic/executable validation rule rather than grammar.

## Types

```ebnf
type =
    "void"
  | IDENT
  | "array" "<" type ">" ;
```

Implemented scalar type names are `i64`, `bool`, `string`, and `void`. Interim pointer-style names such as `ptr_i64` parse as `IDENT`. The parser accepts type names structurally; semantic validation decides which names are meaningful in the current language surface.

## Statements

```ebnf
statement =
    let_stmt
  | assignment_stmt
  | return_stmt
  | break_stmt
  | continue_stmt
  | if_stmt
  | while_stmt
  | for_stmt
  | loop_stmt
  | expr ;

let_stmt =
    "let" IDENT [ type ] "=" expr ;

assignment_stmt =
    IDENT assignment_op expr ;

assignment_op =
    "=" | "+=" | "-=" | "*=" | "/=" ;

return_stmt =
    "return" expr? ;

break_stmt =
    "break" ;

continue_stmt =
    "continue" ;
```

Statements are line-oriented. No semicolon terminator is accepted. A `let` annotation is optional only when the initializer has a concrete semantic type; empty arrays and `void` initializers cannot provide an inferred local type.

## Control Flow

```ebnf
if_stmt =
    "if" expr NEWLINE block
    { "elif" expr NEWLINE block }
    [ "else" NEWLINE block ] ;

while_stmt =
    "while" expr NEWLINE block ;

for_stmt =
    "for" IDENT "from" expr "to" expr [ "by" expr ] NEWLINE block ;

loop_stmt =
    "loop" NEWLINE block ;
```

Range-for loops are inclusive at runtime. A zero step is a runtime error, not a parse error.

## Expressions

Expression precedence, from lowest to highest:

1. inline conditional (ternary): `THEN if COND else ELSE`
2. `or`
3. `and`
4. equality and ordering: `==`, `!=`, `<`, `<=`, `>`, `>=`
5. addition and subtraction: `+`, `-`
6. multiplication, division, and remainder: `*`, `/`, `%`
7. unary: `not`, unary `-`
8. postfix indexing: `target[index]`
9. primary expressions

The inline conditional binds looser than every operator and is
right-associative, so `a + b if c else d` parses as `(a + b) if c else d` and
`x if a else y if b else z` as `x if a else (y if b else z)`. It appears
wherever a full expression is expected (a `let`/`return` value, a call argument,
an array element, an index, a parenthesized group, a closure body).

```ebnf
expr =
    conditional_expr ;

conditional_expr =
    or_expr [ "if" or_expr "else" conditional_expr ] ;

or_expr =
    and_expr { "or" and_expr } ;

and_expr =
    comparison_expr { "and" comparison_expr } ;

comparison_expr =
    additive_expr { comparison_op additive_expr } ;

comparison_op =
    "==" | "!=" | "<" | "<=" | ">" | ">=" ;

additive_expr =
    multiplicative_expr { ("+" | "-") multiplicative_expr } ;

multiplicative_expr =
    unary_expr { ("*" | "/" | "%") unary_expr } ;

unary_expr =
    "not" unary_expr
  | "-" unary_expr
  | postfix_expr ;

postfix_expr =
    primary_expr { "[" expr "]" } ;

primary_expr =
    NUMBER
  | STRING
  | "true"
  | "false"
  | call_expr
  | IDENT
  | array_literal
  | "(" expr ")" ;

call_expr =
    IDENT "(" [ expr { "," expr } ] ")" ;

array_literal =
    "[" [ expr { "," expr } ] "]" ;
```

The parser accepts empty array literals syntactically, but the semantic layer rejects them because empty-array type inference is not implemented.

## Builtins

Builtin calls use the normal call expression grammar:

```ebnf
builtin_call =
    IDENT "(" [ expr { "," expr } ] ")" ;
```

The current builtin names are documented in [language_surface.md](language_surface.md). Whether a call name is a user-defined function or a builtin is decided during semantic validation, not parsing.

## Planned Syntax Rejection

The lexer recognizes several planned keywords, including:

```text
module import package struct union trait interface class match switch try catch throw async await coroutine
```

When these tokens appear where this grammar does not support them, the parser reports `L0211` planned-syntax diagnostics.

## Grammar Versus Semantic Validation

The grammar accepts syntactically valid source. Later phases enforce:

- duplicate function and local names
- known functions and builtins
- function-call arity and argument types
- return behavior and return types
- boolean conditions
- loop-control placement
- arithmetic, comparison, logical, array, pointer, file, and system builtin type rules
- executable zero-argument `main` requirements for `run`, `compile`, and `build`

Use parser tests and AST snapshots for grammar-shape regressions. Use semantic, runtime, CLI, and release tests for full behavior.
