use super::*;
use crate::number_literal::parse_radix_literal;
use lullaby_lexer::lex;

#[test]
fn parses_function_with_expression_return() {
    let tokens = lex("fn add x i64 y i64 -> i64\n    x + y\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    assert_eq!(program.functions.len(), 1);
    assert_eq!(program.functions[0].name, "add");
    assert_eq!(program.functions[0].params.len(), 2);
    assert_eq!(program.functions[0].return_type.name, "i64");
}

/// Extract the single expression that is the body of a one-line
/// `fn main -> i64` (used to inspect parsed operator structure).
fn body_expr(source: &str) -> Expr {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    match &program.functions[0].body[0] {
        Stmt::Expr(expr) => expr.clone(),
        other => panic!("expected an expression statement, got {other:?}"),
    }
}

fn as_binary(expr: &Expr) -> (BinaryOp, &Expr, &Expr) {
    match &expr.kind {
        ExprKind::Binary { left, op, right } => (*op, left, right),
        other => panic!("expected a binary expression, got {other:?}"),
    }
}

#[test]
fn parses_bitwise_operators() {
    let expr = body_expr("fn main -> i64\n    5 & 3\n");
    let (op, _, _) = as_binary(&expr);
    assert_eq!(op, BinaryOp::BitAnd);

    let expr = body_expr("fn main -> i64\n    ~5\n");
    assert!(matches!(
        expr.kind,
        ExprKind::Unary {
            op: UnaryOp::BitNot,
            ..
        }
    ));
}

#[test]
fn bitwise_precedence_matches_c_like_ordering() {
    // Shifts bind tighter than `&`, which binds tighter than `^`, which
    // binds tighter than `|`. `a | b ^ c & d << e` == `a | (b ^ (c & (d << e)))`.
    let expr = body_expr("fn main -> i64\n    1 | 2 ^ 3 & 4 << 5\n");
    let (top, _, or_right) = as_binary(&expr);
    assert_eq!(top, BinaryOp::BitOr, "top of tree is `|`");
    let (xor_op, _, xor_right) = as_binary(or_right);
    assert_eq!(xor_op, BinaryOp::BitXor, "right of `|` is `^`");
    let (and_op, _, and_right) = as_binary(xor_right);
    assert_eq!(and_op, BinaryOp::BitAnd, "right of `^` is `&`");
    let (shl_op, _, _) = as_binary(and_right);
    assert_eq!(shl_op, BinaryOp::Shl, "right of `&` is `<<`");
}

#[test]
fn shift_binds_below_additive_and_bitwise_below_comparison() {
    // `a + b << c` == `(a + b) << c` (additive tighter than shift).
    let expr = body_expr("fn main -> i64\n    1 + 2 << 3\n");
    let (top, left, _) = as_binary(&expr);
    assert_eq!(top, BinaryOp::Shl);
    assert_eq!(as_binary(left).0, BinaryOp::Add);

    // `a & b == c` == `(a & b) == c` (bitwise tighter than comparison).
    let expr = body_expr("fn main -> i64\n    1 & 2 == 3\n");
    let (top, left, _) = as_binary(&expr);
    assert_eq!(top, BinaryOp::Equal);
    assert_eq!(as_binary(left).0, BinaryOp::BitAnd);

    // Unary `~` binds tighter than `&`: `~a & b` == `(~a) & b`.
    let expr = body_expr("fn main -> i64\n    ~1 & 2\n");
    let (top, left, _) = as_binary(&expr);
    assert_eq!(top, BinaryOp::BitAnd);
    assert!(matches!(
        left.kind,
        ExprKind::Unary {
            op: UnaryOp::BitNot,
            ..
        }
    ));
}

#[test]
fn bitwise_operators_format_idempotently() {
    // The formatter must render the new operators and re-parse to the same
    // canonical text (idempotency), parenthesizing only where precedence
    // requires it.
    let source = "fn main -> i64\n    1 | 2 ^ 3 & 4 << 5 >> 6\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let once = format_program(&program);
    let reparsed = parse(&lex(&once).expect("lex")).expect("parse");
    let twice = format_program(&reparsed);
    assert_eq!(once, twice, "formatter is idempotent");
    assert!(
        once.contains("1 | 2 ^ 3 & 4 << 5 >> 6"),
        "no spurious parens for right-descending precedence chain: {once}"
    );

    // `~` renders with no space and parenthesizes a binary operand.
    let source = "fn main -> i64\n    ~5 & 3\n";
    let program = parse(&lex(source).expect("lex")).expect("parse");
    let out = format_program(&program);
    assert!(out.contains("~5 & 3"), "renders unary bitwise not: {out}");
}

#[test]
fn parses_void_function() {
    let tokens = lex("fn main -> void\n    return\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    assert_eq!(program.functions[0].return_type.name, "void");
}

#[test]
fn parses_extern_function_without_body() {
    let source = "extern fn llabs x i64 -> i64\n\nfn main -> i64\n    llabs(-7)\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let extern_fn = &program.functions[0];
    assert_eq!(extern_fn.name, "llabs");
    assert!(extern_fn.is_extern, "extern flag set");
    assert!(extern_fn.body.is_empty(), "extern declaration has no body");
    assert_eq!(extern_fn.params.len(), 1);
    assert_eq!(extern_fn.return_type.name, "i64");
    // The extern signature round-trips through the canonical formatter.
    let formatted = format_program(&program);
    assert!(
        formatted.contains("extern fn llabs x i64 -> i64"),
        "formatter renders extern signature: {formatted}"
    );
}

#[test]
fn parses_export_function_with_body() {
    let source = "export fn add_seven x i64 -> i64\n    x + 7\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let export_fn = &program.functions[0];
    assert_eq!(export_fn.name, "add_seven");
    assert!(export_fn.is_export, "export flag set");
    assert!(!export_fn.is_extern, "export is not extern");
    assert!(!export_fn.body.is_empty(), "export declaration has a body");
    // The export marker round-trips through the canonical formatter.
    let formatted = format_program(&program);
    assert!(
        formatted.contains("export fn add_seven x i64 -> i64"),
        "formatter renders export signature: {formatted}"
    );
}

#[test]
fn rejects_export_combined_with_extern() {
    let source = "export extern fn f x i64 -> i64\n";
    let tokens = lex(source).expect("lex");
    let diagnostics = parse(&tokens).expect_err("combining export and extern is rejected");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0201"),
        "expected L0201: {diagnostics:?}"
    );
}

/// The single expression statement of `main`'s body (a one-statement fn).
fn only_expr(source: &str) -> Expr {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    match &program.functions[0].body[0] {
        Stmt::Expr(expr) => expr.clone(),
        other => panic!("expected an expression statement, got {other:?}"),
    }
}

#[test]
fn parses_postfix_try_on_call() {
    // `f()?` applies `?` to the call result.
    let expr = only_expr("fn main -> result<i64, string>\n    f()?\n");
    match expr.kind {
        ExprKind::Try(inner) => {
            assert!(
                matches!(inner.kind, ExprKind::Call { .. }),
                "operand is a call"
            );
        }
        other => panic!("expected Try, got {other:?}"),
    }
}

#[test]
fn parses_chained_try() {
    // `x??` is a `Try` of a `Try` (left-to-right postfix application).
    let expr = only_expr("fn main -> option<i64>\n    x??\n");
    match expr.kind {
        ExprKind::Try(outer) => match outer.kind {
            ExprKind::Try(inner) => {
                assert!(
                    matches!(inner.kind, ExprKind::Variable(_)),
                    "innermost is a variable"
                );
            }
            other => panic!("expected nested Try, got {other:?}"),
        },
        other => panic!("expected Try, got {other:?}"),
    }
}

#[test]
fn try_binds_tighter_than_binary() {
    // `a + b?` parses as `a + (b?)`, so the `?` applies only to `b`.
    let expr = only_expr("fn main -> result<i64, string>\n    a + b?\n");
    match expr.kind {
        ExprKind::Binary { right, op, .. } => {
            assert_eq!(op, BinaryOp::Add);
            assert!(
                matches!(right.kind, ExprKind::Try(_)),
                "right operand is `b?`"
            );
        }
        other => panic!("expected Binary, got {other:?}"),
    }
}

#[test]
fn parses_try_inside_call_argument() {
    // `f(g()?)` places a `Try` in the argument position; nested `?` works.
    let expr = only_expr("fn main -> result<i64, string>\n    f(g()?)\n");
    match expr.kind {
        ExprKind::Call { name, args } => {
            assert_eq!(name, "f");
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::Try(_)), "arg is `g()?`");
        }
        other => panic!("expected Call, got {other:?}"),
    }
}

