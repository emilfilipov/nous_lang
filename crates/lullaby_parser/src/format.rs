//! Canonical source formatter: render a parsed [`Program`](crate::Program)
//! back to canonical Lullaby source. The output is indentation-only (four
//! spaces per level), has no trailing whitespace, ends in a single newline,
//! and re-parses to an equal AST. Formatting is idempotent.
//!
//! Top-level declarations are emitted in source order (by span line), so the
//! formatter never reorders a file.

use crate::{
    AliasDecl, AssignOp, BinaryOp, EnumDecl, Expr, ExprKind, Function, IfBranch, ImplDecl,
    MatchArm, MatchPattern, MethodSig, Param, Place, Program, RegionDecl, Stmt, StructDecl,
    TraitDecl, TypeParam, UnaryOp,
};

const INDENT: &str = "    ";

/// Render a whole program to canonical source.
pub fn format_program(program: &Program) -> String {
    // Collect every top-level item with its source line so the original
    // ordering is preserved regardless of how the AST buckets declarations.
    let mut items: Vec<(usize, String)> = Vec::new();
    for alias in &program.aliases {
        items.push((alias.span.line, render_alias(alias)));
    }
    for decl in &program.structs {
        items.push((decl.span.line, render_struct(decl)));
    }
    for decl in &program.enums {
        items.push((decl.span.line, render_enum(decl)));
    }
    for decl in &program.traits {
        items.push((decl.span.line, render_trait(decl)));
    }
    for decl in &program.impls {
        items.push((decl.span.line, render_impl(decl)));
    }
    for function in &program.functions {
        items.push((function.span.line, render_function(function)));
    }
    items.sort_by_key(|(line, _)| *line);

    let mut out = items
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n\n");
    out.push('\n');
    out
}

fn render_alias(alias: &AliasDecl) -> String {
    format!("alias {} = {}", alias.name, alias.target.name)
}

fn render_struct(decl: &StructDecl) -> String {
    let mut out = format!("struct {}", decl.name);
    for field in &decl.fields {
        out.push('\n');
        out.push_str(INDENT);
        out.push_str(&format!("{} {}", field.name, field.ty.name));
    }
    out
}

fn render_enum(decl: &EnumDecl) -> String {
    let mut out = format!("enum {}", decl.name);
    for variant in &decl.variants {
        out.push('\n');
        out.push_str(INDENT);
        out.push_str(&variant.name);
        for ty in &variant.payload {
            out.push(' ');
            out.push_str(&ty.name);
        }
    }
    out
}

