# Lullaby Alpha 1 Formal Grammar

Canonical language rules: see [core_language_rules.md](core_language_rules.md).
Frozen Alpha 1 surface: see [alpha1_language_surface.md](alpha1_language_surface.md).

This document is the formal grammar draft for the current implemented Alpha 1 parser. It describes accepted source structure after lexical scanning has removed comments and produced indentation tokens. It does not describe the full planned systems language.

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
    digit { digit } ;

STRING =
    '"' { string_character } '"' ;
```

The current lexer emits integer number text only. Negative numbers are parsed as unary minus over an expression and represented in the AST as `0 - expression`.

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

Only functions are valid top-level declarations in Alpha 1.

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

1. `or`
2. `and`
3. equality and ordering: `==`, `!=`, `<`, `<=`, `>`, `>=`
4. addition and subtraction: `+`, `-`
5. multiplication and division: `*`, `/`
6. unary: `not`, unary `-`
7. postfix indexing: `target[index]`
8. primary expressions

```ebnf
expr =
    or_expr ;

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
    unary_expr { ("*" | "/") unary_expr } ;

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

The parser accepts empty array literals syntactically, but the Alpha 1 semantic layer rejects them because empty-array type inference is not implemented.

## Builtins

Builtin calls use the normal call expression grammar:

```ebnf
builtin_call =
    IDENT "(" [ expr { "," expr } ] ")" ;
```

The current Alpha 1 builtin names are documented in [alpha1_language_surface.md](alpha1_language_surface.md). Whether a call name is a user-defined function or a builtin is decided during semantic validation, not parsing.

## Planned Syntax Rejection

The lexer recognizes several planned keywords, including:

```text
module import package struct union trait interface class match switch try catch throw async await coroutine
```

When these tokens appear where Alpha 1 grammar does not support them, the parser reports `L0211` planned-syntax diagnostics.

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