#[test]
fn formats_try_operator_round_trip() {
    // The formatter renders `expr?` and is idempotent: a compound operand is
    // parenthesized, a call/variable operand is not, and `x??` stays `x??`.
    let source = concat!(
        "fn main -> result<i64, string>\n",
        "    let a i64 = f()?\n",
        "    let b i64 = g()??\n",
        "    ok(a + b)\n",
    );
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let once = format_program(&program);
    assert!(once.contains("f()?"), "renders call?: {once}");
    assert!(once.contains("g()??"), "renders chained ??: {once}");
    // Idempotent: re-parsing and re-formatting yields the same text.
    let tokens2 = lex(&once).expect("re-lex");
    let program2 = parse(&tokens2).expect("re-parse");
    assert_eq!(once, format_program(&program2), "formatter is idempotent");
}

#[test]
fn parses_imports_and_pub_visibility() {
    let source = concat!(
        "import geometry\n",
        "import util\n\n",
        "pub struct Point\n    x i64\n    y i64\n\n",
        "pub fn dot a Point b Point -> i64\n    a.x * b.x\n\n",
        "fn helper n i64 -> i64\n    n\n",
    );
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert_eq!(program.imports, vec!["geometry", "util"]);
    assert!(program.structs[0].is_public);
    assert!(program.functions[0].is_public);
    assert!(!program.functions[1].is_public);
}

