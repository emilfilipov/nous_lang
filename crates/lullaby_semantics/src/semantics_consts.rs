//! Compile-time evaluation of named constants and reference folding.
//!
//! A `const NAME type = <expr>` declaration binds a name to a value computed
//! entirely at compile time. This module evaluates every constant's initializer
//! as a *constant expression* (literals plus arithmetic/logical/bitwise/
//! comparison/unary operators over literals and other already-defined
//! constants), type-checks the result against the declared type, and then folds
//! every reference to a constant in the program into the resulting literal.
//!
//! Folding runs before the type checker validates function bodies, so the
//! checker — and every backend after it (AST/IR/bytecode interpreters, native,
//! WASM) — only ever sees ordinary literals and needs zero `const` awareness.
//! This keeps the whole feature confined to the frontend/semantic layer.
//!
//! Constant-expression rules enforced here mirror the language's value model and
//! the type checker's operand rules:
//! - `i64` arithmetic wraps on overflow; `/` and `%` by zero are rejected.
//! - `f64` arithmetic follows IEEE-754 (`%` is integer-only).
//! - `+` concatenates `string`/`string` and `string`/`char`.
//! - ordering (`< <= > >=`) is limited to `i64`/`f64`/`char`; equality
//!   (`== !=`) requires both operands to share a type.
//! - bitwise/shift operators require `i64` operands; a shift count is masked to
//!   its low 6 bits (matching the interpreters and native backend).
//!
//! Diagnostics:
//! - `L0450` — the initializer is not a valid constant expression (references a
//!   non-constant name, calls a function, uses a non-const construct, mixes
//!   operand types, or divides by zero).
//! - `L0451` — the evaluated value's type does not match the declared type.
//! - `L0452` — a constant is defined in terms of itself (cyclic reference).
//! - `L0453` — a constant name is duplicated or collides with another top-level
//!   declaration.

use std::collections::{HashMap, HashSet};

use lullaby_diagnostics::Span;
use lullaby_parser::{
    BinaryOp, ConstDecl, Expr, ExprKind, MatchPattern, Place, Program, Stmt, TypeRef, UnaryOp,
};

use super::SemanticDiagnostic;

/// A fully-evaluated constant value. Constant initializers only ever produce the
/// literal value kinds source can spell directly (a bare integer literal is
/// `i64`, a float literal `f64`), so this covers the complete set.
#[derive(Debug, Clone, PartialEq)]
enum ConstValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Char(char),
}

impl ConstValue {
    /// The canonical type-name spelling of this value, used to check it against
    /// a constant's declared type and to fold references at the right type.
    fn type_name(&self) -> &'static str {
        match self {
            ConstValue::Int(_) => "i64",
            ConstValue::Float(_) => "f64",
            ConstValue::Bool(_) => "bool",
            ConstValue::Str(_) => "string",
            ConstValue::Char(_) => "char",
        }
    }

    /// The literal expression this value folds into at a reference site. The
    /// reference's own span is reused so downstream diagnostics keep pointing at
    /// the use site.
    fn to_expr(&self, span: Span) -> Expr {
        let kind = match self {
            ConstValue::Int(value) => ExprKind::Integer(*value),
            ConstValue::Float(value) => ExprKind::Float(*value),
            ConstValue::Bool(value) => ExprKind::Bool(*value),
            ConstValue::Str(value) => ExprKind::String(value.clone()),
            ConstValue::Char(value) => ExprKind::Char(*value),
        };
        Expr { kind, span }
    }
}

/// Evaluate every constant in `program`, then fold all references to constants
/// into their literal values (mutating `program`). Returns the declared type of
/// every constant (a safety net so the checker can still type a reference to a
/// constant whose value failed to evaluate, avoiding cascade noise) plus any
/// diagnostics produced while evaluating.
pub(crate) fn resolve_and_fold_consts(
    program: &mut Program,
) -> (
    HashMap<String, TypeRef>,
    HashMap<String, i64>,
    Vec<SemanticDiagnostic>,
) {
    let mut evaluator = Evaluator::new(program);
    let (values, types, diagnostics) = evaluator.evaluate_all();
    // The integer-valued constants, exposed so the array-extent pass can resolve
    // a named extent `array<T, SIZE>` to its literal value. Folding never rewrites
    // text inside a type spelling, so the extent pass resolves these names itself.
    let int_values: HashMap<String, i64> = values
        .iter()
        .filter_map(|(name, value)| match value {
            ConstValue::Int(n) => Some((name.clone(), *n)),
            _ => None,
        })
        .collect();
    if !values.is_empty() {
        let folder = Folder { values: &values };
        folder.fold_program(program);
    }
    (types, int_values, diagnostics)
}

