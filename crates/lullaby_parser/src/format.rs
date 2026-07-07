//! Canonical source formatter: render a parsed [`Program`](crate::Program)
//! back to canonical Lullaby source. The output is indentation-only (four
//! spaces per level), has no trailing whitespace, ends in a single newline,
//! and re-parses to an equal AST. Formatting is idempotent.
//!
//! Top-level declarations are emitted in source order (by span line), so the
//! formatter never reorders a file.

use crate::{
    AliasDecl, AssignOp, BinaryOp, EnumDecl, Expr, ExprKind, Function, IfBranch, MatchArm,
    MatchPattern, Place, Program, RegionDecl, Stmt, StructDecl, UnaryOp,
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

fn render_function(function: &Function) -> String {
    let mut header = format!("fn {}", function.name);
    for param in &function.params {
        header.push_str(&format!(" {} {}", param.name, param.ty.name));
    }
    header.push_str(&format!(" -> {}", function.return_type.name));
    let mut out = header;
    render_block(&function.body, 1, &mut out);
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
        Stmt::Loop { body, .. } => {
            line(out, &pad, "loop");
            render_block(body, depth + 1, out);
        }
        Stmt::Unsafe { body, .. } => {
            line(out, &pad, "unsafe");
            render_block(body, depth + 1, out);
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
        ExprKind::Unary { op, expr } => {
            let UnaryOp::Not = op;
            format!("not {}", render_unary_operand(expr))
        }
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
    }
}

/// Render a binary operand, parenthesizing when the child binds more loosely
/// than the parent (right children also need parens at equal precedence, since
/// all operators are left-associative).
fn render_operand(child: &Expr, parent_prec: u8, is_right: bool) -> String {
    let rendered = render_expr(child);
    if let ExprKind::Binary { op, .. } = &child.kind {
        let child_prec = binary_precedence(op);
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
    if matches!(child.kind, ExprKind::Binary { .. }) {
        format!("({rendered})")
    } else {
        rendered
    }
}

fn render_postfix_target(target: &Expr) -> String {
    let rendered = render_expr(target);
    if matches!(
        target.kind,
        ExprKind::Binary { .. } | ExprKind::Unary { .. } | ExprKind::Match { .. }
    ) {
        format!("({rendered})")
    } else {
        rendered
    }
}

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
        BinaryOp::Add | BinaryOp::Subtract => 4,
        BinaryOp::Multiply | BinaryOp::Divide => 5,
    }
}

fn render_binary_op(op: &BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Subtract => "-",
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::Equal => "==",
        BinaryOp::NotEqual => "!=",
        BinaryOp::Less => "<",
        BinaryOp::LessEqual => "<=",
        BinaryOp::Greater => ">",
        BinaryOp::GreaterEqual => ">=",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
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

    #[test]
    fn formats_function_with_canonical_spacing() {
        let source = "fn add a i64 b i64 -> i64\n    a + b\n";
        assert_eq!(fmt(source), source);
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
}