#[test]
fn nested_generic_type_closes_across_shift_token() {
    // `option<array<i64>>` and deeper nesting lex the trailing `>>`/`>>>` as
    // shift tokens; the type parser must split them to close each generic.
    let source = concat!(
        "fn f a option<array<i64>> b option<option<option<i64>>> -> void\n",
        "    return\n",
    );
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let params = &program.functions[0].params;
    assert_eq!(params[0].ty.name, "option<array<i64>>");
    assert_eq!(params[1].ty.name, "option<option<option<i64>>>");
}

#[test]
fn parses_option_and_result_generic_types() {
    let source = concat!(
        "fn f a option<i64> b result<i64, string> c option<array<i64>> -> void\n",
        "    return\n",
    );
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let params = &program.functions[0].params;
    assert_eq!(params[0].ty.name, "option<i64>");
    assert_eq!(params[1].ty.name, "result<i64, string>");
    assert_eq!(params[2].ty.name, "option<array<i64>>");
}

#[test]
fn generic_args_splits_nesting_aware() {
    assert_eq!(
        TypeRef::new("option<i64>").option_element(),
        Some(TypeRef::new("i64"))
    );
    assert_eq!(
        TypeRef::new("result<i64, string>").result_args(),
        Some((TypeRef::new("i64"), TypeRef::new("string")))
    );
    assert_eq!(
        TypeRef::new("result<array<i64>, string>").generic_args("result"),
        Some(vec![TypeRef::new("array<i64>"), TypeRef::new("string")])
    );
    assert_eq!(
        TypeRef::new("option<array<i64>>").option_element(),
        Some(TypeRef::new("array<i64>"))
    );
    // Canonical spelling round-trips through the shared formatter.
    assert_eq!(
        generic_type("result", &[TypeRef::new("i64"), TypeRef::new("string")]).name,
        "result<i64, string>"
    );
}

#[test]
fn parses_let_and_call_expression() {
    let tokens = lex("fn main -> i64\n    let value i64 = add(1, 2)\n    value\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    assert_eq!(program.functions[0].body.len(), 2);
}

#[test]
fn parses_inferred_let_binding() {
    let tokens = lex("fn main -> i64\n    let value = add(1, 2)\n    value\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Let { ty, .. } = &program.functions[0].body[0] else {
        panic!("expected let statement");
    };
    assert_eq!(ty, &None);
}

#[test]
fn parses_flat_io_and_system_builtin_calls() {
    let source = "fn main -> i64\n    write_file(\"target/parser.txt\", \"alpha\")\n    let text string = read_file(\"target/parser.txt\")\n    sys_status(\"rustc\", [\"--version\"])\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert_eq!(program.functions[0].body.len(), 3);
    assert!(matches!(program.functions[0].body[0], Stmt::Expr(_)));
}

#[test]
fn parses_try_catch_and_throw() {
    let source = "fn main -> void\n    try\n        throw \"boom\"\n    catch e\n        warn(e)\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Try {
        catch_name, body, ..
    } = &program.functions[0].body[0]
    else {
        panic!("expected try/catch");
    };
    assert_eq!(catch_name, "e");
    assert!(matches!(body[0], Stmt::Throw { .. }));
}