struct Evaluator<'a> {
    /// Constant name -> its declaration. Populated only with the first
    /// declaration of each name (a later duplicate is reported and dropped).
    by_name: HashMap<String, &'a ConstDecl>,
    /// Memoized evaluation results: `None` marks a constant that failed to
    /// evaluate (so its diagnostic is emitted once and references never re-run).
    done: HashMap<String, Option<ConstValue>>,
    /// The DFS stack of constants currently being evaluated, for cycle
    /// detection.
    visiting: HashSet<String>,
    diagnostics: Vec<SemanticDiagnostic>,
}

impl<'a> Evaluator<'a> {
    fn new(program: &'a Program) -> Self {
        let mut by_name: HashMap<String, &ConstDecl> = HashMap::new();
        let mut diagnostics = Vec::new();

        // Names already claimed by other top-level declarations. A constant may
        // not collide with a function/struct/enum/variant, matching the flat,
        // no-shadowing top-level namespace.
        let mut reserved: HashSet<String> = HashSet::new();
        for function in &program.functions {
            reserved.insert(function.name.clone());
        }
        for decl in &program.structs {
            reserved.insert(decl.name.clone());
        }
        for decl in &program.enums {
            reserved.insert(decl.name.clone());
            for variant in &decl.variants {
                reserved.insert(variant.name.clone());
            }
        }

        for decl in &program.consts {
            if by_name.contains_key(&decl.name) {
                diagnostics.push(SemanticDiagnostic::at(
                    "L0453",
                    format!("duplicate constant `{}`", decl.name),
                    None,
                    decl.span,
                ));
                continue;
            }
            if reserved.contains(&decl.name) {
                diagnostics.push(SemanticDiagnostic::at(
                    "L0453",
                    format!(
                        "constant `{}` collides with another top-level declaration of the same name",
                        decl.name
                    ),
                    None,
                    decl.span,
                ));
                continue;
            }
            by_name.insert(decl.name.clone(), decl);
        }

        Self {
            by_name,
            done: HashMap::new(),
            visiting: HashSet::new(),
            diagnostics,
        }
    }

    /// Evaluate every constant, returning the fold table (only cleanly-evaluated,
    /// correctly-typed constants), the declared-type table (every constant), and
    /// all diagnostics.
    fn evaluate_all(
        &mut self,
    ) -> (
        HashMap<String, ConstValue>,
        HashMap<String, TypeRef>,
        Vec<SemanticDiagnostic>,
    ) {
        let mut types: HashMap<String, TypeRef> = HashMap::new();
        // Evaluate in source order for deterministic diagnostics.
        let ordered = self.ordered_names();

        for name in &ordered {
            types.insert(name.clone(), self.by_name[name].ty.clone());
            self.resolve(name);
        }

        let mut values: HashMap<String, ConstValue> = HashMap::new();
        for name in &ordered {
            if let Some(Some(value)) = self.done.get(name) {
                values.insert(name.clone(), value.clone());
            }
        }
        (values, types, std::mem::take(&mut self.diagnostics))
    }

    /// The constant names in source (declaration) order.
    fn ordered_names(&self) -> Vec<String> {
        // `by_name` values borrow the program's declarations; recover order from
        // their spans, which are strictly increasing in source order.
        let mut decls: Vec<&ConstDecl> = self.by_name.values().copied().collect();
        decls.sort_by_key(|decl| (decl.span.line, decl.span.column));
        decls.into_iter().map(|decl| decl.name.clone()).collect()
    }

