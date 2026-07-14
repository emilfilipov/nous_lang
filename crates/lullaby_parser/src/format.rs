//! Canonical source formatter: render a parsed [`Program`](crate::Program)
//! back to canonical Lullaby source. The output is indentation-only (four
//! spaces per level), has no trailing whitespace, ends in a single newline,
//! and re-parses to an equal AST. Formatting is idempotent.
//!
//! Top-level declarations are emitted in source order (by span line), so the
//! formatter never reorders a file.
//!
//! # Comment preservation
//!
//! Comments are not part of the AST; the lexer captures them as trivia (see
//! [`lullaby_lexer::Comment`]) and [`format_program_with_comments`] threads them
//! back into the output so `lullaby fmt` never destroys them. Placement is driven
//! entirely by source line: a full-line comment is emitted just before the first
//! construct whose source line is greater than the comment's, taking that
//! construct's indentation; a trailing (inline) comment is re-attached to the end
//! of the line whose source line matches it. Because every emitted comment is
//! re-lexed to the same trivia and re-emitted at the same relative position,
//! formatting a commented file is idempotent.

use crate::{
    AliasDecl, AssignOp, BinaryOp, EnumDecl, Expr, ExprKind, Function, IfBranch, ImplDecl,
    MatchArm, MatchPattern, MethodSig, Param, Place, Program, RegionDecl, Stmt, StructDecl,
    TraitDecl, TypeParam, UnaryOp,
};
use lullaby_lexer::Comment;

const INDENT: &str = "    ";

/// Render a whole program to canonical source. Any comments in the original
/// source are lost (this is the comment-free entry point kept for callers that
/// do not have the trivia, such as AST-only tooling). Prefer
/// [`format_program_with_comments`] for `fmt`.
pub fn format_program(program: &Program) -> String {
    format_program_with_comments(program, &[])
}

/// Render a whole program to canonical source, re-emitting `comments` at their
/// source positions so the format round-trip is comment-preserving. `comments`
/// must be in source order (as produced by [`lullaby_lexer::lex_with_comments`]).
pub fn format_program_with_comments(program: &Program, comments: &[Comment]) -> String {
    // Collect every top-level item with its source line so the original ordering
    // is preserved regardless of how the AST buckets declarations.
    let mut items: Vec<TopItem> = Vec::new();
    for alias in &program.aliases {
        items.push(TopItem::Alias(alias));
    }
    for decl in &program.structs {
        items.push(TopItem::Struct(decl));
    }
    for decl in &program.enums {
        items.push(TopItem::Enum(decl));
    }
    for decl in &program.traits {
        items.push(TopItem::Trait(decl));
    }
    for decl in &program.impls {
        items.push(TopItem::Impl(decl));
    }
    for function in &program.functions {
        items.push(TopItem::Function(function));
    }
    items.sort_by_key(TopItem::line);

    let mut emitter = Emitter::new(comments);
    for index in 0..items.len() {
        // A blank line separates consecutive top-level declarations; any leading
        // comments for the next item are flushed after this separator (attached
        // to the item, below the blank line).
        if index > 0 {
            emitter.out.push('\n');
        }
        let block_next = items
            .get(index + 1)
            .map(TopItem::line)
            .unwrap_or(usize::MAX);
        render_item(&mut emitter, &items[index], block_next);
    }
    // Any comments after the last construct (end-of-file trivia) are emitted at
    // their own indentation so nothing is dropped.
    emitter.flush_before(usize::MAX);
    emitter.finish()
}