#[test]
fn parses_match_expression_with_bindings_and_wildcard() {
    let source = concat!(
        "enum Shape\n    Circle f64\n    Rect f64 f64\n    Empty\n\n",
        "fn area s Shape -> f64\n",
        "    match s\n",
        "        Circle(r) -> 3.14 * r * r\n",
        "        Rect(w, h) -> w * h\n",
        "        _ -> 0.0\n",
    );
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Expr(Expr {
        kind: ExprKind::Match { arms, .. },
        ..
    }) = &program.functions[0].body[0]
    else {
        panic!("expected match expression statement");
    };
    assert_eq!(arms.len(), 3);
    assert_eq!(
        arms[0].pattern,
        MatchPattern::Variant {
            name: "Circle".to_string(),
            bindings: vec!["r".to_string()],
        }
    );
    assert_eq!(
        arms[1].pattern,
        MatchPattern::Variant {
            name: "Rect".to_string(),
            bindings: vec!["w".to_string(), "h".to_string()],
        }
    );
    assert_eq!(arms[2].pattern, MatchPattern::Wildcard);
}

#[test]
fn parses_match_arm_with_indented_block_body() {
    let source = concat!(
        "enum Opt\n    Some i64\n    None\n\n",
        "fn unwrap o Opt -> i64\n",
        "    match o\n",
        "        Some(v) ->\n",
        "            let doubled i64 = v * 2\n",
        "            doubled\n",
        "        None -> 0\n",
    );
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Expr(Expr {
        kind: ExprKind::Match { arms, .. },
        ..
    }) = &program.functions[0].body[0]
    else {
        panic!("expected match expression statement");
    };
    assert_eq!(arms[0].body.len(), 2);
    assert!(matches!(arms[0].body[0], Stmt::Let { .. }));
}

#[test]
fn parses_match_expression_in_let_position() {
    // A `match` on the right of `=` parses as the `let` binding's value: the
    // multi-line arm block is consumed by the value parser, and the statement
    // after the arms (`x`) parses as the next statement, not a leftover.
    let source = concat!(
        "enum P\n    A\n    B\n\n",
        "fn f p P -> i64\n",
        "    let x i64 = match p\n",
        "        A -> 1\n",
        "        B -> 2\n",
        "    x\n",
    );
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let body = &program.functions[0].body;
    let Stmt::Let { value, .. } = &body[0] else {
        panic!("expected let binding, got {:?}", body[0]);
    };
    let ExprKind::Match { arms, .. } = &value.kind else {
        panic!("expected the let value to be a match expression");
    };
    assert_eq!(arms.len(), 2);
    // The trailing `x` is a separate statement, proving the match consumed its
    // own arm block and its closing dedent.
    assert!(matches!(&body[1], Stmt::Expr(_)));
}

#[test]
fn parses_float_literal() {
    let source = "fn main -> f64\n    2.5\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Expr(expr) = &program.functions[0].body[0] else {
        panic!("expected expression statement");
    };
    assert!(matches!(expr.kind, ExprKind::Float(value) if (value - 2.5).abs() < 1e-9));
}

#[test]
fn typed_integer_suffix_desugars_to_conversion_call() {
    // `100i32` becomes `to_i32(100)`; the plain `i64`/`f64` suffixes stay
    // literals.
    let tokens = lex("fn main -> i32\n    100i32\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Expr(expr) = &program.functions[0].body[0] else {
        panic!("expected expression statement");
    };
    let ExprKind::Call { name, args } = &expr.kind else {
        panic!("expected a conversion call, got {:?}", expr.kind);
    };
    assert_eq!(name, "to_i32");
    assert!(matches!(args[0].kind, ExprKind::Integer(100)));

    // A hex body with an unsigned suffix: `0xFFu16` -> `to_u16(255)`.
    let tokens = lex("fn main -> u16\n    0xFFu16\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Expr(expr) = &program.functions[0].body[0] else {
        panic!("expected expression statement");
    };
    let ExprKind::Call { name, args } = &expr.kind else {
        panic!("expected a conversion call");
    };
    assert_eq!(name, "to_u16");
    assert!(matches!(args[0].kind, ExprKind::Integer(255)));

    // `42i64` stays a plain integer (i64 is the default width).
    let tokens = lex("fn main -> i64\n    42i64\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Expr(expr) = &program.functions[0].body[0] else {
        panic!("expected expression statement");
    };
    assert!(matches!(expr.kind, ExprKind::Integer(42)));
}