    /// Evaluate a single constant by name (memoized, cycle-detecting).
    fn resolve(&mut self, name: &str) -> Option<ConstValue> {
        if let Some(cached) = self.done.get(name) {
            return cached.clone();
        }
        if self.visiting.contains(name) {
            // The cycle closes here: `name` is referenced while still being
            // evaluated further up the stack.
            let span = self.by_name[name].span;
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0452",
                format!("constant `{name}` is defined in terms of itself (cyclic reference)"),
                None,
                span,
            ));
            return None;
        }

        self.visiting.insert(name.to_string());
        let decl = self.by_name[name];
        let raw = self.eval_expr(&decl.value);
        let typed = match raw {
            Some(value) => {
                if value.type_name() == decl.ty.name {
                    Some(value)
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0451",
                        format!(
                            "constant `{}` declares `{}` but its value has `{}`",
                            decl.name,
                            decl.ty.name,
                            value.type_name()
                        ),
                        None,
                        decl.value.span,
                    ));
                    None
                }
            }
            None => None,
        };
        self.visiting.remove(name);
        self.done.insert(name.to_string(), typed.clone());
        typed
    }

    /// Evaluate a constant-expression. Any non-constant construct, unknown
    /// reference, type error, or division by zero yields `None` after recording
    /// a diagnostic.
    fn eval_expr(&mut self, expr: &Expr) -> Option<ConstValue> {
        match &expr.kind {
            ExprKind::Integer(value) => Some(ConstValue::Int(*value)),
            ExprKind::Float(value) => Some(ConstValue::Float(*value)),
            ExprKind::Bool(value) => Some(ConstValue::Bool(*value)),
            ExprKind::String(value) => Some(ConstValue::Str(value.clone())),
            ExprKind::Char(value) => Some(ConstValue::Char(*value)),
            ExprKind::Variable(name) => {
                if self.by_name.contains_key(name) {
                    self.resolve(name)
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0450",
                        format!(
                            "constant initializer may only reference other constants; `{name}` is not a constant"
                        ),
                        None,
                        expr.span,
                    ));
                    None
                }
            }
            ExprKind::Unary { op, expr: operand } => {
                let value = self.eval_expr(operand)?;
                self.eval_unary(*op, value, expr.span)
            }
            ExprKind::Binary { left, op, right } => {
                let left_value = self.eval_expr(left);
                let right_value = self.eval_expr(right);
                match (left_value, right_value) {
                    (Some(left_value), Some(right_value)) => {
                        self.eval_binary(left_value, *op, right_value, expr.span)
                    }
                    _ => None,
                }
            }
            _ => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0450",
                    "constant initializer must be a constant expression (literals and \
                     arithmetic/logical/bitwise/comparison operators over literals and other \
                     constants)",
                    None,
                    expr.span,
                ));
                None
            }
        }
    }

    fn eval_unary(&mut self, op: UnaryOp, value: ConstValue, span: Span) -> Option<ConstValue> {
        match (op, value) {
            (UnaryOp::Not, ConstValue::Bool(b)) => Some(ConstValue::Bool(!b)),
            (UnaryOp::BitNot, ConstValue::Int(v)) => Some(ConstValue::Int(!v)),
            (UnaryOp::Negate, ConstValue::Int(v)) => Some(ConstValue::Int(v.wrapping_neg())),
            (UnaryOp::Negate, ConstValue::Float(v)) => Some(ConstValue::Float(-v)),
            _ => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0450",
                    "invalid operand type for a unary operator in a constant expression",
                    None,
                    span,
                ));
                None
            }
        }
    }

    fn eval_binary(
        &mut self,
        left: ConstValue,
        op: BinaryOp,
        right: ConstValue,
        span: Span,
    ) -> Option<ConstValue> {
        use ConstValue::{Bool, Char, Float, Int, Str};

        // `+` concatenation for string/string and string/char (either order).
        if op == BinaryOp::Add {
            match (&left, &right) {
                (Str(a), Str(b)) => return Some(Str(format!("{a}{b}"))),
                (Str(a), Char(b)) => return Some(Str(format!("{a}{b}"))),
                (Char(a), Str(b)) => return Some(Str(format!("{a}{b}"))),
                _ => {}
            }
        }

        match (left, right) {
            (Int(a), Int(b)) => self.eval_int_binary(a, op, b, span),
            (Float(a), Float(b)) => self.eval_float_binary(a, op, b, span),
            (Bool(a), Bool(b)) => match op {
                BinaryOp::And => Some(Bool(a && b)),
                BinaryOp::Or => Some(Bool(a || b)),
                BinaryOp::Equal => Some(Bool(a == b)),
                BinaryOp::NotEqual => Some(Bool(a != b)),
                _ => self.binary_type_error(span),
            },
            (Char(a), Char(b)) => match op {
                BinaryOp::Equal => Some(Bool(a == b)),
                BinaryOp::NotEqual => Some(Bool(a != b)),
                BinaryOp::Less => Some(Bool(a < b)),
                BinaryOp::LessEqual => Some(Bool(a <= b)),
                BinaryOp::Greater => Some(Bool(a > b)),
                BinaryOp::GreaterEqual => Some(Bool(a >= b)),
                _ => self.binary_type_error(span),
            },
            (Str(a), Str(b)) => match op {
                // Equality on strings is allowed; ordering on strings is not (it
                // mirrors the checker's orderable-scalar rule).
                BinaryOp::Equal => Some(Bool(a == b)),
                BinaryOp::NotEqual => Some(Bool(a != b)),
                _ => self.binary_type_error(span),
            },
            _ => self.binary_type_error(span),
        }
    }

    fn eval_int_binary(&mut self, a: i64, op: BinaryOp, b: i64, span: Span) -> Option<ConstValue> {
        use ConstValue::{Bool, Int};
        match op {
            BinaryOp::Add => Some(Int(a.wrapping_add(b))),
            BinaryOp::Subtract => Some(Int(a.wrapping_sub(b))),
            BinaryOp::Multiply => Some(Int(a.wrapping_mul(b))),
            BinaryOp::Divide => {
                if b == 0 {
                    self.const_div_by_zero(span, "division")
                } else {
                    Some(Int(a.wrapping_div(b)))
                }
            }
            BinaryOp::Remainder => {
                if b == 0 {
                    self.const_div_by_zero(span, "remainder")
                } else {
                    Some(Int(a.wrapping_rem(b)))
                }
            }
            BinaryOp::Equal => Some(Bool(a == b)),
            BinaryOp::NotEqual => Some(Bool(a != b)),
            BinaryOp::Less => Some(Bool(a < b)),
            BinaryOp::LessEqual => Some(Bool(a <= b)),
            BinaryOp::Greater => Some(Bool(a > b)),
            BinaryOp::GreaterEqual => Some(Bool(a >= b)),
            BinaryOp::And | BinaryOp::Or => self.binary_type_error(span),
            BinaryOp::BitAnd => Some(Int(a & b)),
            BinaryOp::BitOr => Some(Int(a | b)),
            BinaryOp::BitXor => Some(Int(a ^ b)),
            // Shift counts are masked to the low 6 bits, matching the
            // interpreters and native backend for `i64`.
            BinaryOp::Shl => Some(Int(a.wrapping_shl(((b as u64) & 63) as u32))),
            BinaryOp::Shr => Some(Int(a.wrapping_shr(((b as u64) & 63) as u32))),
        }
    }

    fn eval_float_binary(
        &mut self,
        a: f64,
        op: BinaryOp,
        b: f64,
        span: Span,
    ) -> Option<ConstValue> {
        use ConstValue::{Bool, Float};
        match op {
            BinaryOp::Add => Some(Float(a + b)),
            BinaryOp::Subtract => Some(Float(a - b)),
            BinaryOp::Multiply => Some(Float(a * b)),
            BinaryOp::Divide => Some(Float(a / b)),
            BinaryOp::Equal => Some(Bool(a == b)),
            BinaryOp::NotEqual => Some(Bool(a != b)),
            BinaryOp::Less => Some(Bool(a < b)),
            BinaryOp::LessEqual => Some(Bool(a <= b)),
            BinaryOp::Greater => Some(Bool(a > b)),
            BinaryOp::GreaterEqual => Some(Bool(a >= b)),
            // `%` is integer-only; bitwise/shift/logical require integers.
            BinaryOp::Remainder
            | BinaryOp::And
            | BinaryOp::Or
            | BinaryOp::BitAnd
            | BinaryOp::BitOr
            | BinaryOp::BitXor
            | BinaryOp::Shl
            | BinaryOp::Shr => self.binary_type_error(span),
        }
    }

    fn binary_type_error(&mut self, span: Span) -> Option<ConstValue> {
        self.diagnostics.push(SemanticDiagnostic::at(
            "L0450",
            "invalid or mismatched operand types for an operator in a constant expression",
            None,
            span,
        ));
        None
    }

    fn const_div_by_zero(&mut self, span: Span, kind: &str) -> Option<ConstValue> {
        self.diagnostics.push(SemanticDiagnostic::at(
            "L0450",
            format!("{kind} by zero in a constant expression"),
            None,
            span,
        ));
        None
    }
}