/// A top-level declaration, kept as a borrow so the formatter can sort the mixed
/// declaration buckets by source line without cloning.
enum TopItem<'a> {
    Alias(&'a AliasDecl),
    Struct(&'a StructDecl),
    Enum(&'a EnumDecl),
    Trait(&'a TraitDecl),
    Impl(&'a ImplDecl),
    Function(&'a Function),
}

impl TopItem<'_> {
    fn line(&self) -> usize {
        match self {
            TopItem::Alias(alias) => alias.span.line,
            TopItem::Struct(decl) => decl.span.line,
            TopItem::Enum(decl) => decl.span.line,
            TopItem::Trait(decl) => decl.span.line,
            TopItem::Impl(decl) => decl.span.line,
            TopItem::Function(function) => function.span.line,
        }
    }
}

/// Accumulates canonical source while interleaving comment trivia by source line.
///
/// `comments` is in source order and consumed strictly left-to-right via `idx`.
/// Because every construct is emitted in increasing-source-line order (top-level
/// items are sorted, statements are in source order, and nested blocks lie
/// between their parent's line and the next sibling's line), a single monotonic
/// cursor keeps comment placement consistent across arbitrary nesting.
struct Emitter<'a> {
    out: String,
    comments: &'a [Comment],
    idx: usize,
}

impl<'a> Emitter<'a> {
    fn new(comments: &'a [Comment]) -> Self {
        Self {
            out: String::new(),
            comments,
            idx: 0,
        }
    }

    /// Write a single full-line comment on its own line at the block depth implied
    /// by the comment's own source indentation (four spaces per level), so a
    /// comment keeps the nesting level it was written at rather than inheriting the
    /// depth of whichever construct happens to flush it.
    fn emit_comment(&mut self, comment: &Comment) {
        self.out.push('\n');
        self.out.push_str(&indent(comment.indent / INDENT.len()));
        self.out.push_str(&comment.text);
    }

    /// Emit every not-yet-consumed comment whose source line is `< anchor`, each
    /// at its own indentation. Used before a construct: all comments that
    /// physically precede it are its leading comments.
    fn flush_before(&mut self, anchor: usize) {
        while self.idx < self.comments.len() && self.comments[self.idx].line < anchor {
            let comment = self.comments[self.idx].clone();
            self.emit_comment(&comment);
            self.idx += 1;
        }
    }

    /// Emit comments that belong to a block that is ending: those on a line
    /// `< anchor` whose depth is at least `min_depth`. Stops at the first comment
    /// shallower than the block (it belongs to an enclosing scope and is left for
    /// the following construct to place at its own, shallower depth).
    fn flush_block_end(&mut self, anchor: usize, min_depth: usize) {
        while self.idx < self.comments.len() {
            let comment = &self.comments[self.idx];
            if comment.line >= anchor || comment.indent / INDENT.len() < min_depth {
                break;
            }
            let comment = comment.clone();
            self.emit_comment(&comment);
            self.idx += 1;
        }
    }

    /// Emit the full-line comments physically above a spanless keyword line
    /// (`else`, `catch`, or a bare `return`) at depth `depth`: comments on a line
    /// `< boundary` whose depth is at most `depth`. Stops at the first comment
    /// deeper than the keyword (it belongs inside the keyword's body and is
    /// emitted after the keyword instead).
    fn flush_above_keyword(&mut self, boundary: usize, depth: usize) {
        while self.idx < self.comments.len() {
            let comment = &self.comments[self.idx];
            if comment.line >= boundary || comment.indent / INDENT.len() > depth {
                break;
            }
            let comment = comment.clone();
            self.emit_comment(&comment);
            self.idx += 1;
        }
    }

    /// Emit a construct line at `depth` whose source line is `source_line`:
    /// flush the full-line comments that precede it, write the line, then
    /// re-attach a trailing comment sitting on the same source line.
    fn emit_line(&mut self, depth: usize, source_line: usize, text: &str) {
        self.flush_before(source_line);
        self.out.push('\n');
        self.out.push_str(&indent(depth));
        self.out.push_str(text);
        self.attach_trailing(source_line);
    }

    /// Attach a trailing comment whose source line is exactly `source_line` to the
    /// end of the line just written (two spaces then the comment).
    fn attach_trailing(&mut self, source_line: usize) {
        if self.idx < self.comments.len()
            && self.comments[self.idx].trailing
            && self.comments[self.idx].line == source_line
        {
            self.out.push_str("  ");
            self.out.push_str(&self.comments[self.idx].text);
            self.idx += 1;
        }
    }

    /// Emit a keyword line with no source span of its own (`else`, `catch`, or a
    /// bare `return`). `boundary` is the source line of the first construct that
    /// follows this line's block; full-line comments physically above the keyword
    /// are flushed before it. See the module note on spanless placement.
    fn emit_spanless(&mut self, depth: usize, text: &str, boundary: usize) {
        self.flush_above_keyword(boundary, depth);
        self.out.push('\n');
        self.out.push_str(&indent(depth));
        self.out.push_str(text);
    }

    /// Strip the leading newline produced by the first emitted line and guarantee
    /// exactly one trailing newline, matching the canonical output shape.
    fn finish(mut self) -> String {
        while self.out.starts_with('\n') {
            self.out.remove(0);
        }
        while self.out.ends_with('\n') {
            self.out.pop();
        }
        self.out.push('\n');
        self.out
    }
}

fn render_item(emitter: &mut Emitter, item: &TopItem, block_next: usize) {
    match item {
        TopItem::Alias(alias) => emitter.emit_line(0, alias.span.line, &render_alias(alias)),
        TopItem::Struct(decl) => render_struct(emitter, decl, block_next),
        TopItem::Enum(decl) => render_enum(emitter, decl, block_next),
        TopItem::Trait(decl) => render_trait(emitter, decl, block_next),
        TopItem::Impl(decl) => render_impl(emitter, decl, block_next),
        TopItem::Function(function) => render_function(emitter, function, block_next),
    }
}

fn render_alias(alias: &AliasDecl) -> String {
    format!("alias {} = {}", alias.name, alias.target.name)
}

fn render_struct(emitter: &mut Emitter, decl: &StructDecl, block_next: usize) {
    let mut header = String::new();
    if decl.is_public {
        header.push_str("pub ");
    }
    header.push_str(&format!("struct {}", decl.name));
    emitter.emit_line(0, decl.span.line, &header);
    for field in &decl.fields {
        // `StructField` carries no source span, so field-level comments cannot be
        // line-anchored; they are flushed inside the struct body below.
        emitter.push_field_line(1, &format!("{} {}", field.name, field.ty.name));
    }
    // Any comments inside the struct body are re-emitted within the body so they
    // are preserved (never dropped) and the file stays idempotent.
    emitter.flush_block_end(block_next, 1);
}

fn render_enum(emitter: &mut Emitter, decl: &EnumDecl, block_next: usize) {
    let mut header = String::new();
    if decl.is_public {
        header.push_str("pub ");
    }
    header.push_str(&format!("enum {}", decl.name));
    emitter.emit_line(0, decl.span.line, &header);
    for variant in &decl.variants {
        let mut text = variant.name.clone();
        for ty in &variant.payload {
            text.push(' ');
            text.push_str(&ty.name);
        }
        emitter.push_field_line(1, &text);
    }
    emitter.flush_block_end(block_next, 1);
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

fn render_trait(emitter: &mut Emitter, decl: &TraitDecl, block_next: usize) {
    let mut header = String::new();
    if decl.is_public {
        header.push_str("pub ");
    }
    header.push_str(&format!("trait {}", decl.name));
    emitter.emit_line(0, decl.span.line, &header);
    for method in &decl.methods {
        emitter.emit_line(1, method.span.line, &render_method_sig(method));
    }
    emitter.flush_block_end(block_next, 1);
}

fn render_method_sig(method: &MethodSig) -> String {
    let mut header = format!("fn {} self", method.name);
    header.push_str(&render_params(&method.params));
    header.push_str(&format!(" -> {}", method.return_type.name));
    header
}

fn render_impl(emitter: &mut Emitter, decl: &ImplDecl, block_next: usize) {
    emitter.emit_line(
        0,
        decl.span.line,
        &format!("impl {} for {}", decl.trait_name, decl.type_name),
    );
    for index in 0..decl.methods.len() {
        let following = decl
            .methods
            .get(index + 1)
            .map(|method| method.span.line)
            .unwrap_or(block_next);
        render_impl_method(emitter, &decl.methods[index], following);
    }
    emitter.flush_block_end(block_next, 1);
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
fn render_impl_method(emitter: &mut Emitter, function: &Function, block_next: usize) {
    let mut header = format!("fn {} self", function.name);
    header.push_str(&render_params(function.params.get(1..).unwrap_or(&[])));
    header.push_str(&format!(" -> {}", function.return_type.name));
    emitter.emit_line(1, function.span.line, &header);
    render_block(emitter, &function.body, 2, block_next);
}

fn render_function(emitter: &mut Emitter, function: &Function, block_next: usize) {
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
    emitter.emit_line(0, function.span.line, &header);
    // An extern declaration is body-less; render only the signature line.
    if !function.is_extern {
        render_block(emitter, &function.body, 1, block_next);
    }
}

/// The 1-based source line a statement's header sits on, if it has one. Every
/// statement kind carries a usable span except a bare `return` (no value), which
/// keeps no span in the AST and is therefore placed by boundary (see
/// [`Emitter::emit_spanless`]).
fn stmt_line(stmt: &Stmt) -> Option<usize> {
    match stmt {
        Stmt::Let { span, .. } => Some(span.line),
        Stmt::Assign { span, .. } => Some(span.line),
        Stmt::Return(Some(expr)) => Some(expr.span.line),
        Stmt::Return(None) => None,
        Stmt::Break(span) => Some(span.line),
        Stmt::Continue(span) => Some(span.line),
        Stmt::Expr(expr) => Some(expr.span.line),
        Stmt::If { span, .. } => Some(span.line),
        Stmt::While { span, .. } => Some(span.line),
        Stmt::For { span, .. } => Some(span.line),
        Stmt::ForEach { span, .. } => Some(span.line),
        Stmt::Loop { span, .. } => Some(span.line),
        Stmt::Unsafe { span, .. } => Some(span.line),
        Stmt::Asm { span, .. } => Some(span.line),
        Stmt::Region(decl) => Some(decl.span.line),
        Stmt::Throw { span, .. } => Some(span.line),
        Stmt::Try { span, .. } => Some(span.line),
    }
}

/// The source line of the first statement in `body` that has one, used as the
/// boundary that bounds a preceding block's comment flushing.
fn first_stmt_line(body: &[Stmt]) -> Option<usize> {
    body.iter().find_map(stmt_line)
}

/// Append a block of statements, each on its own line at `depth` indentation.
/// `block_next` is the source line of the first construct that follows this whole
/// block, so trailing end-of-block comments are flushed at the correct depth.
fn render_block(emitter: &mut Emitter, body: &[Stmt], depth: usize, block_next: usize) {
    for index in 0..body.len() {
        // The next sibling with a known line bounds this statement's nested-block
        // comment flushing (and a bare `return`'s placement).
        let following = body[index + 1..]
            .iter()
            .find_map(stmt_line)
            .unwrap_or(block_next);
        render_stmt(emitter, &body[index], depth, following);
    }
    // Comments after the last statement that are indented at least to this block's
    // depth belong to it; shallower ones are left for an enclosing construct.
    emitter.flush_block_end(block_next, depth);
}

fn indent(depth: usize) -> String {
    INDENT.repeat(depth)
}

fn render_stmt(emitter: &mut Emitter, stmt: &Stmt, depth: usize, following: usize) {
    match stmt {
        Stmt::Let {
            name,
            ty,
            value,
            span,
        } => {
            let annotation = match ty {
                Some(ty) => format!(" {}", ty.name),
                None => String::new(),
            };
            render_value_stmt(
                emitter,
                depth,
                span.line,
                following,
                &format!("let {name}{annotation} = "),
                value,
            );
        }
        Stmt::Assign {
            name,
            path,
            op,
            value,
            span,
        } => {
            let target = render_place_path(name, path);
            render_value_stmt(
                emitter,
                depth,
                span.line,
                following,
                &format!("{target} {} ", render_assign_op(op)),
                value,
            );
        }
        Stmt::Return(Some(expr)) => {
            render_value_stmt(emitter, depth, expr.span.line, following, "return ", expr);
        }
        Stmt::Return(None) => emitter.emit_spanless(depth, "return", following),
        Stmt::Break(span) => emitter.emit_line(depth, span.line, "break"),
        Stmt::Continue(span) => emitter.emit_line(depth, span.line, "continue"),
        Stmt::Expr(expr) => {
            // A bare expression statement; block-expressions render multi-line.
            if let ExprKind::Match { scrutinee, arms } = &expr.kind {
                emitter.emit_line(
                    depth,
                    expr.span.line,
                    &format!("match {}", render_expr(scrutinee)),
                );
                render_match_arms(emitter, arms, depth + 1, following);
            } else {
                emitter.emit_line(depth, expr.span.line, &render_expr(expr));
            }
        }
        Stmt::If {
            branches,
            else_body,
            ..
        } => render_if(emitter, branches, else_body, depth, following),
        Stmt::While {
            condition,
            body,
            span,
        } => {
            emitter.emit_line(
                depth,
                span.line,
                &format!("while {}", render_expr(condition)),
            );
            render_block(emitter, body, depth + 1, following);
        }
        Stmt::For {
            name,
            start,
            end,
            step,
            body,
            span,
        } => {
            let mut head = format!(
                "for {name} from {} to {}",
                render_expr(start),
                render_expr(end)
            );
            if let Some(step) = step {
                head.push_str(&format!(" by {}", render_expr(step)));
            }
            emitter.emit_line(depth, span.line, &head);
            render_block(emitter, body, depth + 1, following);
        }
        Stmt::ForEach {
            name,
            iterable,
            body,
            span,
        } => {
            emitter.emit_line(
                depth,
                span.line,
                &format!("for {name} in {}", render_expr(iterable)),
            );
            render_block(emitter, body, depth + 1, following);
        }
        Stmt::Loop { body, span } => {
            emitter.emit_line(depth, span.line, "loop");
            render_block(emitter, body, depth + 1, following);
        }
        Stmt::Unsafe { body, span } => {
            emitter.emit_line(depth, span.line, "unsafe");
            render_block(emitter, body, depth + 1, following);
        }
        Stmt::Asm { bytes, span } => {
            let rendered = bytes
                .iter()
                .map(|byte| byte.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            emitter.emit_line(depth, span.line, &format!("asm {rendered}"));
        }
        Stmt::Region(decl) => emitter.emit_line(depth, decl.span.line, &render_region(decl)),
        Stmt::Throw { value, span } => {
            emitter.emit_line(depth, span.line, &format!("throw {}", render_expr(value)));
        }
        Stmt::Try {
            body,
            catch_name,
            catch_body,
            span,
        } => {
            emitter.emit_line(depth, span.line, "try");
            let catch_boundary = first_stmt_line(catch_body).unwrap_or(following);
            render_block(emitter, body, depth + 1, catch_boundary);
            emitter.emit_spanless(depth, &format!("catch {catch_name}"), catch_boundary);
            render_block(emitter, catch_body, depth + 1, following);
        }
    }
}

/// Emit a `let`/`return`/assignment header plus its value. A `match` value
/// continues on following indented lines; any other value finishes the header
/// line. `line` is the header's source line, and `block_next` is the source line
/// of the construct after this statement (bounding the match arms' comments).
fn render_value_stmt(
    emitter: &mut Emitter,
    depth: usize,
    line: usize,
    block_next: usize,
    prefix: &str,
    value: &Expr,
) {
    if let ExprKind::Match { scrutinee, arms } = &value.kind {
        emitter.emit_line(
            depth,
            line,
            &format!("{prefix}match {}", render_expr(scrutinee)),
        );
        render_match_arms(emitter, arms, depth + 1, block_next);
    } else {
        emitter.emit_line(depth, line, &format!("{prefix}{}", render_expr(value)));
    }
}

/// The source line an inline match arm sits on, if determinable (an inline arm's
/// single-expression body carries a span).
fn arm_line(arm: &MatchArm) -> Option<usize> {
    if let [Stmt::Expr(expr)] = arm.body.as_slice() {
        Some(expr.span.line)
    } else {
        first_stmt_line(&arm.body)
    }
}

fn render_match_arms(emitter: &mut Emitter, arms: &[MatchArm], depth: usize, block_next: usize) {
    for index in 0..arms.len() {
        let following = arms[index + 1..]
            .iter()
            .find_map(arm_line)
            .unwrap_or(block_next);
        let arm = &arms[index];
        let pattern = render_pattern(&arm.pattern);
        // Inline arm bodies are a single non-block expression statement.
        if let [Stmt::Expr(expr)] = arm.body.as_slice()
            && !is_block_expr(expr)
        {
            emitter.emit_line(
                depth,
                expr.span.line,
                &format!("{pattern} -> {}", render_expr(expr)),
            );
            continue;
        }
        // A block-bodied arm: the `Pattern ->` line has no dedicated span, so use
        // the first body statement's line as its boundary.
        let boundary = first_stmt_line(&arm.body).unwrap_or(following);
        emitter.emit_spanless(depth, &format!("{pattern} ->"), boundary);
        render_block(emitter, &arm.body, depth + 1, following);
    }
    emitter.flush_block_end(block_next, depth);
}

fn is_block_expr(expr: &Expr) -> bool {
    matches!(expr.kind, ExprKind::Match { .. })
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

fn render_if(
    emitter: &mut Emitter,
    branches: &[IfBranch],
    else_body: &[Stmt],
    depth: usize,
    following: usize,
) {
    for (index, branch) in branches.iter().enumerate() {
        let keyword = if index == 0 { "if" } else { "elif" };
        emitter.emit_line(
            depth,
            branch.condition.span.line,
            &format!("{keyword} {}", render_expr(&branch.condition)),
        );
        // This branch body ends where the next branch's condition begins, or at
        // the else body's first statement, or at the whole `if`'s follower.
        let branch_next = branches
            .get(index + 1)
            .map(|next| next.condition.span.line)
            .or_else(|| first_stmt_line(else_body))
            .unwrap_or(following);
        render_block(emitter, &branch.body, depth + 1, branch_next);
    }
    if !else_body.is_empty() {
        // The `else` keyword has no dedicated span; place it before the first
        // else-body statement.
        let else_boundary = first_stmt_line(else_body).unwrap_or(following);
        emitter.emit_spanless(depth, "else", else_boundary);
        render_block(emitter, else_body, depth + 1, following);
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

impl Emitter<'_> {
    /// Emit a struct/enum body line (a field or variant) that has no source span
    /// of its own. It cannot carry a trailing comment attachment, so it is written
    /// plainly at `depth`; body comments are flushed separately by the caller.
    fn push_field_line(&mut self, depth: usize, text: &str) {
        self.out.push('\n');
        self.out.push_str(&indent(depth));
        self.out.push_str(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use lullaby_lexer::{lex, lex_with_comments};

    fn fmt(source: &str) -> String {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        format_program(&program)
    }

    /// Format `source` preserving its comments, exercising the `fmt` path.
    fn fmt_commented(source: &str) -> String {
        let (tokens, comments) = lex_with_comments(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        format_program_with_comments(&program, &comments)
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

    // --- Comment preservation -------------------------------------------------

    /// Assert `fmt` preserves comments and is idempotent: formatting `source`
    /// yields `expected`, and re-formatting `expected` is a fixed point.
    fn assert_comment_roundtrip(source: &str, expected: &str) {
        let once = fmt_commented(source);
        assert_eq!(once, expected, "unexpected formatted output");
        let twice = fmt_commented(&once);
        assert_eq!(twice, expected, "comment formatting is not idempotent");
    }

    #[test]
    fn preserves_full_line_and_trailing_comments_repro() {
        // The exact reported repro: a full-line comment and a trailing comment,
        // both of which the old formatter silently deleted.
        let source = concat!(
            "fn main -> i64\n",
            "    # this is a comment\n",
            "    let x i64 = 5  # trailing comment\n",
            "    x\n",
        );
        assert_comment_roundtrip(source, source);
    }

    #[test]
    fn preserves_leading_file_and_between_declaration_comments() {
        let source = concat!(
            "# file header\n",
            "fn a -> i64\n",
            "    1\n",
            "\n",
            "# describes b\n",
            "fn b -> i64\n",
            "    2\n",
        );
        assert_comment_roundtrip(source, source);
    }

    #[test]
    fn preserves_comments_in_nested_control_flow() {
        let source = concat!(
            "fn classify n i64 -> i64\n",
            "    # negative?\n",
            "    if n < 0\n",
            "        # yes\n",
            "        return 0 - 1  # sentinel\n",
            "    # otherwise\n",
            "    0\n",
        );
        assert_comment_roundtrip(source, source);
    }

    #[test]
    fn normalizes_trailing_comment_spacing_then_is_idempotent() {
        // A single space before a trailing comment normalizes to two spaces, and
        // the result is then a fixed point (idempotent).
        let source = "fn main -> i64\n    let x i64 = 5 # note\n    x\n";
        let expected = "fn main -> i64\n    let x i64 = 5  # note\n    x\n";
        assert_comment_roundtrip(source, expected);
    }

    #[test]
    fn comment_free_output_is_unchanged_by_the_comment_path() {
        // With no comments, the comment-aware entry point produces byte-identical
        // output to the plain formatter (guards the refactor).
        let source = concat!(
            "enum Shape\n    Circle i64\n    Empty\n\n",
            "fn area s Shape -> i64\n    match s\n        Circle(r) -> r * r\n        Empty -> 0\n",
        );
        let plain = fmt(source);
        assert_eq!(fmt_commented(source), plain);
    }

    #[test]
    fn preserves_trailing_comment_on_function_header() {
        let source = "fn main -> i64  # entry point\n    0\n";
        assert_comment_roundtrip(source, source);
    }

    #[test]
    fn preserves_comments_inside_struct_and_enum_bodies() {
        // Comments inside a struct/enum body carry no per-field span, so they are
        // re-emitted within the body (at the end of it) rather than dropped, and
        // the result is idempotent. A trailing comment on the header line stays on
        // the header.
        let source = concat!(
            "struct Point  # a 2D point\n",
            "    x i64\n",
            "    y i64\n",
            "    # fields above\n",
        );
        let expected = source;
        assert_comment_roundtrip(source, expected);
    }

    #[test]
    fn preserves_end_of_body_comment_at_body_indentation() {
        // A comment at the end of a function body (no following statement) stays at
        // the body's indentation and is idempotent.
        let source = "fn main -> i64\n    let x i64 = 1\n    # done here\n    x\n";
        assert_comment_roundtrip(source, source);
    }

    #[test]
    fn else_body_leading_comment_is_preserved_and_idempotent() {
        // Documents the one known placement limitation: a full-line comment sitting
        // between an `else` keyword and its body's first statement is preserved and
        // idempotent, but attaches to the end of the preceding branch (it renders
        // just above `else`) because the `else` keyword has no source span. Comments
        // deeper inside the else body, and comments elsewhere, are placed exactly.
        let source = concat!(
            "fn f n i64 -> i64\n",
            "    if n < 0\n",
            "        0\n",
            "    else\n",
            "        # fall through\n",
            "        1\n",
        );
        let expected = concat!(
            "fn f n i64 -> i64\n",
            "    if n < 0\n",
            "        0\n",
            "        # fall through\n",
            "    else\n",
            "        1\n",
        );
        // Preserved (not dropped) and a fixed point on the already-formatted form.
        assert_comment_roundtrip(source, expected);
    }
}