#[test]
fn typed_float_suffix_desugars_and_defaults_stay_literals() {
    // `2.5f32` -> `to_f32(2.5)`.
    let tokens = lex("fn main -> f32\n    2.5f32\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Expr(expr) = &program.functions[0].body[0] else {
        panic!("expected expression statement");
    };
    let ExprKind::Call { name, args } = &expr.kind else {
        panic!("expected a conversion call");
    };
    assert_eq!(name, "to_f32");
    assert!(matches!(args[0].kind, ExprKind::Float(value) if (value - 2.5).abs() < 1e-9));

    // A hex body is never read as an `f32` suffix: `0xABF32` is a hex number.
    let tokens = lex("fn main -> i64\n    0xABF32\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Expr(expr) = &program.functions[0].body[0] else {
        panic!("expected expression statement");
    };
    assert!(matches!(expr.kind, ExprKind::Integer(0xABF32)));
}

#[test]
fn out_of_range_typed_literal_is_rejected() {
    // 256 does not fit i8; the parser rejects the literal.
    let tokens = lex("fn main -> i8\n    256i8\n").expect("lex");
    assert!(parse(&tokens).is_err(), "256i8 must be rejected");
    // A decimal point is invalid with an integer suffix.
    let tokens = lex("fn main -> i32\n    1.5i32\n").expect("lex");
    assert!(parse(&tokens).is_err(), "1.5i32 must be rejected");
    // A `u64` literal is writable up to i64::MAX; larger values must use
    // `to_u64` (their i64 cell would be negative, with no literal form).
    let tokens = lex("fn main -> u64\n    9223372036854775807u64\n").expect("lex");
    assert!(parse(&tokens).is_ok(), "i64::MAX as u64 must be accepted");
    let tokens = lex("fn main -> u64\n    9223372036854775808u64\n").expect("lex");
    assert!(
        parse(&tokens).is_err(),
        "a u64 literal above i64::MAX must be rejected"
    );
}

#[test]
fn parses_digit_separators_in_integer_and_float_literals() {
    let tokens = lex("fn main -> i64\n    1_000_000\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Expr(expr) = &program.functions[0].body[0] else {
        panic!("expected expression statement");
    };
    assert!(matches!(expr.kind, ExprKind::Integer(1_000_000)));

    let tokens = lex("fn main -> f64\n    1_234.567_8\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Expr(expr) = &program.functions[0].body[0] else {
        panic!("expected expression statement");
    };
    assert!(matches!(expr.kind, ExprKind::Float(value) if (value - 1_234.567_8).abs() < 1e-9));
}

#[test]
fn rejects_misplaced_digit_separators() {
    for bad in ["1__000", "1_000_", "3._14", "3_.14"] {
        let source = format!("fn main -> i64\n    {bad}\n");
        let tokens = lex(&source).expect("lex");
        assert!(
            parse(&tokens).is_err(),
            "expected `{bad}` to be rejected as a malformed literal"
        );
    }
}

#[test]
fn parses_base_prefixed_integer_literals() {
    for (source, expected) in [
        ("0xFF", 255i64),
        ("0b1010", 10),
        ("0o17", 15),
        ("0xFF_FF", 65535),
        ("0b1010_0101", 165),
        ("0XdeadBEEF", 0xdead_beef),
    ] {
        let program =
            parse(&lex(&format!("fn main -> i64\n    {source}\n")).expect("lex")).expect("parse");
        let Stmt::Expr(expr) = &program.functions[0].body[0] else {
            panic!("expected expression statement for `{source}`");
        };
        assert!(
            matches!(expr.kind, ExprKind::Integer(value) if value == expected),
            "expected `{source}` to parse as {expected}"
        );
    }
}

#[test]
fn rejects_malformed_base_prefixed_literals() {
    for bad in ["0x", "0xG", "0b2", "0o8", "0x1.5", "0xF__F", "0x_F", "0xF_"] {
        let source = format!("fn main -> i64\n    {bad}\n");
        let tokens = lex(&source).expect("lex");
        assert!(
            parse(&tokens).is_err(),
            "expected `{bad}` to be rejected as a malformed literal"
        );
    }
}