/// Render a type-parameter list `<T, U: Show + Ord>` (no surrounding `<>` when
/// the list is empty). Bounds join with ` + `.
fn render_type_params(type_params: &[TypeParam]) -> String {
    if type_params.is_empty() {
        return String::new();
    }
    let rendered = type_params
        .iter()
        .map(|param| {
            if param.bounds.is_empty() {
                param.name.clone()
            } else {
                format!("{}: {}", param.name, param.bounds.join(" + "))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("<{rendered}>")
}

fn render_trait(decl: &TraitDecl) -> String {
    let mut out = String::new();
    if decl.is_public {
        out.push_str("pub ");
    }
    out.push_str(&format!("trait {}", decl.name));
    for method in &decl.methods {
        out.push('\n');
        out.push_str(INDENT);
        out.push_str(&render_method_sig(method));
    }
    out
}

fn render_method_sig(method: &MethodSig) -> String {
    let mut header = format!("fn {} self", method.name);
    header.push_str(&render_params(&method.params));
    header.push_str(&format!(" -> {}", method.return_type.name));
    header
}

fn render_impl(decl: &ImplDecl) -> String {
    let mut out = format!("impl {} for {}", decl.trait_name, decl.type_name);
    for method in &decl.methods {
        // Render each method with its `self` receiver untyped (source spelling)
        // and its body indented one level under the impl block.
        let rendered = render_impl_method(method);
        let indented = rendered
            .lines()
            .map(|line| format!("{INDENT}{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        out.push('\n');
        out.push_str(&indented);
    }
    out
}

/// Render a space-separated parameter list, grouping consecutive parameters that
/// share a type into the terse comma form: `a i64 b i64 c i64` renders as
/// `a, b, c i64`. This is the canonical output, so `lullaby fmt` produces (and
/// round-trips) the token-efficient grouped spelling while keeping every type
/// explicit. Returns a leading-space-prefixed string, or empty for no params.
fn render_params(params: &[Param]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < params.len() {
        let ty = &params[i].ty.name;
        let mut names = vec![params[i].name.as_str()];
        let mut j = i + 1;
        while j < params.len() && &params[j].ty.name == ty {
            names.push(params[j].name.as_str());
            j += 1;
        }
        out.push_str(&format!(" {} {}", names.join(", "), ty));
        i = j;
    }
    out
}

/// Render an impl method as `fn name self [param Type ...] -> Ret` + body. The
/// first parameter is the injected `self` receiver, which is rendered untyped to
/// round-trip with the parser.
fn render_impl_method(function: &Function) -> String {
    let mut header = format!("fn {} self", function.name);
    header.push_str(&render_params(function.params.get(1..).unwrap_or(&[])));
    header.push_str(&format!(" -> {}", function.return_type.name));
    let mut out = header;
    render_block(&function.body, 1, &mut out);
    out
}

fn render_function(function: &Function) -> String {
    let mut header = String::new();
    if function.is_public {
        header.push_str("pub ");
    }
    if function.is_async {
        header.push_str("async ");
    }
    if function.is_extern {
        header.push_str("extern ");
    }
    if function.is_export {
        header.push_str("export ");
    }
    header.push_str(&format!("fn {}", function.name));
    header.push_str(&render_type_params(&function.type_params));
    header.push_str(&render_params(&function.params));
    // An omitted (inferred) return type renders without a `->` clause, so
    // `fn f x i64` round-trips; an explicit type keeps its `-> T`.
    if function.return_type.name != crate::INFERRED_RETURN {
        header.push_str(&format!(" -> {}", function.return_type.name));
    }
    let mut out = header;
    // An extern declaration is body-less; render only the signature line.
    if !function.is_extern {
        render_block(&function.body, 1, &mut out);
    }
    out
}

/// Append a block of statements, each on its own line at `depth` indentation.
fn render_block(body: &[Stmt], depth: usize, out: &mut String) {
    for stmt in body {
        render_stmt(stmt, depth, out);
    }
}

fn indent(depth: usize) -> String {
    INDENT.repeat(depth)
}

fn render_stmt(stmt: &Stmt, depth: usize, out: &mut String) {
    let pad = indent(depth);
    match stmt {
        Stmt::Let {
            name, ty, value, ..
        } => {
            let annotation = match ty {
                Some(ty) => format!(" {}", ty.name),
                None => String::new(),
            };
            line(out, &pad, &format!("let {name}{annotation} = "));
            render_value_tail(value, depth, out);
        }
        Stmt::Assign {
            name,
            path,
            op,
            value,
            ..
        } => {
            let target = render_place_path(name, path);
            line(out, &pad, &format!("{target} {} ", render_assign_op(op)));
            render_value_tail(value, depth, out);
        }
        Stmt::Return(Some(expr)) => {
            line(out, &pad, "return ");
            render_value_tail(expr, depth, out);
        }
        Stmt::Return(None) => line(out, &pad, "return"),
        Stmt::Break(_) => line(out, &pad, "break"),
        Stmt::Continue(_) => line(out, &pad, "continue"),
        Stmt::Expr(expr) => {
            // A bare expression statement; block-expressions render multi-line.
            if is_block_expr(expr) {
                render_block_expr(expr, &pad, depth, out);
            } else {
                line(out, &pad, &render_expr(expr));
            }
        }
        Stmt::If {
            branches,
            else_body,
            ..
        } => render_if(branches, else_body, depth, out),
        Stmt::While {
            condition, body, ..
        } => {
            line(out, &pad, &format!("while {}", render_expr(condition)));
            render_block(body, depth + 1, out);
        }
        Stmt::For {
            name,
            start,
            end,
            step,
            body,
            ..
        } => {
            let mut head = format!(
                "for {name} from {} to {}",
                render_expr(start),
                render_expr(end)
            );
            if let Some(step) = step {
                head.push_str(&format!(" by {}", render_expr(step)));
            }
            line(out, &pad, &head);
            render_block(body, depth + 1, out);
        }
        Stmt::ForEach {
            name,
            iterable,
            body,
            ..
        } => {
            line(
                out,
                &pad,
                &format!("for {name} in {}", render_expr(iterable)),
            );
            render_block(body, depth + 1, out);
        }
        Stmt::Loop { body, .. } => {
            line(out, &pad, "loop");
            render_block(body, depth + 1, out);
        }
        Stmt::Unsafe { body, .. } => {
            line(out, &pad, "unsafe");
            render_block(body, depth + 1, out);
        }
        Stmt::Asm { bytes, .. } => {
            let rendered = bytes
                .iter()
                .map(|byte| byte.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            line(out, &pad, &format!("asm {rendered}"));
        }
        Stmt::Region(decl) => line(out, &pad, &render_region(decl)),
        Stmt::Throw { value, .. } => {
            line(out, &pad, &format!("throw {}", render_expr(value)));
        }
        Stmt::Try {
            body,
            catch_name,
            catch_body,
            ..
        } => {
            line(out, &pad, "try");
            render_block(body, depth + 1, out);
            line(out, &pad, &format!("catch {catch_name}"));
            render_block(catch_body, depth + 1, out);
        }
    }
}

/// Render the right-hand side of a `let`/`return`/assignment. If the value is a
/// block expression (`match`) it continues on following indented lines;
/// otherwise it finishes the current line.
fn render_value_tail(value: &Expr, depth: usize, out: &mut String) {
    if let ExprKind::Match { scrutinee, arms } = &value.kind {
        // Continue the current line with `match SCRUT`, then the arms follow on
        // their own indented lines (each `line()` prepends its own newline).
        out.push_str(&format!("match {}", render_expr(scrutinee)));
        render_match_arms(arms, depth + 1, out);
    } else {
        out.push_str(&render_expr(value));
    }
}

fn is_block_expr(expr: &Expr) -> bool {
    matches!(expr.kind, ExprKind::Match { .. })
}

fn render_block_expr(expr: &Expr, pad: &str, depth: usize, out: &mut String) {
    if let ExprKind::Match { scrutinee, arms } = &expr.kind {
        line(out, pad, &format!("match {}", render_expr(scrutinee)));
        render_match_arms(arms, depth + 1, out);
    }
}

fn render_match_arms(arms: &[MatchArm], depth: usize, out: &mut String) {
    let pad = indent(depth);
    for arm in arms {
        let pattern = render_pattern(&arm.pattern);
        // Inline arm bodies are a single expression statement.
        if let [Stmt::Expr(expr)] = arm.body.as_slice()
            && !is_block_expr(expr)
        {
            line(out, &pad, &format!("{pattern} -> {}", render_expr(expr)));
            continue;
        }
        line(out, &pad, &format!("{pattern} ->"));
        render_block(&arm.body, depth + 1, out);
    }
}

fn render_pattern(pattern: &MatchPattern) -> String {
    match pattern {
        MatchPattern::Wildcard => "_".to_string(),
        MatchPattern::Variant { name, bindings } => {
            if bindings.is_empty() {
                name.clone()
            } else {
                format!("{name}({})", bindings.join(", "))
            }
        }
    }
}

fn render_if(branches: &[IfBranch], else_body: &[Stmt], depth: usize, out: &mut String) {
    let pad = indent(depth);
    for (index, branch) in branches.iter().enumerate() {
        let keyword = if index == 0 { "if" } else { "elif" };
        line(
            out,
            &pad,
            &format!("{keyword} {}", render_expr(&branch.condition)),
        );
        render_block(&branch.body, depth + 1, out);
    }
    if !else_body.is_empty() {
        line(out, &pad, "else");
        render_block(else_body, depth + 1, out);
    }
}

fn render_region(decl: &RegionDecl) -> String {
    let mut out = format!("region {}: size={}", decl.name, decl.size);
    if let Some(align) = decl.align {
        out.push_str(&format!(", align={align}"));
    }
    out.push_str(&format!(", kind={}", decl.kind));
    out.push_str(&format!(", mutable={}", decl.mutable));
    out
}

fn render_place_path(name: &str, path: &[Place]) -> String {
    let mut out = name.to_string();
    for place in path {
        match place {
            Place::Field(field) => {
                out.push('.');
                out.push_str(field);
            }
            Place::Index(index) => {
                out.push('[');
                out.push_str(&render_expr(index));
                out.push(']');
            }
        }
    }
    out
}

fn render_assign_op(op: &AssignOp) -> &'static str {
    match op {
        AssignOp::Replace => "=",
        AssignOp::Add => "+=",
        AssignOp::Subtract => "-=",
        AssignOp::Multiply => "*=",
        AssignOp::Divide => "/=",
        AssignOp::Remainder => "%=",
    }
}

/// Append `text` at `pad` indentation as its own line.
fn line(out: &mut String, pad: &str, text: &str) {
    out.push('\n');
    out.push_str(pad);
    out.push_str(text);
}

fn render_expr(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Integer(value) => value.to_string(),
        ExprKind::Float(value) => format_float(*value),
        ExprKind::Bool(value) => value.to_string(),
        // The lexer stores string contents verbatim (no escape processing) and
        // a literal cannot contain a quote, so render the value as-is.
        ExprKind::String(value) => format!("\"{value}\""),
        // A char literal cannot contain a quote (the lexer stops at the closing
        // `'`), so render the scalar as-is between single quotes.
        ExprKind::Char(value) => format!("'{value}'"),
        ExprKind::Array(values) => {
            let items = values
                .iter()
                .map(render_expr)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{items}]")
        }
        ExprKind::Variable(name) => name.clone(),
        ExprKind::Index { target, index } => {
            format!("{}[{}]", render_postfix_target(target), render_expr(index))
        }
        ExprKind::Field { target, field } => {
            format!("{}.{field}", render_postfix_target(target))
        }
        ExprKind::Unary { op, expr } => match op {
            UnaryOp::Not => format!("not {}", render_unary_operand(expr)),
            // Bitwise NOT prints with no space, like the source spelling `~a`.
            UnaryOp::BitNot => format!("~{}", render_unary_operand(expr)),
            // Arithmetic negation prints with no space, like `-a`.
            UnaryOp::Negate => format!("-{}", render_unary_operand(expr)),
        },
        ExprKind::Binary { left, op, right } => {
            let prec = binary_precedence(op);
            format!(
                "{} {} {}",
                render_operand(left, prec, false),
                render_binary_op(op),
                render_operand(right, prec, true)
            )
        }
        ExprKind::Call { name, args } => {
            let args = args.iter().map(render_expr).collect::<Vec<_>>().join(", ");
            format!("{name}({args})")
        }
        ExprKind::StructLiteral { name, fields } => {
            let fields = fields
                .iter()
                .map(|(field, value)| format!("{field}: {}", render_expr(value)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{name}({fields})")
        }
        // A `match` used as a nested expression is rare; render its scrutinee
        // inline. Statement-position matches are handled multi-line elsewhere.
        ExprKind::Match { scrutinee, .. } => {
            format!("match {}", render_expr(scrutinee))
        }
        ExprKind::Await { expr } => {
            format!("await {}", render_unary_operand(expr))
        }
        // Postfix `?` binds tighter than binary/unary operators, so a compound
        // operand is parenthesized (`(a + b)?`) while a call/field/index/variable
        // operand renders directly (`f()?`, `x?`). Chained `x??` renders as-is
        // because the inner `Try` is not one of the parenthesized forms.
        ExprKind::Try(inner) => {
            format!("{}?", render_postfix_target(inner))
        }
        // Inline closure literal `fn <name type ...> -> <body>`. Parameters render
        // as `name type` pairs (the top-level `fn` shape); the single-expression
        // body renders inline after `->`. The body re-parses correctly because a
        // closure body is `parse_conditional()`, which stops at a `,`/`)`/newline.
        ExprKind::Closure { params, body, .. } => {
            let mut out = String::from("fn");
            for param in params {
                out.push_str(&format!(" {} {}", param.name, param.ty.name));
            }
            out.push_str(&format!(" -> {}", render_expr(body)));
            out
        }
        // Inline conditional `THEN if COND else ELSE`. `then`/`cond` are
        // parenthesized when they are themselves ternaries so the source
        // re-parses with the same structure; the `else` branch renders bare so a
        // right-associative `x if a else y if b else z` chain round-trips.
        ExprKind::Conditional {
            cond,
            then_branch,
            else_branch,
        } => {
            format!(
                "{} if {} else {}",
                render_ternary_branch(then_branch),
                render_ternary_branch(cond),
                render_expr(else_branch),
            )
        }
        // Membership `VALUE in COLLECTION`. Both operands are parenthesized when
        // they are themselves low-precedence (`in`/ternary) so the source
        // re-parses with the same structure.
        ExprKind::In { value, collection } => {
            format!(
                "{} in {}",
                render_ternary_branch(value),
                render_ternary_branch(collection),
            )
        }
        // String slice `target[start:end]`; either bound may be empty.
        ExprKind::Slice { target, start, end } => {
            let start = start.as_deref().map(render_expr).unwrap_or_default();
            let end = end.as_deref().map(render_expr).unwrap_or_default();
            format!("{}[{start}:{end}]", render_postfix_target(target))
        }
    }
}

/// Render a low-precedence operand (a `then`/`cond` of a ternary, or either side
/// of `in`), parenthesizing a nested ternary or `in` so it does not merge into
/// the surrounding expression on re-parse.
fn render_ternary_branch(child: &Expr) -> String {
    let rendered = render_expr(child);
    if matches!(
        child.kind,
        ExprKind::Conditional { .. } | ExprKind::In { .. }
    ) {
        format!("({rendered})")
    } else {
        rendered
    }
}

/// Render a binary operand, parenthesizing when the child binds more loosely
/// than the parent (right children also need parens at equal precedence, since
/// all operators are left-associative).
fn render_operand(child: &Expr, parent_prec: u8, is_right: bool) -> String {
    let rendered = render_expr(child);
    // Precedence of the child on the shared binary scale: a ternary is the
    // loosest form (0), `in` sits at comparison level (3). Parenthesize only
    // when the child binds as loosely or (on a right operand) equally, so no
    // redundant parentheses are emitted (e.g. `x in a and y in b` stays flat).
    let child_prec = match &child.kind {
        ExprKind::Conditional { .. } => Some(0),
        ExprKind::In { .. } => Some(3),
        ExprKind::Binary { op, .. } => Some(binary_precedence(op)),
        _ => None,
    };
    if let Some(child_prec) = child_prec {
        let needs = if is_right {
            child_prec <= parent_prec
        } else {
            child_prec < parent_prec
        };
        if needs {
            return format!("({rendered})");
        }
    }
    rendered
}

fn render_unary_operand(child: &Expr) -> String {
    let rendered = render_expr(child);
    if matches!(
        child.kind,
        ExprKind::Binary { .. } | ExprKind::Conditional { .. } | ExprKind::In { .. }
    ) {
        format!("({rendered})")
    } else {
        rendered
    }
}

fn render_postfix_target(target: &Expr) -> String {
    let rendered = render_expr(target);
    if matches!(
        target.kind,
        ExprKind::Binary { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Match { .. }
            | ExprKind::Await { .. }
            | ExprKind::Conditional { .. }
            | ExprKind::In { .. }
    ) {
        format!("({rendered})")
    } else {
        rendered
    }
}

/// Must mirror the parser's `peek_binary_op` precedence so the formatter
/// parenthesizes exactly where the grammar disambiguates.
fn binary_precedence(op: &BinaryOp) -> u8 {
    match op {
        BinaryOp::Or => 1,
        BinaryOp::And => 2,
        BinaryOp::Equal
        | BinaryOp::NotEqual
        | BinaryOp::Less
        | BinaryOp::LessEqual
        | BinaryOp::Greater
        | BinaryOp::GreaterEqual => 3,
        BinaryOp::BitOr => 4,
        BinaryOp::BitXor => 5,
        BinaryOp::BitAnd => 6,
        BinaryOp::Shl | BinaryOp::Shr => 7,
        BinaryOp::Add | BinaryOp::Subtract => 8,
        BinaryOp::Multiply | BinaryOp::Divide | BinaryOp::Remainder => 9,
    }
}

fn render_binary_op(op: &BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Subtract => "-",
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::Remainder => "%",
        BinaryOp::Equal => "==",
        BinaryOp::NotEqual => "!=",
        BinaryOp::Less => "<",
        BinaryOp::LessEqual => "<=",
        BinaryOp::Greater => ">",
        BinaryOp::GreaterEqual => ">=",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::BitXor => "^",
        BinaryOp::Shl => "<<",
        BinaryOp::Shr => ">>",
    }
}

/// Render an `f64` literal so it always keeps a decimal point (so it re-parses
/// as a float, not an integer).
fn format_float(value: f64) -> String {
    let text = value.to_string();
    if text.contains('.') || text.contains('e') || text.contains("inf") || text.contains("NaN") {
        text
    } else {
        format!("{text}.0")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use lullaby_lexer::lex;

    fn fmt(source: &str) -> String {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        format_program(&program)
    }

    /// Read a named fixture from `tests/fixtures/valid/<name>.lby`.
    fn read_fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/valid")
            .join(format!("{name}.lby"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    /// Assert the formatter is idempotent and stable on a fixture: parse the
    /// source, format once (`f1`), re-parse `f1` and format again (`f2`), and
    /// require `f1 == f2`. Also confirms `f1` re-lexes and re-parses cleanly
    /// (a round-trip guard).
    fn assert_fixture_idempotent(name: &str) {
        let source = read_fixture(name);
        let tokens = lex(&source).unwrap_or_else(|e| panic!("lex {name}: {e:?}"));
        let program = parse(&tokens).unwrap_or_else(|e| panic!("parse {name}: {e:?}"));
        let f1 = format_program(&program);

        let tokens2 = lex(&f1).unwrap_or_else(|e| panic!("re-lex formatted {name}: {e:?}"));
        let program2 =
            parse(&tokens2).unwrap_or_else(|e| panic!("re-parse formatted {name}: {e:?}"));
        let f2 = format_program(&program2);

        assert_eq!(f1, f2, "formatter not idempotent on fixture {name}");
    }

    #[test]
    fn formats_function_with_canonical_spacing() {
        // Consecutive same-type parameters group into the comma form, and the
        // grouped spelling round-trips.
        assert_eq!(
            fmt("fn add a i64 b i64 -> i64\n    a + b\n"),
            "fn add a, b i64 -> i64\n    a + b\n"
        );
        let grouped = "fn add a, b i64 -> i64\n    a + b\n";
        assert_eq!(fmt(grouped), grouped);
    }

    #[test]
    fn groups_and_ungroups_same_type_parameters() {
        // A run of same-type params groups; a differently-typed param breaks the
        // run, and the following same-type run starts a new group.
        assert_eq!(
            fmt("fn f x f64 y f64 z f64 label string a i64 b i64\n    x\n"),
            "fn f x, y, z f64 label string a, b i64\n    x\n"
        );
        // A single parameter is left ungrouped (no trailing comma).
        assert_eq!(fmt("fn g n i64\n    n\n"), "fn g n i64\n    n\n");
    }

    #[test]
    fn interpolation_desugars_to_concatenation() {
        // `fmt` normalizes string interpolation to explicit `to_string`/`+`
        // concatenation (it is parse-time sugar, not a distinct AST node).
        assert_eq!(
            fmt("fn f n i64 -> string\n    \"n=${n}\"\n"),
            "fn f n i64 -> string\n    \"n=\" + to_string(n)\n"
        );
    }

    #[test]
    fn formats_inferred_return_without_arrow() {
        // A function with no `-> T` clause round-trips without one (the return
        // type is inferred); an explicit return type keeps its clause.
        let inferred = "fn add a, b i64\n    a + b\n";
        assert_eq!(fmt(inferred), inferred);
        let explicit = "fn add a, b i64 -> i64\n    a + b\n";
        assert_eq!(fmt(explicit), explicit);
    }

    #[test]
    fn formats_asm_statement_canonically() {
        // An `asm` block renders as `asm b0, b1, ...` and is idempotent.
        let source = "fn main -> i64\n    unsafe\n        asm 72, 199, 192, 42, 0, 0, 0\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn formats_inline_conditional() {
        // A ternary in tail position renders as `THEN if COND else ELSE`.
        assert_eq!(
            fmt("fn f a, b i64 c bool -> i64\n    a if c else b\n"),
            "fn f a, b i64 c bool -> i64\n    a if c else b\n"
        );
        // A ternary binds looser than every binary operator, so it is
        // parenthesized as a binary operand.
        assert_eq!(
            fmt("fn f a, b i64 c bool -> i64\n    (a if c else b) + 1\n"),
            "fn f a, b i64 c bool -> i64\n    (a if c else b) + 1\n"
        );
        // The right-associative `else` chain keeps no redundant parentheses.
        assert_eq!(
            fmt("fn f a, b i64 c bool -> i64\n    1 if c else (2 if a > b else 3)\n"),
            "fn f a, b i64 c bool -> i64\n    1 if c else 2 if a > b else 3\n"
        );
    }

    #[test]
    fn conditional_fixture_is_idempotent() {
        assert_fixture_idempotent("run_conditional");
    }

    #[test]
    fn formats_membership_operator() {
        // `in` renders bare in tail position.
        assert_eq!(
            fmt("fn f c char -> bool\n    c in \"aeiou\"\n"),
            "fn f c char -> bool\n    c in \"aeiou\"\n"
        );
        // `in` binds tighter than `and`, so its operands need no parentheses.
        assert_eq!(
            fmt("fn f c char -> bool\n    c in \"ab\" and c in \"bc\"\n"),
            "fn f c char -> bool\n    c in \"ab\" and c in \"bc\"\n"
        );
    }

    #[test]
    fn in_operator_fixture_is_idempotent() {
        assert_fixture_idempotent("run_in_operator");
    }

    #[test]
    fn formats_string_slice() {
        // All four slice shapes round-trip with the terse `[start:end]` syntax.
        for src in [
            "fn f s string -> string\n    s[1:3]\n",
            "fn f s string -> string\n    s[2:]\n",
            "fn f s string -> string\n    s[:4]\n",
            "fn f s string -> string\n    s[:]\n",
        ] {
            assert_eq!(fmt(src), src);
        }
    }

    #[test]
    fn string_slice_fixture_is_idempotent() {
        assert_fixture_idempotent("run_string_slice");
    }

    #[test]
    fn preserves_precedence_without_redundant_parens() {
        assert_eq!(
            fmt("fn main -> i64\n    1 + 2 * 3\n"),
            "fn main -> i64\n    1 + 2 * 3\n"
        );
        assert_eq!(
            fmt("fn main -> i64\n    (1 + 2) * 3\n"),
            "fn main -> i64\n    (1 + 2) * 3\n"
        );
    }

    #[test]
    fn keeps_parens_for_right_associative_grouping() {
        assert_eq!(
            fmt("fn main -> i64\n    10 - (2 - 1)\n"),
            "fn main -> i64\n    10 - (2 - 1)\n"
        );
    }

    #[test]
    fn formats_struct_enum_and_match() {
        let source = concat!(
            "enum Shape\n    Circle i64\n    Empty\n\n",
            "fn area s Shape -> i64\n    match s\n        Circle(r) -> r * r\n        Empty -> 0\n",
        );
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn formats_generic_function_header() {
        let source = concat!(
            "fn choose<T, U> pick bool a T b U -> T\n    a\n\n",
            "fn identity<T> x T -> T\n    x\n",
        );
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn formats_if_elif_else_and_for() {
        let source = concat!(
            "fn classify n i64 -> i64\n",
            "    if n < 0\n        0 - 1\n    elif n == 0\n        0\n    else\n        1\n",
        );
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn is_idempotent_and_reparses_over_all_valid_fixtures() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/valid");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("valid fixtures dir") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("lby") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read fixture");
            let Ok(tokens) = lex(&source) else { continue };
            let Ok(program) = parse(&tokens) else {
                continue;
            };
            let once = format_program(&program);
            // The formatted output must itself parse.
            let tokens2 = lex(&once).unwrap_or_else(|_| panic!("re-lex {}", path.display()));
            let program2 =
                parse(&tokens2).unwrap_or_else(|_| panic!("re-parse {}", path.display()));
            // And formatting must be idempotent.
            let twice = format_program(&program2);
            assert_eq!(once, twice, "not idempotent: {}", path.display());
            checked += 1;
        }
        assert!(checked >= 10, "expected many fixtures, checked {checked}");
    }

    // Per-construct idempotency + round-trip guards over representative real
    // fixtures. Each names the constructs it exercises so a regression points at
    // the relevant language feature. All listed fixtures are single top-level
    // files (no module loader required).

    #[test]
    fn idempotent_arithmetic() {
        // arithmetic / operator precedence / parenthesization.
        assert_fixture_idempotent("run_arithmetic");
    }

    #[test]
    fn idempotent_logic() {
        // boolean logic (and / or / not), comparisons.
        assert_fixture_idempotent("run_logic");
    }

    #[test]
    fn idempotent_if_elif_else() {
        // if / elif / else branching.
        assert_fixture_idempotent("branch");
    }

    #[test]
    fn idempotent_while_loop() {
        // while loops.
        assert_fixture_idempotent("run_while");
    }

    #[test]
    fn idempotent_loop() {
        // infinite `loop` + break/continue.
        assert_fixture_idempotent("run_loop");
    }

    #[test]
    fn idempotent_for() {
        // `for ... from ... to ... by ...` counted loops.
        assert_fixture_idempotent("run_for_step");
    }

    #[test]
    fn idempotent_arrays() {
        // array literals + indexing + mutation.
        assert_fixture_idempotent("run_array");
    }

    #[test]
    fn idempotent_named_struct() {
        // struct declarations, struct literals, field access.
        assert_fixture_idempotent("run_named_struct");
    }

    #[test]
    fn idempotent_enum_and_match() {
        // enums + match arms (inline and block-bodied).
        assert_fixture_idempotent("run_match");
    }

    #[test]
    fn idempotent_enum() {
        // enum declarations with payloads.
        assert_fixture_idempotent("run_enum");
    }

    #[test]
    fn idempotent_option_result() {
        // option / result flavored control flow.
        assert_fixture_idempotent("run_option_result");
    }

    #[test]
    fn idempotent_list() {
        // list-style collection usage.
        assert_fixture_idempotent("run_list");
    }

    #[test]
    fn idempotent_map() {
        // map-style collection usage.
        assert_fixture_idempotent("run_map");
    }

    #[test]
    fn idempotent_generics() {
        // generic functions `<T>` and generic type params.
        assert_fixture_idempotent("run_generics");
    }

    #[test]
    fn idempotent_traits() {
        // traits (`trait` / `impl`), bounded params `<T: Bound>`.
        assert_fixture_idempotent("run_traits");
    }

    #[test]
    fn idempotent_first_class_fn() {
        // first-class functions.
        assert_fixture_idempotent("run_first_class_fn");
    }

    #[test]
    fn idempotent_methods() {
        // methods via impl blocks (self receiver).
        assert_fixture_idempotent("run_methods");
    }

    #[test]
    fn idempotent_compose() {
        // composition of many constructs.
        assert_fixture_idempotent("run_compose");
    }

    #[test]
    fn idempotent_showcase() {
        // broad showcase exercising many features together.
        assert_fixture_idempotent("run_showcase");
    }
}