/// Scope-aware folder that replaces every reference to a constant with its
/// evaluated literal. A local binding (parameter, `let`, loop variable, `match`
/// binding, closure parameter, or `catch` name) that shares a constant's name
/// shadows it, so such references are left untouched.
struct Folder<'a> {
    values: &'a HashMap<String, ConstValue>,
}

impl Folder<'_> {
    fn fold_program(&self, program: &mut Program) {
        for function in &mut program.functions {
            let mut local: HashSet<String> =
                function.params.iter().map(|p| p.name.clone()).collect();
            self.fold_stmts(&mut function.body, &mut local);
        }
        for decl in &mut program.impls {
            for method in &mut decl.methods {
                let mut local: HashSet<String> =
                    method.params.iter().map(|p| p.name.clone()).collect();
                self.fold_stmts(&mut method.body, &mut local);
            }
        }
        // Fold constants inside actor `init` and handler bodies too, threading the
        // actor's state fields plus the init/handler parameters as locals so a
        // constant reference in a handler is folded like one in a function.
        for decl in &mut program.actors {
            let state: HashSet<String> = decl.state.iter().map(|f| f.name.clone()).collect();
            if let Some(init) = &mut decl.init {
                let mut local: HashSet<String> = state.clone();
                local.extend(init.params.iter().map(|p| p.name.clone()));
                self.fold_stmts(&mut init.body, &mut local);
            }
            for handler in &mut decl.handlers {
                let mut local: HashSet<String> = state.clone();
                local.extend(handler.params.iter().map(|p| p.name.clone()));
                self.fold_stmts(&mut handler.body, &mut local);
            }
        }
    }

    /// Fold a statement list, threading `local` so a `let` binds its name for the
    /// rest of the same block.
    fn fold_stmts(&self, stmts: &mut [Stmt], local: &mut HashSet<String>) {
        for stmt in stmts {
            self.fold_stmt(stmt, local);
        }
    }

    fn fold_stmt(&self, stmt: &mut Stmt, local: &mut HashSet<String>) {
        match stmt {
            Stmt::Let { name, value, .. } => {
                self.fold_expr(value, local);
                local.insert(name.clone());
            }
            Stmt::Assign { path, value, .. } => {
                for place in path.iter_mut() {
                    if let Place::Index(index) = place {
                        self.fold_expr(index, local);
                    }
                }
                self.fold_expr(value, local);
            }
            Stmt::Return(Some(expr)) => self.fold_expr(expr, local),
            Stmt::Return(None) | Stmt::Break(_) | Stmt::Continue(_) => {}
            Stmt::Expr(expr) => self.fold_expr(expr, local),
            Stmt::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches.iter_mut() {
                    self.fold_expr(&mut branch.condition, local);
                    let mut inner = local.clone();
                    self.fold_stmts(&mut branch.body, &mut inner);
                }
                let mut inner = local.clone();
                self.fold_stmts(else_body, &mut inner);
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.fold_expr(condition, local);
                let mut inner = local.clone();
                self.fold_stmts(body, &mut inner);
            }
            Stmt::For {
                name,
                start,
                end,
                step,
                body,
                ..
            } => {
                self.fold_expr(start, local);
                self.fold_expr(end, local);
                if let Some(step) = step {
                    self.fold_expr(step, local);
                }
                let mut inner = local.clone();
                inner.insert(name.clone());
                self.fold_stmts(body, &mut inner);
            }
            Stmt::ForEach {
                name,
                iterable,
                body,
                ..
            } => {
                self.fold_expr(iterable, local);
                let mut inner = local.clone();
                inner.insert(name.clone());
                self.fold_stmts(body, &mut inner);
            }
            Stmt::Loop { body, .. } | Stmt::Unsafe { body, .. } => {
                let mut inner = local.clone();
                self.fold_stmts(body, &mut inner);
            }
            Stmt::Throw { value, .. } => self.fold_expr(value, local),
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                ..
            } => {
                let mut inner = local.clone();
                self.fold_stmts(body, &mut inner);
                let mut catch_scope = local.clone();
                catch_scope.insert(catch_name.clone());
                self.fold_stmts(catch_body, &mut catch_scope);
            }
            // A region declaration carries only literal numeric fields, and an
            // `asm` statement carries only literal bytes: neither can reference a
            // constant.
            Stmt::Region(_) | Stmt::Asm { .. } => {}
        }
    }

    fn fold_expr(&self, expr: &mut Expr, local: &HashSet<String>) {
        match &mut expr.kind {
            ExprKind::Variable(name) => {
                // A constant reference (not shadowed by a local of the same name)
                // folds to its literal value.
                if let Some(value) = self.values.get(name).filter(|_| !local.contains(name)) {
                    *expr = value.to_expr(expr.span);
                }
            }
            ExprKind::Integer(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::String(_)
            | ExprKind::Char(_) => {}
            ExprKind::Array(items) => {
                for item in items {
                    self.fold_expr(item, local);
                }
            }
            ExprKind::ArrayFill { value, count } => {
                self.fold_expr(value, local);
                self.fold_expr(count, local);
            }
            ExprKind::Index { target, index } => {
                self.fold_expr(target, local);
                self.fold_expr(index, local);
            }
            ExprKind::Unary { expr, .. } => self.fold_expr(expr, local),
            ExprKind::Binary { left, right, .. } => {
                self.fold_expr(left, local);
                self.fold_expr(right, local);
            }
            // The callee name is a function/constructor, never a constant; only
            // the arguments can carry constant references.
            ExprKind::Call { args, .. } => {
                for arg in args {
                    self.fold_expr(arg, local);
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                for (_, value) in fields {
                    self.fold_expr(value, local);
                }
            }
            ExprKind::Field { target, .. } => self.fold_expr(target, local),
            ExprKind::Match { scrutinee, arms } => {
                self.fold_expr(scrutinee, local);
                for arm in arms {
                    let mut inner = local.clone();
                    if let MatchPattern::Variant { bindings, .. } = &arm.pattern {
                        for binding in bindings {
                            inner.insert(binding.clone());
                        }
                    }
                    self.fold_stmts(&mut arm.body, &mut inner);
                }
            }
            ExprKind::Await { expr } => self.fold_expr(expr, local),
            ExprKind::Combinator { operand, .. } => self.fold_expr(operand, local),
            ExprKind::Try(inner) => self.fold_expr(inner, local),
            ExprKind::Closure { params, body, .. } => {
                let mut inner = local.clone();
                for param in params.iter() {
                    inner.insert(param.name.clone());
                }
                self.fold_expr(body, &inner);
            }
            ExprKind::Conditional {
                cond,
                then_branch,
                else_branch,
            } => {
                self.fold_expr(cond, local);
                self.fold_expr(then_branch, local);
                self.fold_expr(else_branch, local);
            }
            ExprKind::In { value, collection } => {
                self.fold_expr(value, local);
                self.fold_expr(collection, local);
            }
            ExprKind::Slice { target, start, end } => {
                self.fold_expr(target, local);
                if let Some(start) = start {
                    self.fold_expr(start, local);
                }
                if let Some(end) = end {
                    self.fold_expr(end, local);
                }
            }
            ExprKind::Spawn { args, .. } => {
                for arg in args {
                    self.fold_expr(arg, local);
                }
            }
            ExprKind::Tell { target, args, .. } => {
                self.fold_expr(target, local);
                for arg in args {
                    self.fold_expr(arg, local);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use lullaby_lexer::lex;
    use lullaby_parser::parse;

    use crate::{SemanticDiagnostic, validate};

    fn diagnostics(source: &str) -> Vec<SemanticDiagnostic> {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        validate(&program).err().unwrap_or_default()
    }

    fn has_code(source: &str, code: &str) -> bool {
        diagnostics(source).iter().any(|d| d.code == code)
    }

    #[test]
    fn accepts_constant_expression_over_literals_and_constants() {
        // Literals, arithmetic over another constant, bitwise, and mixed scalar
        // kinds all type-check and evaluate.
        let source = concat!(
            "const MAX_LEN i64 = 128\n",
            "const DOUBLED i64 = MAX_LEN * 2\n",
            "const MASK i64 = (1 << 4) - 1\n",
            "const GREETING string = \"hi\"\n",
            "const PI f64 = 3.0\n",
            "const ENABLED bool = true\n",
            "const GRADE char = 'A'\n",
            "fn main -> i64\n",
            "    DOUBLED + MASK + len(GREETING)\n",
        );
        assert!(
            validate(&parse(&lex(source).expect("lex")).expect("parse")).is_ok(),
            "expected a clean constant program: {:?}",
            diagnostics(source)
        );
    }

    #[test]
    fn rejects_non_constant_initializer_call() {
        // A call to an ordinary function is not a constant expression.
        let source = concat!(
            "fn helper -> i64\n    5\n\n",
            "const X i64 = helper()\n\n",
            "fn main -> i64\n    X\n",
        );
        assert!(has_code(source, "L0450"), "{:?}", diagnostics(source));
    }

    #[test]
    fn rejects_non_constant_initializer_reference_to_runtime_name() {
        // A reference to something that is neither a literal nor a constant.
        let source = concat!(
            "fn helper -> i64\n    5\n\n",
            "const X i64 = helper\n\n",
            "fn main -> i64\n    X\n",
        );
        assert!(has_code(source, "L0450"), "{:?}", diagnostics(source));
    }

    #[test]
    fn rejects_division_by_zero_in_constant() {
        assert!(
            has_code("const X i64 = 5 / 0\n\nfn main -> i64\n    X\n", "L0450"),
            "constant division by zero is rejected"
        );
        assert!(
            has_code("const X i64 = 5 % 0\n\nfn main -> i64\n    X\n", "L0450"),
            "constant remainder by zero is rejected"
        );
    }

    #[test]
    fn rejects_type_mismatch() {
        assert!(
            has_code("const X i64 = \"hi\"\n\nfn main -> i64\n    X\n", "L0451"),
            "string initializer for an i64 constant is rejected"
        );
        assert!(
            has_code("const X f64 = 5\n\nfn main -> f64\n    X\n", "L0451"),
            "integer literal for an f64 constant is rejected (no implicit widening)"
        );
    }

    #[test]
    fn rejects_cyclic_constants() {
        let source = concat!(
            "const A i64 = B + 1\n",
            "const B i64 = A + 1\n\n",
            "fn main -> i64\n    A\n",
        );
        assert!(has_code(source, "L0452"), "{:?}", diagnostics(source));
    }

    #[test]
    fn rejects_self_referential_constant() {
        let source = "const A i64 = A + 1\n\nfn main -> i64\n    A\n";
        assert!(has_code(source, "L0452"), "{:?}", diagnostics(source));
    }

    #[test]
    fn rejects_constant_colliding_with_function_name() {
        // A constant may not reuse a top-level function name (flat namespace).
        let source = concat!(
            "fn value -> i64\n    1\n\n",
            "const value i64 = 2\n\n",
            "fn main -> i64\n    value()\n",
        );
        assert!(has_code(source, "L0453"), "{:?}", diagnostics(source));
    }

    #[test]
    fn constant_error_does_not_cascade_into_unknown_variable() {
        // A constant whose value fails to evaluate is still typed by its declared
        // type, so a reference to it does not additionally raise `L0306`.
        let source = concat!(
            "fn helper -> i64\n    5\n\n",
            "const X i64 = helper()\n\n",
            "fn main -> i64\n    X + 1\n",
        );
        let found = diagnostics(source);
        assert!(
            found.iter().any(|d| d.code == "L0450"),
            "the real constant error is reported: {found:?}"
        );
        assert!(
            !found.iter().any(|d| d.code == "L0306"),
            "no spurious unknown-variable cascade: {found:?}"
        );
    }
}