#[test]
fn parses_radix_literal_helper() {
    assert_eq!(parse_radix_literal("0xFF"), Some(255));
    assert_eq!(parse_radix_literal("0b1010"), Some(10));
    assert_eq!(parse_radix_literal("0o17"), Some(15));
    assert_eq!(parse_radix_literal("0xFF_FF"), Some(65535));
    assert_eq!(parse_radix_literal("0x"), None);
    assert_eq!(parse_radix_literal("0xG"), None);
    assert_eq!(parse_radix_literal("0b2"), None);
    assert_eq!(parse_radix_literal("0o8"), None);
    assert_eq!(parse_radix_literal("0xF__F"), None);
    // A plain decimal literal is not a base-prefixed literal.
    assert_eq!(parse_radix_literal("42"), None);
}

#[test]
fn normalizes_number_literals() {
    assert_eq!(normalize_number_literal("42").as_deref(), Some("42"));
    assert_eq!(normalize_number_literal("1_000").as_deref(), Some("1000"));
    assert_eq!(
        normalize_number_literal("3.141_592").as_deref(),
        Some("3.141592")
    );
    assert_eq!(normalize_number_literal("_1").as_deref(), None);
    assert_eq!(normalize_number_literal("1_").as_deref(), None);
    assert_eq!(normalize_number_literal("1__0").as_deref(), None);
}

#[test]
fn parses_type_alias_declaration() {
    let source = "alias Count = i64\nalias Numbers = array<i64>\n\nfn main -> Count\n    0\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert_eq!(program.aliases.len(), 2);
    assert_eq!(program.aliases[0].name, "Count");
    assert_eq!(program.aliases[0].target.name, "i64");
    assert_eq!(program.aliases[1].target.name, "array<i64>");
}

#[test]
fn parses_asm_statement() {
    let source = "fn main -> i64\n    unsafe\n        asm 72, 199, 192, 42, 0, 0, 0\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Unsafe { body, .. } = &program.functions[0].body[0] else {
        panic!("expected unsafe block");
    };
    let Stmt::Asm { bytes, .. } = &body[0] else {
        panic!("expected asm statement");
    };
    assert_eq!(bytes, &vec![72, 199, 192, 42, 0, 0, 0]);
}

#[test]
fn parses_region_declaration() {
    let source =
        "fn main -> i64\n    region pool: size=4096, align=16, kind=static, mutable=true\n    0\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Region(decl) = &program.functions[0].body[0] else {
        panic!("expected region declaration");
    };
    assert_eq!(decl.name, "pool");
    assert_eq!(decl.size, 4096);
    assert_eq!(decl.align, Some(16));
    assert_eq!(decl.kind, "static");
    assert!(decl.mutable);
}

#[test]
fn parses_unsafe_block_and_reference_types() {
    let source = "fn main -> i64\n    let h rc<i64> = rc_new(1)\n    let p ptr_i64 = alloc(2)\n    unsafe\n        let v i64 = ptr_read(p)\n    rc_get(h)\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert!(matches!(program.functions[0].body[2], Stmt::Unsafe { .. }));
    // `rc<i64>` parses as a generic reference type.
    let Stmt::Let { ty: Some(ty), .. } = &program.functions[0].body[0] else {
        panic!("expected typed let");
    };
    assert_eq!(ty.rc_target().expect("rc target").name, "i64");
}

#[test]
fn parses_assignment_and_while_loop() {
    let source = "fn main -> i64\n    let x i64 = 0\n    while x < 3\n        x += 1\n    x\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert_eq!(program.functions[0].body.len(), 3);
    assert!(matches!(program.functions[0].body[1], Stmt::While { .. }));
}

#[test]
fn parses_loop_break_and_continue() {
    let source = "fn main -> void\n    loop\n        continue\n        break\n    return\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert!(matches!(program.functions[0].body[0], Stmt::Loop { .. }));
}

#[test]
fn parses_for_loop() {
    let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    total\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert!(matches!(program.functions[0].body[1], Stmt::For { .. }));
}

#[test]
fn parses_for_loop_with_step() {
    let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 5 by 2\n        total += i\n    total\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert!(matches!(
        program.functions[0].body[1],
        Stmt::For { step: Some(_), .. }
    ));
}

#[test]
fn parses_generic_type_parameters() {
    let source = "fn choose<T, U> pick bool a T b U -> T\n    a\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert_eq!(
        program.functions[0].type_params,
        vec![TypeParam::new("T"), TypeParam::new("U")]
    );
    // A type-parameter name is spelled as an ordinary `TypeRef`.
    assert_eq!(program.functions[0].params[1].ty.name, "T");
    assert_eq!(program.functions[0].return_type.name, "T");
}

#[test]
fn parses_trait_impl_and_bounded_type_param() {
    let source = concat!(
        "trait Show\n",
        "    fn show self -> string\n\n",
        "struct Point\n",
        "    x i64\n\n",
        "impl Show for Point\n",
        "    fn show self -> string\n",
        "        to_string(self.x)\n\n",
        "fn describe<T: Show> v T -> string\n",
        "    v.show()\n",
    );
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert_eq!(program.traits.len(), 1);
    assert_eq!(program.traits[0].name, "Show");
    assert_eq!(program.traits[0].methods[0].name, "show");
    assert_eq!(program.impls.len(), 1);
    assert_eq!(program.impls[0].trait_name, "Show");
    assert_eq!(program.impls[0].type_name, "Point");
    // The impl method injects an untyped `self` with the implementing type.
    assert_eq!(program.impls[0].methods[0].params[0].name, "self");
    assert_eq!(program.impls[0].methods[0].params[0].ty.name, "Point");
    // The bounded type parameter records its trait bound.
    let describe = program
        .functions
        .iter()
        .find(|f| f.name == "describe")
        .expect("describe");
    assert_eq!(describe.type_params[0].name, "T");
    assert_eq!(describe.type_params[0].bounds, vec!["Show".to_string()]);
}

#[test]
fn non_generic_function_has_empty_type_params() {
    let source = "fn add a i64 b i64 -> i64\n    a + b\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert!(program.functions[0].type_params.is_empty());
}

#[test]
fn rejects_duplicate_type_parameter() {
    let tokens = lex("fn dup<T, T> a T -> T\n    a\n").expect("lex");
    let diagnostics = parse(&tokens).expect_err("parse should fail");
    assert!(diagnostics.iter().any(|d| d.code == "L0394"));
}

#[test]
fn rejects_type_parameter_that_shadows_builtin() {
    let tokens = lex("fn bad<i64> a i64 -> i64\n    a\n").expect("lex");
    let diagnostics = parse(&tokens).expect_err("parse should fail");
    assert!(diagnostics.iter().any(|d| d.code == "L0394"));
}

#[test]
fn parses_logical_expressions() {
    let source = "fn main -> bool\n    not false and true or false\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert!(matches!(program.functions[0].body[0], Stmt::Expr(_)));
}

#[test]
fn parses_array_literal_and_index() {
    let source = "fn main -> i64\n    let values array<i64> = [1, 2, 3]\n    values[1]\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert_eq!(program.functions[0].body.len(), 2);
}

#[test]
fn requires_indented_function_body() {
    let tokens = lex("fn main -> void\nreturn\n").expect("lex");
    let diagnostics = parse(&tokens).expect_err("parse should fail");
    assert_eq!(diagnostics[0].code, "L0205");
}

#[test]
fn rejects_planned_top_level_syntax() {
    // `module` remains a planned keyword rejected with L0211; `import` and
    // `pub` are now accepted (see `parses_imports_and_pub_visibility`).
    let tokens = lex("module demo\nfn main -> i64\n    1\n").expect("lex");
    let diagnostics = parse(&tokens).expect_err("parse should fail");
    assert_eq!(diagnostics[0].code, "L0211");
    assert!(diagnostics[0].message.contains("module"));
}

#[test]
fn rejects_struct_declaration_inside_function_body() {
    // Struct declarations are top-level only.
    let tokens = lex("fn main -> i64\n    struct Point\n        x i64\n    1\n").expect("lex");
    assert!(parse(&tokens).is_err());
}

#[test]
fn parses_struct_field_assignment() {
    let source = "struct Point\n    x i64\n\nfn main -> i64\n    let p Point = Point(1)\n    p.x = 5\n    p.x\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Assign { name, path, .. } = &program.functions[0].body[1] else {
        panic!("expected field assignment");
    };
    assert_eq!(name, "p");
    assert_eq!(path, &vec![Place::Field("x".to_string())]);
}

#[test]
fn parses_array_element_assignment() {
    let source = "fn main -> i64\n    let a array<i64> = [1, 2, 3]\n    a[1] = 9\n    a[1]\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Assign { name, path, .. } = &program.functions[0].body[1] else {
        panic!("expected array element assignment");
    };
    assert_eq!(name, "a");
    assert!(matches!(path.as_slice(), [Place::Index(_)]));
}

#[test]
fn desugars_method_call_to_ufcs_call() {
    // `recv.name(args)` parses to `name(recv, args)`; plain `recv.name`
    // stays field access.
    let source = "fn main -> i64\n    p.scaled(2)\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Expr(Expr {
        kind: ExprKind::Call { name, args },
        ..
    }) = &program.functions[0].body[0]
    else {
        panic!("expected method call desugared to a call");
    };
    assert_eq!(name, "scaled");
    assert_eq!(args.len(), 2);
    assert!(matches!(&args[0].kind, ExprKind::Variable(v) if v == "p"));
    assert!(matches!(&args[1].kind, ExprKind::Integer(2)));
}

#[test]
fn parses_enum_declaration_with_unit_and_payload_variants() {
    let source =
        "enum Shape\n    Circle f64\n    Rect f64 f64\n    Empty\n\nfn main -> i64\n    0\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert_eq!(program.enums.len(), 1);
    assert_eq!(program.enums[0].name, "Shape");
    assert_eq!(program.enums[0].variants.len(), 3);
    assert_eq!(program.enums[0].variants[0].name, "Circle");
    assert_eq!(program.enums[0].variants[0].payload.len(), 1);
    assert_eq!(program.enums[0].variants[1].payload.len(), 2);
    assert!(program.enums[0].variants[2].payload.is_empty());
}

#[test]
fn parses_struct_declaration_and_field_access() {
    let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(3, 4)\n    p.x\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    assert_eq!(program.structs.len(), 1);
    assert_eq!(program.structs[0].name, "Point");
    assert_eq!(program.structs[0].fields.len(), 2);
    assert_eq!(program.structs[0].fields[0].name, "x");
}

#[test]
fn parses_closure_literal_in_let_value() {
    // `fn x i64 -> x + n` parses to a `Closure` node with one typed param and
    // an expression body; the top-level `fn main` declaration is unaffected.
    let source = "fn main -> i64\n    let n i64 = 10\n    let f fn(i64) -> i64 = fn x i64 -> x + n\n    f(5)\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Let { value, .. } = &program.functions[0].body[1] else {
        panic!("expected the `let f` binding");
    };
    let ExprKind::Closure { id, params, body } = &value.kind else {
        panic!("expected a closure literal, got {:?}", value.kind);
    };
    assert_eq!(*id, 0, "the first closure literal gets id 0");
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].name, "x");
    assert_eq!(params[0].ty.name, "i64");
    assert!(matches!(body.kind, ExprKind::Binary { .. }));
}

#[test]
fn parses_closure_literal_as_call_argument() {
    // A closure body stops at the argument-separating `,`, so
    // `apply(fn x i64 -> x + 1, 5)` parses as a two-argument call whose first
    // argument is a closure.
    let source = "fn apply f fn(i64) -> i64 v i64 -> i64\n    f(v)\n\nfn main -> i64\n    apply(fn x i64 -> x + 1, 5)\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let Stmt::Expr(expr) = &program.functions[1].body[0] else {
        panic!("expected the `apply(...)` expression");
    };
    let ExprKind::Call { name, args } = &expr.kind else {
        panic!("expected a call, got {:?}", expr.kind);
    };
    assert_eq!(name, "apply");
    assert_eq!(args.len(), 2);
    assert!(matches!(args[0].kind, ExprKind::Closure { .. }));
    assert!(matches!(args[1].kind, ExprKind::Integer(5)));
}

#[test]
fn closure_literals_get_distinct_monotonic_ids() {
    // Two closure literals in the same program get distinct, monotonic ids,
    // which key each backend's closure-body table.
    let source = "fn main -> i64\n    let f fn(i64) -> i64 = fn x i64 -> x + 1\n    let g fn(i64) -> i64 = fn y i64 -> y + 2\n    f(0) + g(0)\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let mut ids = Vec::new();
    for stmt in &program.functions[0].body {
        if let Stmt::Let { value, .. } = stmt
            && let ExprKind::Closure { id, .. } = &value.kind
        {
            ids.push(*id);
        }
    }
    assert_eq!(ids, vec![0, 1]);
}

#[test]
fn malformed_closure_missing_arrow_is_rejected() {
    // A closure literal without `->` is a parser diagnostic, not a panic.
    let source = "fn main -> i64\n    let f fn(i64) -> i64 = fn x i64 x + 1\n    f(0)\n";
    let tokens = lex(source).expect("lex");
    assert!(
        parse(&tokens).is_err(),
        "a closure missing `->` must be rejected"
    );
}
