//! The bytecode VM: the flat `VmOp` instruction set, the `VmCompiler` that
//! lowers eligible bytecode functions to it, and the shared `Env` scope stack
//! used by both the VM and the tree-walking IR interpreter. Split out of lib.rs;
//! sees the crate's IR/bytecode types via `use super::*`.

use super::*;

pub(crate) enum VmOp {
    PushConst(Value),
    PushVoid,
    LoadLocal(usize),
    StoreLocal(usize),
    Binary(BinaryOp),
    Unary(UnaryOp),
    Index,
    /// `a[i]` where `a` is a bare local: borrow the container from its slot and
    /// clone only the element (the tree-walker's borrow fast path).
    IndexLocal(usize),
    Field(String),
    /// `s.field` where `s` is a bare local: borrow the struct from its slot.
    FieldLocal(usize, String),
    Call(String, usize),
    MakeArray(usize),
    Jump(usize),
    JumpIfFalse(usize),
    JumpIfTrue(usize),
    Pop,
    CheckStepNonzero(usize),
    /// Push whether the range-`for` counter (slot `var`) is still within `end`
    /// given `step`'s sign — the loop-continuation test.
    ForCheck {
        var: usize,
        end: usize,
        step: usize,
    },
    /// Advance the range-`for` counter (slot `var`) by `step` (wrapping).
    ForStep {
        var: usize,
        step: usize,
    },
    Return,
}

/// A compiled function body: the op stream, a parallel span per op (so a runtime
/// error carries the same source span the tree-walker would attach), and the
/// number of frame slots it uses.
pub(crate) struct VmProgram {
    pub(crate) ops: Vec<VmOp>,
    pub(crate) spans: Vec<Span>,
    pub(crate) frame_size: usize,
}

/// The control outcome of executing one [`VmOp`].
pub(crate) enum VmStep {
    Next,
    Jump(usize),
    Return(Value),
}

/// Apply a unary operator to a value — the exact logic of the tree-walker's
/// `Unary` arm, reused by the VM so results match.
pub(crate) fn eval_unary_value(op: UnaryOp, value: Value) -> Result<Value, RuntimeError> {
    match op {
        UnaryOp::Not => Ok(Value::Bool(!value.as_bool()?)),
        UnaryOp::BitNot => match value {
            Value::Int { value, ty } => Ok(Value::int(!value, ty)),
            other => Ok(Value::I64(!other.as_i64()?)),
        },
        UnaryOp::Negate => match value {
            Value::Int { value, ty } => Ok(Value::int(value.wrapping_neg(), ty)),
            Value::F64(f) => Ok(Value::F64(-f)),
            Value::F32(f) => Ok(Value::F32(-f)),
            other => Ok(Value::I64(other.as_i64()?.wrapping_neg())),
        },
    }
}

/// Read a struct field — the tree-walker's `Field` logic, reused by the VM.
pub(crate) fn field_of(target: &Value, field: &str) -> Result<Value, RuntimeError> {
    match target {
        Value::Struct(s) => s
            .fields
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| RuntimeError::new("L0371", format!("no field `{field}`"))),
        _ => Err(RuntimeError::new(
            "L0371",
            format!("cannot access field `{field}` on non-struct value"),
        )),
    }
}

/// The binary operator a compound assignment applies (`x += v` is `x = x + v`).
pub(crate) fn assign_binop(op: AssignOp) -> BinaryOp {
    match op {
        AssignOp::Add => BinaryOp::Add,
        AssignOp::Subtract => BinaryOp::Subtract,
        AssignOp::Multiply => BinaryOp::Multiply,
        AssignOp::Divide => BinaryOp::Divide,
        AssignOp::Remainder => BinaryOp::Remainder,
        AssignOp::Replace => unreachable!("Replace is not a compound op"),
    }
}

/// Break/continue patch targets for one loop being compiled (mirrors the native
/// backend's `NativeLoop`). `continue_target` is set when known up front
/// (`while`/`loop` continue to the top); a range-`for` continue jumps forward to
/// its step block, so those jumps are recorded and patched once its offset exists.
pub(crate) struct VmLoop {
    continue_target: Option<usize>,
    continue_sites: Vec<usize>,
    break_sites: Vec<usize>,
}

/// Compiles an [`IrFunction`] body into a [`VmProgram`], assigning every binding
/// a flat frame slot and linearizing control flow to jumps. Returns `Err(())` the
/// moment it meets a construct it does not lower, so the caller falls back.
pub(crate) struct VmCompiler {
    ops: Vec<VmOp>,
    /// Source span for each emitted op (parallel to `ops`), so a failing op
    /// reports the same span the tree-walker would attach.
    spans: Vec<Span>,
    /// The span attached to subsequently-emitted ops; set to the current
    /// statement/expression before emitting its op.
    cur_span: Span,
    /// Lexical scopes of `(name, slot)`, searched innermost-first (and newest-first
    /// within a scope, so a re-`let` shadows) — matching the tree-walker's
    /// resolution. Slots themselves are unique across the whole function.
    scopes: Vec<Vec<(String, usize)>>,
    next_slot: usize,
    /// Every name ever bound as a local, so a `Call` whose name is a local (a
    /// first-class function value) can be rejected as ineligible.
    locals: HashSet<String>,
    loops: Vec<VmLoop>,
}

pub(crate) fn compile_function_to_vm(function: &IrFunction) -> Option<VmProgram> {
    let mut c = VmCompiler {
        ops: Vec::new(),
        spans: Vec::new(),
        cur_span: function.span,
        scopes: vec![Vec::new()],
        next_slot: 0,
        locals: HashSet::new(),
        loops: Vec::new(),
    };
    for param in &function.params {
        c.declare(&param.name);
    }
    // The function body is the function scope (params + top-level lets share it,
    // like the tree-walker): compile the statements in tail position so the last
    // one's value is the implicit return, then a trailing `Return` yields it.
    c.compile_stmts(&function.body, true).ok()?;
    c.emit(VmOp::Return);
    Some(VmProgram {
        ops: c.ops,
        spans: c.spans,
        frame_size: c.next_slot,
    })
}

impl VmCompiler {
    pub(crate) fn emit(&mut self, op: VmOp) -> usize {
        let index = self.ops.len();
        self.ops.push(op);
        self.spans.push(self.cur_span);
        index
    }

    pub(crate) fn patch(&mut self, site: usize, target: usize) {
        match &mut self.ops[site] {
            VmOp::Jump(t) | VmOp::JumpIfFalse(t) | VmOp::JumpIfTrue(t) => *t = target,
            _ => unreachable!("patch site is not a jump"),
        }
    }

    /// Introduce a binding, giving it a fresh unique slot in the current scope.
    pub(crate) fn declare(&mut self, name: &str) -> usize {
        let slot = self.next_slot;
        self.next_slot += 1;
        self.scopes
            .last_mut()
            .expect("a scope is always open")
            .push((name.to_string(), slot));
        self.locals.insert(name.to_string());
        slot
    }

    /// A slot with no source name (a range-`for`'s `end`/`step` temporaries).
    pub(crate) fn alloc_temp(&mut self) -> usize {
        let slot = self.next_slot;
        self.next_slot += 1;
        slot
    }

    pub(crate) fn resolve(&self, name: &str) -> Option<usize> {
        for scope in self.scopes.iter().rev() {
            if let Some((_, slot)) = scope.iter().rev().find(|(n, _)| n == name) {
                return Some(*slot);
            }
        }
        None
    }

    pub(crate) fn bare_local_slot(&self, expr: &IrExpr) -> Option<usize> {
        match &expr.kind {
            IrExprKind::Variable(name) | IrExprKind::Local { name, .. } => self.resolve(name),
            _ => None,
        }
    }

    pub(crate) fn compile_stmts(&mut self, body: &[IrStmt], tail: bool) -> Result<(), ()> {
        if body.is_empty() {
            if tail {
                self.emit(VmOp::PushVoid);
            }
            return Ok(());
        }
        let last = body.len() - 1;
        for (index, stmt) in body.iter().enumerate() {
            self.compile_stmt(stmt, tail && index == last)?;
        }
        Ok(())
    }

    pub(crate) fn compile_scoped_block(&mut self, body: &[IrStmt], tail: bool) -> Result<(), ()> {
        self.scopes.push(Vec::new());
        let result = self.compile_stmts(body, tail);
        self.scopes.pop();
        result
    }

    pub(crate) fn compile_stmt(&mut self, stmt: &IrStmt, tail: bool) -> Result<(), ()> {
        self.cur_span = statement_span(stmt);
        match stmt {
            IrStmt::Let { name, value, .. } => {
                self.compile_expr(value)?;
                let slot = self.declare(name);
                self.emit(VmOp::StoreLocal(slot));
                if tail {
                    self.emit(VmOp::PushVoid);
                }
            }
            IrStmt::Assign {
                name,
                path,
                op,
                value,
                ..
            } => {
                if !path.is_empty() {
                    return Err(()); // indexed/field assignment: fall back
                }
                let slot = self.resolve(name).ok_or(())?;
                match op {
                    AssignOp::Replace => {
                        self.compile_expr(value)?;
                        self.emit(VmOp::StoreLocal(slot));
                    }
                    other => {
                        self.emit(VmOp::LoadLocal(slot));
                        self.compile_expr(value)?;
                        self.emit(VmOp::Binary(assign_binop(*other)));
                        self.emit(VmOp::StoreLocal(slot));
                    }
                }
                if tail {
                    self.emit(VmOp::PushVoid);
                }
            }
            IrStmt::Return(expr) => {
                match expr {
                    Some(expr) => self.compile_expr(expr)?,
                    None => {
                        self.emit(VmOp::PushVoid);
                    }
                }
                self.emit(VmOp::Return);
            }
            IrStmt::Expr(expr) => {
                self.compile_expr(expr)?;
                if !tail {
                    self.emit(VmOp::Pop);
                }
            }
            IrStmt::If {
                branches,
                else_body,
                ..
            } => {
                let mut end_jumps = Vec::new();
                for branch in branches {
                    self.compile_expr(&branch.condition)?;
                    let skip = self.emit(VmOp::JumpIfFalse(0));
                    self.compile_scoped_block(&branch.body, tail)?;
                    end_jumps.push(self.emit(VmOp::Jump(0)));
                    let next = self.ops.len();
                    self.patch(skip, next);
                }
                self.compile_scoped_block(else_body, tail)?;
                let end = self.ops.len();
                for jump in end_jumps {
                    self.patch(jump, end);
                }
            }
            IrStmt::While {
                condition, body, ..
            } => {
                let top = self.ops.len();
                self.compile_expr(condition)?;
                let exit = self.emit(VmOp::JumpIfFalse(0));
                self.loops.push(VmLoop {
                    continue_target: Some(top),
                    continue_sites: Vec::new(),
                    break_sites: Vec::new(),
                });
                self.compile_scoped_block(body, false)?;
                self.emit(VmOp::Jump(top));
                let loop_ctx = self.loops.pop().expect("loop pushed");
                let end = self.ops.len();
                self.patch(exit, end);
                for site in loop_ctx.break_sites {
                    self.patch(site, end);
                }
                if tail {
                    self.emit(VmOp::PushVoid);
                }
            }
            IrStmt::For {
                name,
                start,
                end,
                step,
                body,
                ..
            } => {
                // The loop variable lives in its own scope (popped after the loop).
                self.scopes.push(Vec::new());
                self.compile_expr(start)?;
                let var = self.declare(name);
                self.emit(VmOp::StoreLocal(var));
                self.compile_expr(end)?;
                let end_slot = self.alloc_temp();
                self.emit(VmOp::StoreLocal(end_slot));
                match step {
                    Some(step) => self.compile_expr(step)?,
                    None => {
                        self.emit(VmOp::PushConst(Value::I64(1)));
                    }
                }
                let step_slot = self.alloc_temp();
                self.emit(VmOp::StoreLocal(step_slot));
                self.emit(VmOp::CheckStepNonzero(step_slot));
                let top = self.ops.len();
                self.emit(VmOp::ForCheck {
                    var,
                    end: end_slot,
                    step: step_slot,
                });
                let exit = self.emit(VmOp::JumpIfFalse(0));
                self.loops.push(VmLoop {
                    continue_target: None,
                    continue_sites: Vec::new(),
                    break_sites: Vec::new(),
                });
                self.compile_scoped_block(body, false)?;
                let loop_ctx = self.loops.pop().expect("loop pushed");
                let step_pc = self.ops.len();
                self.emit(VmOp::ForStep {
                    var,
                    step: step_slot,
                });
                self.emit(VmOp::Jump(top));
                let end_pc = self.ops.len();
                self.patch(exit, end_pc);
                for site in loop_ctx.break_sites {
                    self.patch(site, end_pc);
                }
                for site in loop_ctx.continue_sites {
                    self.patch(site, step_pc);
                }
                self.scopes.pop();
                if tail {
                    self.emit(VmOp::PushVoid);
                }
            }
            IrStmt::Loop { body, .. } => {
                let top = self.ops.len();
                self.loops.push(VmLoop {
                    continue_target: Some(top),
                    continue_sites: Vec::new(),
                    break_sites: Vec::new(),
                });
                self.compile_scoped_block(body, false)?;
                self.emit(VmOp::Jump(top));
                let loop_ctx = self.loops.pop().expect("loop pushed");
                let end = self.ops.len();
                for site in loop_ctx.break_sites {
                    self.patch(site, end);
                }
                if tail {
                    self.emit(VmOp::PushVoid);
                }
            }
            IrStmt::Break(_) => {
                let site = self.emit(VmOp::Jump(0));
                self.loops.last_mut().ok_or(())?.break_sites.push(site);
            }
            IrStmt::Continue(_) => {
                let target = self.loops.last().ok_or(())?.continue_target;
                match target {
                    Some(top) => {
                        self.emit(VmOp::Jump(top));
                    }
                    None => {
                        let site = self.emit(VmOp::Jump(0));
                        self.loops
                            .last_mut()
                            .expect("loop present")
                            .continue_sites
                            .push(site);
                    }
                }
            }
            // Constructs the VM does not lower: fall back to the tree-walker.
            IrStmt::Try { .. }
            | IrStmt::Match { .. }
            | IrStmt::Throw { .. }
            | IrStmt::Asm { .. } => return Err(()),
        }
        Ok(())
    }

    pub(crate) fn compile_expr(&mut self, expr: &IrExpr) -> Result<(), ()> {
        let span = expr.span;
        self.cur_span = span;
        match &expr.kind {
            IrExprKind::Integer(value) => {
                self.emit(VmOp::PushConst(Value::I64(*value)));
            }
            IrExprKind::Float(value) => {
                self.emit(VmOp::PushConst(Value::F64(*value)));
            }
            IrExprKind::Bool(value) => {
                self.emit(VmOp::PushConst(Value::Bool(*value)));
            }
            IrExprKind::String(value) => {
                self.emit(VmOp::PushConst(Value::String(value.clone().into())));
            }
            IrExprKind::Char(value) => {
                self.emit(VmOp::PushConst(Value::Char(*value)));
            }
            IrExprKind::Variable(name) | IrExprKind::Local { name, .. } => {
                // A bare name that is not a local (an enum variant or a top-level
                // function used as a value) needs the tree-walker's fallback logic.
                let slot = self.resolve(name).ok_or(())?;
                self.emit(VmOp::LoadLocal(slot));
            }
            IrExprKind::Unary { op, expr } => {
                self.compile_expr(expr)?;
                self.cur_span = span;
                self.emit(VmOp::Unary(*op));
            }
            IrExprKind::Binary { left, op, right } => match op {
                // Short-circuit: `a && b` = if !a { false } else { b }.
                BinaryOp::And => {
                    self.compile_expr(left)?;
                    let to_false = self.emit(VmOp::JumpIfFalse(0));
                    self.compile_expr(right)?;
                    let to_end = self.emit(VmOp::Jump(0));
                    let false_pc = self.ops.len();
                    self.patch(to_false, false_pc);
                    self.emit(VmOp::PushConst(Value::Bool(false)));
                    let end = self.ops.len();
                    self.patch(to_end, end);
                }
                // `a || b` = if a { true } else { b }.
                BinaryOp::Or => {
                    self.compile_expr(left)?;
                    let to_true = self.emit(VmOp::JumpIfTrue(0));
                    self.compile_expr(right)?;
                    let to_end = self.emit(VmOp::Jump(0));
                    let true_pc = self.ops.len();
                    self.patch(to_true, true_pc);
                    self.emit(VmOp::PushConst(Value::Bool(true)));
                    let end = self.ops.len();
                    self.patch(to_end, end);
                }
                _ => {
                    self.compile_expr(left)?;
                    self.compile_expr(right)?;
                    self.cur_span = span;
                    self.emit(VmOp::Binary(*op));
                }
            },
            IrExprKind::Index { target, index } => {
                if let Some(slot) = self.bare_local_slot(target) {
                    self.compile_expr(index)?;
                    self.cur_span = span;
                    self.emit(VmOp::IndexLocal(slot));
                } else {
                    self.compile_expr(target)?;
                    self.compile_expr(index)?;
                    self.cur_span = span;
                    self.emit(VmOp::Index);
                }
            }
            IrExprKind::Field { target, field } => {
                self.cur_span = span;
                if let Some(slot) = self.bare_local_slot(target) {
                    self.emit(VmOp::FieldLocal(slot, field.clone()));
                } else {
                    self.compile_expr(target)?;
                    self.cur_span = span;
                    self.emit(VmOp::Field(field.clone()));
                }
            }
            IrExprKind::Call { name, args } => {
                // A call through a local (a first-class function value) needs the
                // tree-walker's env-based dispatch; the VM only calls by name.
                if self.locals.contains(name) {
                    return Err(());
                }
                for arg in args {
                    self.compile_expr(arg)?;
                }
                self.cur_span = span;
                self.emit(VmOp::Call(name.clone(), args.len()));
            }
            IrExprKind::Array(elements) => {
                for element in elements {
                    self.compile_expr(element)?;
                }
                self.emit(VmOp::MakeArray(elements.len()));
            }
            // `await` and closure literals are not lowered by the VM.
            IrExprKind::Await { .. } | IrExprKind::Closure { .. } => return Err(()),
        }
        Ok(())
    }
}

pub(crate) enum Control {
    Return(Value),
    Break,
    Continue,
    Value(Value),
}

pub(crate) fn statement_span(statement: &IrStmt) -> Span {
    match statement {
        IrStmt::Let { span, .. }
        | IrStmt::Assign { span, .. }
        | IrStmt::Break(span)
        | IrStmt::Continue(span)
        | IrStmt::If { span, .. }
        | IrStmt::While { span, .. }
        | IrStmt::For { span, .. }
        | IrStmt::Loop { span, .. }
        | IrStmt::Asm { span, .. }
        | IrStmt::Throw { span, .. }
        | IrStmt::Try { span, .. }
        | IrStmt::Match { span, .. } => *span,
        IrStmt::Return(Some(expr)) | IrStmt::Expr(expr) => expr.span,
        IrStmt::Return(None) => Span::new(1, 1),
    }
}

/// Conservative "does `name` appear anywhere in this IR expression?" walk, the
/// IR twin of the AST runtime's `expr_mentions_var`. Used by the
/// move-on-functional-update fast path to prove the target variable is not
/// referenced outside its single consuming argument. It over-approximates on
/// purpose (a mention inside a nested closure body or a matching call name still
/// counts); over-approximating only ever forgoes the optimization, never changes
/// a result, so the walk stays simple and total over `IrExprKind`.
pub(crate) fn expr_mentions_var(expr: &IrExpr, name: &str) -> bool {
    match &expr.kind {
        IrExprKind::Integer(_)
        | IrExprKind::Float(_)
        | IrExprKind::Bool(_)
        | IrExprKind::String(_)
        | IrExprKind::Char(_) => false,
        // A resolved `Local` still names the same binding, so it counts as a
        // mention. Missing it here would let the move-on-functional-update fast
        // path move a value that a `Local` still reads — an under-approximation
        // that must never happen (over-approximating only forgoes the move).
        IrExprKind::Variable(v) | IrExprKind::Local { name: v, .. } => v == name,
        IrExprKind::Array(items) => items.iter().any(|item| expr_mentions_var(item, name)),
        IrExprKind::Index { target, index } => {
            expr_mentions_var(target, name) || expr_mentions_var(index, name)
        }
        IrExprKind::Unary { expr, .. } => expr_mentions_var(expr, name),
        IrExprKind::Binary { left, right, .. } => {
            expr_mentions_var(left, name) || expr_mentions_var(right, name)
        }
        IrExprKind::Call { name: callee, args } => {
            callee == name || args.iter().any(|arg| expr_mentions_var(arg, name))
        }
        IrExprKind::Field { target, .. } => expr_mentions_var(target, name),
        IrExprKind::Await { expr } => expr_mentions_var(expr, name),
        IrExprKind::Closure { .. } => false,
    }
}

/// A lexical environment: a stack of scopes, each an insertion-ordered
/// association list of `(name, value)`. Function-call and block scopes are
/// small, so a linear-scan `Vec` beats a `HashMap` — it avoids a per-scope
/// bucket allocation and per-access string hashing, and its contiguous layout
/// is cache-friendly. `define` keeps at most one binding per name per scope
/// (replacing in place, like the previous `HashMap::insert`), so resolution
/// never disambiguates duplicates within a scope; cross-scope shadowing is
/// innermost-first. Mirrors the AST runtime's `Env` one-to-one.
#[derive(Debug, Clone)]
pub(crate) struct Env {
    scopes: Vec<Vec<(String, Value)>>,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            scopes: vec![Vec::new()],
        }
    }
}

impl Env {
    /// Reset to a single empty scope so a pooled environment can be reused for the
    /// next call, keeping each scope's `Vec` capacity. Clearing every entry means
    /// no stale binding can leak into the reused environment.
    pub(crate) fn reset(&mut self) {
        self.scopes.truncate(1);
        match self.scopes.first_mut() {
            Some(first) => first.clear(),
            None => self.scopes.push(Vec::new()),
        }
    }

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Update the loop variable's binding in the innermost scope in place. The
    /// range-`for` lowering calls this each iteration with the loop-variable scope
    /// innermost (the body scope has been popped), so it never allocates or clones
    /// the name — the hot-path replacement for a per-iteration `define`.
    pub(crate) fn set_loop_var(&mut self, name: &str, value: Value) {
        let scope = self.scopes.last_mut().expect("env always has a scope");
        for (existing, slot) in scope.iter_mut() {
            if existing == name {
                *slot = value;
                return;
            }
        }
        scope.push((name.to_string(), value));
    }

    pub(crate) fn define(&mut self, name: String, value: Value) {
        let scope = self.scopes.last_mut().expect("env always has a scope");
        for (existing, slot) in scope.iter_mut() {
            if *existing == name {
                *slot = value;
                return;
            }
        }
        scope.push((name, value));
    }

    /// Borrow the nearest binding of `name` mutably for in-place element/field
    /// mutation (`a[i] = v`), avoiding a whole-container clone + write-back.
    pub(crate) fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        for scope in self.scopes.iter_mut().rev() {
            for (existing, slot) in scope.iter_mut() {
                if existing == name {
                    return Some(slot);
                }
            }
        }
        None
    }

    pub(crate) fn assign(&mut self, name: &str, value: Value) -> Result<(), RuntimeError> {
        for scope in self.scopes.iter_mut().rev() {
            for (existing, slot) in scope.iter_mut() {
                if existing == name {
                    *slot = value;
                    return Ok(());
                }
            }
        }
        Err(RuntimeError::new(
            "L0403",
            format!("unknown variable `{name}`"),
        ))
    }

    pub(crate) fn get(&self, name: &str) -> Result<Value, RuntimeError> {
        self.get_ref(name)
            .cloned()
            .ok_or_else(|| RuntimeError::new("L0403", format!("unknown variable `{name}`")))
    }

    /// Borrow a binding's value without cloning it (innermost-first, like
    /// [`Env::get`]). Used to classify a call target on the
    /// move-on-functional-update fast path without paying for a clone.
    pub(crate) fn get_ref(&self, name: &str) -> Option<&Value> {
        for scope in self.scopes.iter().rev() {
            for (existing, value) in scope.iter() {
                if existing == name {
                    return Some(value);
                }
            }
        }
        None
    }

    /// Borrow a slot-resolved binding directly, with no name scan. `packed` is a
    /// `(depth, slot)` pair produced by [`resolve_slots`]: `depth` counts scopes up
    /// from the innermost and `slot` indexes within that scope. The lookup is
    /// **validated** — it confirms the binding at that position still carries
    /// `name` before returning it, and returns `None` (so the caller falls back to
    /// the name scan) if the position is out of range or the name does not match.
    /// That validation makes the fast path correct-or-slower by construction: a
    /// mis-resolved slot can never read the wrong binding, only miss and fall back.
    pub(crate) fn get_slot(&self, packed: u32, name: &str) -> Option<&Value> {
        let (depth, slot) = unpack_slot(packed);
        let idx = self.scopes.len().checked_sub(1 + depth)?;
        let (existing, value) = self.scopes.get(idx)?.get(slot)?;
        (existing == name).then_some(value)
    }

    /// True when `name` is bound in the innermost (current) scope. A `let x =
    /// f(x, …)` re-binding only moves when the consumed binding lives here,
    /// because `let` shadows into the innermost scope rather than overwriting an
    /// outer binding.
    pub(crate) fn innermost_has(&self, name: &str) -> bool {
        self.scopes
            .last()
            .is_some_and(|scope| scope.iter().any(|(n, _)| n == name))
    }

    /// True when `name` is bound in any scope (a normal local). A plain `x =
    /// f(x, …)` reassignment moves from — and writes back to — the *nearest*
    /// binding, and both [`Env::get`] and [`Env::assign`] resolve nearest-first to
    /// that same slot, so the move is safe at any scope depth (e.g. `x` declared
    /// outside a loop, reassigned inside it).
    pub(crate) fn is_bound(&self, name: &str) -> bool {
        self.get_ref(name).is_some()
    }

    /// Move the value out of the nearest scope binding `name`, leaving a cheap
    /// [`Value::Void`] placeholder in the same slot (no clone), and return the old
    /// value. Nearest-first, matching [`Env::get`]/[`Env::assign`] resolution, so
    /// the caller's write-back overwrites this exact slot. The placeholder is
    /// never observable (see the AST runtime twin for the full argument).
    pub(crate) fn move_out_nearest(&mut self, name: &str) -> Option<Value> {
        for scope in self.scopes.iter_mut().rev() {
            for (existing, slot) in scope.iter_mut() {
                if existing == name {
                    return Some(std::mem::replace(slot, Value::Void));
                }
            }
        }
        None
    }

    /// Snapshot every in-scope local by value for closure frame capture, mirroring
    /// the AST runtime: one `(name, value.clone())` per visible binding, inner
    /// scopes shadowing outer ones, sorted by name for a deterministic order.
    pub(crate) fn snapshot_locals(&self) -> Vec<(String, Value)> {
        let mut flattened: HashMap<&str, &Value> = HashMap::new();
        for scope in &self.scopes {
            for (name, value) in scope {
                flattened.insert(name.as_str(), value);
            }
        }
        let mut captured: Vec<(String, Value)> = flattened
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect();
        captured.sort_by(|(a, _), (b, _)| a.cmp(b));
        captured
    }
}

pub(crate) struct Lowerer<'a> {
    program: &'a Program,
    signatures: &'a HashMap<String, Signature>,
    /// Declared return type of the function currently being lowered. Threaded so
    /// `return EXPR` and a function's final expression can supply the expected
    /// type that `none`/`ok`/`err` need. Set at the start of each function.
    current_return_type: std::cell::RefCell<TypeRef>,
    /// Statements hoisted while desugaring postfix `?` operators in the statement
    /// currently being lowered. Each `EXPR?` pushes a `let __q = <operand>`, a
    /// typed `let __v`, and a `match __q` (writing `__v` on success, `return`ing
    /// the failure value otherwise) here, then rewrites its position to reference
    /// `__v`. The block lowerers drain this in order before the statement, so the
    /// `?` node never reaches the IR — only `let`/`assign`/`match`/`return`, which
    /// every backend already handles.
    try_prelude: std::cell::RefCell<Vec<IrStmt>>,
    /// Monotonic counter for fresh `?`-desugar temp names, unique per program so
    /// hoisted temporaries never collide with user bindings or each other.
    next_try_temp: std::cell::Cell<usize>,
    /// Monotonic counter for fresh inline-conditional (`THEN if COND else ELSE`)
    /// desugar temp names, unique per program for the same reason.
    next_cond_temp: std::cell::Cell<usize>,
    /// Monotonic counter for fresh match-expression desugar temp names, unique
    /// per program for the same reason.
    next_match_temp: std::cell::Cell<usize>,
    /// Lowered closure bodies collected while lowering, keyed by parse-order id.
    /// Each `ExprKind::Closure` lowering registers an entry here and emits an
    /// `IrExprKind::Closure { id }` node; the accumulated table is attached to
    /// the `IrModule` at the end of lowering.
    closures: std::cell::RefCell<Vec<IrClosureDef>>,
}

impl<'a> Lowerer<'a> {
    pub(crate) fn new(program: &'a Program, signatures: &'a HashMap<String, Signature>) -> Self {
        Self {
            program,
            signatures,
            current_return_type: std::cell::RefCell::new(TypeRef::new("void")),
            try_prelude: std::cell::RefCell::new(Vec::new()),
            next_try_temp: std::cell::Cell::new(0),
            next_cond_temp: std::cell::Cell::new(0),
            next_match_temp: std::cell::Cell::new(0),
            closures: std::cell::RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn lower_program(&self) -> Result<IrModule, IrLoweringError> {
        // Extern (C-ABI) declarations are body-less: they are recorded by name in
        // `extern_functions` (below) and never lowered to an `IrFunction`.
        let functions = self
            .program
            .functions
            .iter()
            .filter(|function| !function.is_extern)
            .map(|function| self.lower_function(function))
            .collect::<Result<Vec<_>, _>>()?;
        let structs = self
            .program
            .structs
            .iter()
            .map(|declaration| IrStructDef {
                name: declaration.name.clone(),
                fields: declaration
                    .fields
                    .iter()
                    .map(|field| (field.name.clone(), field.ty.clone()))
                    .collect(),
            })
            .collect();
        let enums = self
            .program
            .enums
            .iter()
            .map(|declaration| IrEnumDef {
                name: declaration.name.clone(),
                variants: declaration
                    .variants
                    .iter()
                    .map(|variant| IrEnumVariant {
                        name: variant.name.clone(),
                        payload: variant.payload.clone(),
                    })
                    .collect(),
            })
            .collect();
        // Lower every trait impl method to an IR function, keyed by
        // `(type_name, method_name)` for runtime dispatch.
        let mut impls = Vec::new();
        for decl in &self.program.impls {
            for method in &decl.methods {
                impls.push(IrImplMethod {
                    type_name: decl.type_name.clone(),
                    method_name: method.name.clone(),
                    function: self.lower_function(method)?,
                });
            }
        }
        let trait_methods = self
            .program
            .traits
            .iter()
            .flat_map(|decl| decl.methods.iter().map(|method| method.name.clone()))
            .collect();
        // Record every `async fn` so the interpreter and VM spawn a thread on a
        // call to one (and yield a `Future`).
        let async_functions = self
            .program
            .functions
            .iter()
            .filter(|function| function.is_async)
            .map(|function| function.name.clone())
            .collect();
        // Record every `extern fn` so a call resolves to an external-symbol call
        // on the native backend and to an `L0423` on the interpreters.
        let extern_functions = self
            .program
            .functions
            .iter()
            .filter(|function| function.is_extern)
            .map(|function| function.name.clone())
            .collect();
        // The full C-ABI signature of each `extern fn`, so the native backend can
        // marshal argument/return scalar widths correctly. Same declaration order
        // as `extern_functions`.
        let extern_signatures = self
            .program
            .functions
            .iter()
            .filter(|function| function.is_extern)
            .map(|function| IrExternSignature {
                name: function.name.clone(),
                params: function.params.iter().map(|p| p.ty.clone()).collect(),
                return_type: function.return_type.clone(),
            })
            .collect();
        // Record every `export fn` so the native backend emits an externally
        // visible, defined symbol for it under its plain C name. The function is
        // lowered like any ordinary function (it has a body); `export` only
        // affects native symbol visibility.
        let export_functions = self
            .program
            .functions
            .iter()
            .filter(|function| function.is_export)
            .map(|function| function.name.clone())
            .collect();
        // Every closure body lowered above (across functions and impl methods) was
        // collected into the accumulator; sort by id for a deterministic module.
        let mut closures = self.closures.borrow().clone();
        closures.sort_by_key(|def| def.id);
        Ok(IrModule {
            functions,
            structs,
            enums,
            impls,
            trait_methods,
            async_functions,
            extern_functions,
            extern_signatures,
            export_functions,
            closures,
        })
    }

    /// The declared type of `field` on the struct value type `struct_name`, if
    /// any. `struct_name` may be a concrete generic instantiation spelling such
    /// as `Box<i64>`: its head (`Box`) resolves the declaration and its type
    /// arguments (`[i64]`) are substituted for the declaration's type parameters,
    /// so `value T` on `Box<i64>` resolves to `i64`. A non-generic struct's field
    /// type is returned verbatim.
    pub(crate) fn struct_field_type(&self, struct_name: &str, field: &str) -> Option<TypeRef> {
        let (head, args) = match struct_name.find('<') {
            Some(open) if struct_name.ends_with('>') && !struct_name.starts_with("fn(") => {
                let head = struct_name[..open].to_string();
                let args = TypeRef::new(struct_name)
                    .generic_args(&head)
                    .unwrap_or_default();
                (head, args)
            }
            _ => (struct_name.to_string(), Vec::new()),
        };
        let declaration = self
            .program
            .structs
            .iter()
            .find(|declaration| declaration.name == head)?;
        let field_ty = declaration
            .fields
            .iter()
            .find(|f| f.name == field)?
            .ty
            .clone();
        if declaration.type_params.is_empty() {
            return Some(field_ty);
        }
        let mut subst: HashMap<String, TypeRef> = HashMap::new();
        for (param, arg) in declaration.type_params.iter().zip(args.iter()) {
            subst.insert(param.name.clone(), arg.clone());
        }
        Some(lullaby_semantics::substitute_type(&field_ty, &subst))
    }

    pub(crate) fn is_struct(&self, name: &str) -> bool {
        self.program.structs.iter().any(|s| s.name == name)
    }

    /// If `name` is a trait method, its declared signature (from the trait).
    pub(crate) fn trait_method_sig(&self, name: &str) -> Option<&'a lullaby_parser::MethodSig> {
        self.program
            .traits
            .iter()
            .flat_map(|decl| decl.methods.iter())
            .find(|method| method.name == name)
    }

    /// If `name` is a known enum variant, the owning enum's name.
    pub(crate) fn enum_of_variant(&self, name: &str) -> Option<String> {
        // The compiler-provided `MemoryOrder` enum is not part of the user
        // program's declarations, so resolve its unit variants explicitly (as
        // semantics and the interpreters do).
        if MEMORY_ORDER_VARIANTS.contains(&name) {
            return Some("MemoryOrder".to_string());
        }
        self.program.enums.iter().find_map(|declaration| {
            declaration
                .variants
                .iter()
                .any(|variant| variant.name == name)
                .then(|| declaration.name.clone())
        })
    }

    pub(crate) fn lower_function(
        &self,
        function: &Function,
    ) -> Result<IrFunction, IrLoweringError> {
        let mut scope = function
            .params
            .iter()
            .map(|param| (param.name.clone(), param.ty.clone()))
            .collect::<HashMap<_, _>>();
        // Record the return type so `return` and the final expression can supply
        // the expected type to `none`/`ok`/`err`.
        *self.current_return_type.borrow_mut() = function.return_type.clone();
        Ok(IrFunction {
            name: function.name.clone(),
            params: function
                .params
                .iter()
                .map(|param| IrParam {
                    name: param.name.clone(),
                    ty: param.ty.clone(),
                })
                .collect(),
            return_type: function.return_type.clone(),
            body: self.lower_function_body(&function.body, &mut scope)?,
            span: function.span,
        })
    }

    pub(crate) fn lower_block(
        &self,
        statements: &[Stmt],
        scope: &mut HashMap<String, TypeRef>,
    ) -> Result<Vec<IrStmt>, IrLoweringError> {
        let mut lowered = Vec::with_capacity(statements.len());
        for statement in statements {
            match statement {
                // `unsafe` is a transparent compile-time gate; inline its body
                // into the enclosing block so no IR node is needed for it.
                Stmt::Unsafe { body, .. } => {
                    lowered.extend(self.lower_block(body, scope)?);
                }
                other => {
                    let stmt = self.lower_statement(other, scope)?;
                    self.drain_try_prelude_into(&mut lowered);
                    lowered.push(stmt);
                }
            }
        }
        Ok(lowered)
    }

    /// Move any `?`-desugar statements accumulated while lowering the current
    /// statement (see [`Lowerer::try_prelude`]) to the front of the statement's
    /// emitted output, preserving their left-to-right / inner-before-outer order.
    pub(crate) fn drain_try_prelude_into(&self, out: &mut Vec<IrStmt>) {
        let mut prelude = self.try_prelude.borrow_mut();
        out.extend(prelude.drain(..));
    }

    /// Lower a function body. A trailing bare expression statement is lowered
    /// against the function's return type so a final `some/none/ok/err` gets its
    /// context-directed type, mirroring the semantic final-expression rule.
    pub(crate) fn lower_function_body(
        &self,
        statements: &[Stmt],
        scope: &mut HashMap<String, TypeRef>,
    ) -> Result<Vec<IrStmt>, IrLoweringError> {
        let last_index = statements.len().checked_sub(1);
        let return_type = self.current_return_type.borrow().clone();
        let mut lowered = Vec::with_capacity(statements.len());
        for (index, statement) in statements.iter().enumerate() {
            match statement {
                Stmt::Unsafe { body, .. } => {
                    lowered.extend(self.lower_block(body, scope)?);
                }
                // A bare tail `match` returning a value stays a direct
                // `IrStmt::Match` (its taken arm's tail is the function's return
                // value), but now with the return type flowed into its arm tails
                // so a tail `none`/`ok`/`err` resolves.
                Stmt::Expr(Expr {
                    kind: ExprKind::Match { scrutinee, arms },
                    span,
                }) if Some(index) == last_index && !return_type.is_void() => {
                    let stmt =
                        self.lower_match(scrutinee, arms, Some(&return_type), *span, scope)?;
                    self.drain_try_prelude_into(&mut lowered);
                    lowered.push(stmt);
                }
                Stmt::Expr(expr) if Some(index) == last_index && !return_type.is_void() => {
                    let lowered_expr = self.lower_expr_expected(expr, Some(&return_type), scope)?;
                    self.drain_try_prelude_into(&mut lowered);
                    lowered.push(IrStmt::Expr(lowered_expr));
                }
                other => {
                    let stmt = self.lower_statement(other, scope)?;
                    self.drain_try_prelude_into(&mut lowered);
                    lowered.push(stmt);
                }
            }
        }
        Ok(lowered)
    }

    /// Lower a block whose value is its final expression, flowing an optional
    /// `expected` type into that tail expression (so a bare `none`/`ok`/`err` in
    /// tail position resolves against the expected type). Used for `match` arm
    /// bodies, mirroring `lower_function_body`'s tail handling. Non-tail
    /// statements are lowered exactly like `lower_block`.
    pub(crate) fn lower_block_value(
        &self,
        statements: &[Stmt],
        scope: &mut HashMap<String, TypeRef>,
        expected: Option<&TypeRef>,
    ) -> Result<Vec<IrStmt>, IrLoweringError> {
        let last_index = statements.len().checked_sub(1);
        let mut lowered = Vec::with_capacity(statements.len());
        for (index, statement) in statements.iter().enumerate() {
            match statement {
                Stmt::Unsafe { body, .. } => {
                    let inner_expected = if Some(index) == last_index {
                        expected
                    } else {
                        None
                    };
                    lowered.extend(self.lower_block_value(body, scope, inner_expected)?);
                }
                Stmt::Expr(expr) if Some(index) == last_index => {
                    let lowered_expr = self.lower_expr_expected(expr, expected, scope)?;
                    self.drain_try_prelude_into(&mut lowered);
                    lowered.push(IrStmt::Expr(lowered_expr));
                }
                other => {
                    let stmt = self.lower_statement(other, scope)?;
                    self.drain_try_prelude_into(&mut lowered);
                    lowered.push(stmt);
                }
            }
        }
        Ok(lowered)
    }

    pub(crate) fn lower_statement(
        &self,
        statement: &Stmt,
        scope: &mut HashMap<String, TypeRef>,
    ) -> Result<IrStmt, IrLoweringError> {
        match statement {
            Stmt::Let {
                name,
                ty,
                value,
                span,
            } => {
                let value = self.lower_expr_expected(value, ty.as_ref(), scope)?;
                let binding_type = ty.clone().unwrap_or_else(|| value.ty.clone());
                scope.insert(name.clone(), binding_type.clone());
                Ok(IrStmt::Let {
                    name: name.clone(),
                    ty: binding_type,
                    value,
                    span: *span,
                })
            }
            Stmt::Assign {
                name,
                path,
                op,
                value,
                span,
            } => {
                let path = path
                    .iter()
                    .map(|place| self.lower_place(place, scope))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut value = self.lower_expr(value, scope)?;
                // `s += c` where `s: string` and `c: char` coerces the char to a
                // string, matching `s + c`. Only the bare-local string target
                // needs this here; the AST interpreter handles it directly.
                if *op == AssignOp::Add
                    && path.is_empty()
                    && value.ty == TypeRef::new("char")
                    && scope.get(name).map(|t| t.name.as_str()) == Some("string")
                {
                    value = self.to_string_wrap(value);
                }
                Ok(IrStmt::Assign {
                    name: name.clone(),
                    path,
                    op: *op,
                    value,
                    span: *span,
                })
            }
            Stmt::Return(expr) => {
                let return_type = self.current_return_type.borrow().clone();
                Ok(IrStmt::Return(
                    expr.as_ref()
                        .map(|expr| self.lower_expr_expected(expr, Some(&return_type), scope))
                        .transpose()?,
                ))
            }
            Stmt::Break(span) => Ok(IrStmt::Break(*span)),
            Stmt::Continue(span) => Ok(IrStmt::Continue(*span)),
            // A `match` reaches lowering wrapped in a `Stmt::Expr`; lower it to a
            // dedicated `IrStmt::Match` so it threads through the IR and bytecode
            // backends and optimizers exactly like `try`.
            Stmt::Expr(Expr {
                kind: ExprKind::Match { scrutinee, arms },
                span,
            }) => self.lower_match(scrutinee, arms, None, *span, scope),
            Stmt::Expr(expr) => Ok(IrStmt::Expr(self.lower_expr(expr, scope)?)),
            Stmt::If {
                branches,
                else_body,
                span,
            } => {
                let branches = branches
                    .iter()
                    .map(|branch| {
                        let condition = self.lower_expr(&branch.condition, scope)?;
                        let mut branch_scope = scope.clone();
                        let body = self.lower_block(&branch.body, &mut branch_scope)?;
                        Ok(IrIfBranch { condition, body })
                    })
                    .collect::<Result<Vec<_>, IrLoweringError>>()?;
                let mut else_scope = scope.clone();
                let else_body = self.lower_block(else_body, &mut else_scope)?;
                Ok(IrStmt::If {
                    branches,
                    else_body,
                    span: *span,
                })
            }
            Stmt::While {
                condition,
                body,
                span,
            } => {
                let condition = self.lower_expr(condition, scope)?;
                let mut loop_scope = scope.clone();
                let body = self.lower_block(body, &mut loop_scope)?;
                Ok(IrStmt::While {
                    condition,
                    body,
                    span: *span,
                })
            }
            Stmt::For {
                name,
                start,
                end,
                step,
                body,
                span,
            } => {
                let start = self.lower_expr(start, scope)?;
                let end = self.lower_expr(end, scope)?;
                let step = step
                    .as_ref()
                    .map(|step| self.lower_expr(step, scope))
                    .transpose()?;
                let mut loop_scope = scope.clone();
                loop_scope.insert(name.clone(), TypeRef::new("i64"));
                let body = self.lower_block(body, &mut loop_scope)?;
                Ok(IrStmt::For {
                    name: name.clone(),
                    start,
                    end,
                    step,
                    body,
                    span: *span,
                })
            }
            // `for x in coll` desugars to an index-based `for` over `0..len-1`.
            // The collection is bound to a hidden local (via the prelude) so it is
            // evaluated exactly once, then each element is read by `[]` (arrays and
            // strings) or `get` (lists) into `x` as the loop body's first binding.
            Stmt::ForEach {
                name,
                iterable,
                body,
                span,
            } => {
                let coll = self.lower_expr(iterable, scope)?;
                let coll_ty = coll.ty.clone();
                let elem_ty = if coll_ty.name == "string" {
                    TypeRef::new("char")
                } else {
                    coll_ty
                        .array_element()
                        .or_else(|| coll_ty.list_element())
                        .ok_or_else(|| {
                            IrLoweringError::new(
                                "`for … in` requires an array, list, or string",
                                Some(*span),
                            )
                        })?
                };
                // A bare variable iterable (`for x in xs`) is re-read for free, so
                // reference it directly — no hidden copy, which also keeps the
                // native backend's array-length inference intact. Only a computed
                // iterable is bound to a hidden local (evaluated once).
                let coll_binding = match &coll.kind {
                    IrExprKind::Variable(name) => Some(name.clone()),
                    _ => None,
                };
                let coll_name = coll_binding
                    .clone()
                    .unwrap_or_else(|| format!("__foreach_coll_{}_{}", span.line, span.column));
                let idx_name = format!("__foreach_idx_{}_{}", span.line, span.column);
                let i64_ty = TypeRef::new("i64");
                let coll_var = IrExpr {
                    kind: IrExprKind::Variable(coll_name.clone()),
                    ty: coll_ty.clone(),
                    span: *span,
                };
                let idx_var = IrExpr {
                    kind: IrExprKind::Variable(idx_name.clone()),
                    ty: i64_ty.clone(),
                    span: *span,
                };
                // element read: `coll[idx]` (array/string) or `get(coll, idx)` (list)
                let element = if coll_ty.list_element().is_some() {
                    IrExpr {
                        kind: IrExprKind::Call {
                            name: "get".to_string(),
                            args: vec![coll_var.clone(), idx_var.clone()],
                        },
                        ty: elem_ty.clone(),
                        span: *span,
                    }
                } else {
                    IrExpr {
                        kind: IrExprKind::Index {
                            target: Box::new(coll_var.clone()),
                            index: Box::new(idx_var.clone()),
                        },
                        ty: elem_ty.clone(),
                        span: *span,
                    }
                };
                // end = len(coll) - 1
                let end = IrExpr {
                    kind: IrExprKind::Binary {
                        left: Box::new(IrExpr {
                            kind: IrExprKind::Call {
                                name: "len".to_string(),
                                args: vec![coll_var],
                            },
                            ty: i64_ty.clone(),
                            span: *span,
                        }),
                        op: BinaryOp::Subtract,
                        right: Box::new(IrExpr {
                            kind: IrExprKind::Integer(1),
                            ty: i64_ty.clone(),
                            span: *span,
                        }),
                    },
                    ty: i64_ty.clone(),
                    span: *span,
                };
                // Lower the user body first (drains any nested preludes correctly),
                // then prepend `let x = <element>`.
                let mut loop_scope = scope.clone();
                loop_scope.insert(idx_name.clone(), i64_ty.clone());
                loop_scope.insert(name.clone(), elem_ty.clone());
                let mut for_body = vec![IrStmt::Let {
                    name: name.clone(),
                    ty: elem_ty,
                    value: element,
                    span: *span,
                }];
                for_body.extend(self.lower_block(body, &mut loop_scope)?);
                // Bind a computed collection once, before the loop (a bare variable
                // needs no binding — it is referenced directly above).
                if coll_binding.is_none() {
                    self.try_prelude.borrow_mut().push(IrStmt::Let {
                        name: coll_name,
                        ty: coll_ty,
                        value: coll,
                        span: *span,
                    });
                }
                Ok(IrStmt::For {
                    name: idx_name,
                    start: IrExpr {
                        kind: IrExprKind::Integer(0),
                        ty: i64_ty.clone(),
                        span: *span,
                    },
                    end,
                    step: None,
                    body: for_body,
                    span: *span,
                })
            }
            Stmt::Loop { body, span } => {
                let mut loop_scope = scope.clone();
                let body = self.lower_block(body, &mut loop_scope)?;
                Ok(IrStmt::Loop { body, span: *span })
            }
            // Inline assembly lowers straight through to an `IrStmt::Asm` carrying
            // the raw bytes. Semantics has already validated each byte is 0..=255,
            // so the `as u8` truncation is exact.
            Stmt::Asm { bytes, span } => Ok(IrStmt::Asm {
                bytes: bytes.iter().map(|byte| *byte as u8).collect(),
                span: *span,
            }),
            // A region declaration lowers to a `region_create` marker call so
            // its metadata flows through memory analysis as a RegionCreate op.
            Stmt::Region(decl) => {
                let mut args = vec![
                    IrExpr {
                        kind: IrExprKind::String(decl.name.clone()),
                        ty: TypeRef::new("string"),
                        span: decl.span,
                    },
                    IrExpr {
                        kind: IrExprKind::Integer(decl.size),
                        ty: TypeRef::new("i64"),
                        span: decl.span,
                    },
                ];
                if let Some(align) = decl.align {
                    args.push(IrExpr {
                        kind: IrExprKind::Integer(align),
                        ty: TypeRef::new("i64"),
                        span: decl.span,
                    });
                }
                Ok(IrStmt::Expr(IrExpr {
                    kind: IrExprKind::Call {
                        name: "region_create".to_string(),
                        args,
                    },
                    ty: TypeRef::new("void"),
                    span: decl.span,
                }))
            }
            Stmt::Throw { value, span } => Ok(IrStmt::Throw {
                value: self.lower_expr(value, scope)?,
                span: *span,
            }),
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                span,
            } => {
                let mut try_scope = scope.clone();
                let body = self.lower_block(body, &mut try_scope)?;
                let mut catch_scope = scope.clone();
                catch_scope.insert(catch_name.clone(), TypeRef::new("string"));
                let catch_body = self.lower_block(catch_body, &mut catch_scope)?;
                Ok(IrStmt::Try {
                    body,
                    catch_name: catch_name.clone(),
                    catch_body,
                    span: *span,
                })
            }
            // `unsafe` blocks are flattened in `lower_block`; reaching here means
            // a lone unsafe statement, which we lower transparently by inlining.
            Stmt::Unsafe { body, span } => {
                let mut lowered = self.lower_block(body, scope)?;
                match lowered.len() {
                    1 => Ok(lowered.remove(0)),
                    // An empty or multi-statement unsafe body cannot collapse to
                    // one IR statement; represent it as a always-false guard-free
                    // block via an `if false` is overkill, so surface it as a
                    // lowering error to be handled by the flattening path.
                    _ => Err(IrLoweringError::new(
                        "unsafe block must be lowered by lower_block".to_string(),
                        Some(*span),
                    )),
                }
            }
        }
    }

    /// Lower a `match` to an `IrStmt::Match`. Each arm's payload bindings are
    /// typed by the owning variant's declared payload types and inserted into a
    /// per-arm scope, so arm bodies lower against the right binding types.
    pub(crate) fn lower_match(
        &self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        expected: Option<&TypeRef>,
        span: Span,
        scope: &mut HashMap<String, TypeRef>,
    ) -> Result<IrStmt, IrLoweringError> {
        let scrutinee = self.lower_expr(scrutinee, scope)?;
        let scrutinee_ty = scrutinee.ty.clone();
        let mut lowered_arms = Vec::with_capacity(arms.len());
        for arm in arms {
            let mut arm_scope = scope.clone();
            let pattern = match &arm.pattern {
                MatchPattern::Wildcard => IrMatchPattern::Wildcard,
                MatchPattern::Variant { name, bindings } => {
                    let payload = self.variant_binding_types(&scrutinee_ty, name);
                    for (binding, ty) in bindings.iter().zip(payload.iter()) {
                        arm_scope.insert(binding.clone(), ty.clone());
                    }
                    IrMatchPattern::Variant {
                        name: name.clone(),
                        bindings: bindings.clone(),
                    }
                }
            };
            // Flow the expected result type into each arm's tail expression, so a
            // tail `none`/`ok`/`err` resolves against it exactly as semantics does.
            let body = self.lower_block_value(&arm.body, &mut arm_scope, expected)?;
            lowered_arms.push(IrMatchArm { pattern, body });
        }
        Ok(IrStmt::Match {
            scrutinee,
            arms: lowered_arms,
            span,
        })
    }

    /// Desugar a value-position `match` (a `let`/assignment RHS, a `return`, or a
    /// nested tail expression) into a hoisted temporary plus an `IrStmt::Match`,
    /// returning a reference to the temporary. This mirrors the `?` and
    /// inline-conditional desugars so the IR interpreter, bytecode VM, and (via
    /// the existing demote-on-`match` gate) native/WASM backends need no
    /// match-expression node:
    ///
    /// ```text
    /// let __match_s_N = <scrutinee>          # bind the scrutinee once
    /// let __match_v_N: T = __match_s_N       # dead init, overwritten by the taken arm
    /// match __match_s_N
    ///     <arm pattern> -> <arm body...>; __match_v_N = <arm tail>
    ///     ...
    /// ```
    ///
    /// `match` is exhaustive (semantics enforces this), so exactly one arm runs
    /// and writes `__match_v_N` before it is read; the dead initializer's value
    /// (the scrutinee) is never observed. The initializer is a valid value of any
    /// type, so — unlike the inline conditional's zero init — a `match` may yield
    /// an enum, struct, or collection result, not only a scalar/`string`. A
    /// diverging arm (`return`/`throw`) leaves control before the read, so it is
    /// left unrewritten. The interpreters are dynamically typed and any function
    /// containing a `match` is demoted to the interpreter, so the declared temp
    /// type never has to match the dead initializer's runtime type.
    pub(crate) fn desugar_match(
        &self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        expected: Option<&TypeRef>,
        span: Span,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<IrExpr, IrLoweringError> {
        // Isolate the prelude buffer: `lower_block` drains the whole buffer per
        // statement, so it must be empty while we lower arm bodies. We take any
        // prelude accumulated so far aside and restore it, in order, at the end.
        let outer_prelude: Vec<IrStmt> = std::mem::take(&mut self.try_prelude.borrow_mut());

        let scrutinee_ir = self.lower_expr(scrutinee, scope)?;
        let scrutinee_ty = scrutinee_ir.ty.clone();
        let scrutinee_prelude: Vec<IrStmt> = std::mem::take(&mut self.try_prelude.borrow_mut());

        let id = self.next_match_temp.get();
        self.next_match_temp.set(id + 1);
        let s_name = format!("__match_s_{id}");
        let v_name = format!("__match_v_{id}");

        let mut lowered_arms = Vec::with_capacity(arms.len());
        let mut result_ty: Option<TypeRef> = expected.cloned();
        for arm in arms {
            let mut arm_scope = scope.clone();
            let pattern = match &arm.pattern {
                MatchPattern::Wildcard => IrMatchPattern::Wildcard,
                MatchPattern::Variant { name, bindings } => {
                    let payload = self.variant_binding_types(&scrutinee_ty, name);
                    for (binding, ty) in bindings.iter().zip(payload.iter()) {
                        arm_scope.insert(binding.clone(), ty.clone());
                    }
                    IrMatchPattern::Variant {
                        name: name.clone(),
                        bindings: bindings.clone(),
                    }
                }
            };
            // The arm body's own hoists drain cleanly into `body` because the
            // prelude buffer is empty here. Flow the expected result type into
            // the tail so a `none`/`ok`/`err` arm resolves against it.
            let mut body = self.lower_block_value(&arm.body, &mut arm_scope, expected)?;
            // Rewrite the arm's tail expression into `__match_v_N = <tail>`. A
            // diverging tail (`return`/`throw`) or a valueless arm is left as-is.
            if matches!(body.last(), Some(IrStmt::Expr(_)))
                && let Some(IrStmt::Expr(value)) = body.pop()
            {
                if result_ty.is_none() {
                    result_ty = Some(value.ty.clone());
                }
                body.push(IrStmt::Assign {
                    name: v_name.clone(),
                    path: Vec::new(),
                    op: AssignOp::Replace,
                    value,
                    span,
                });
            }
            lowered_arms.push(IrMatchArm { pattern, body });
        }

        let result_ty = result_ty.unwrap_or_else(|| TypeRef::new("void"));

        let s_let = IrStmt::Let {
            name: s_name.clone(),
            ty: scrutinee_ty.clone(),
            value: scrutinee_ir,
            span,
        };
        let v_let = IrStmt::Let {
            name: v_name.clone(),
            ty: result_ty.clone(),
            value: IrExpr {
                kind: IrExprKind::Variable(s_name.clone()),
                ty: scrutinee_ty.clone(),
                span,
            },
            span,
        };
        let match_stmt = IrStmt::Match {
            scrutinee: IrExpr {
                kind: IrExprKind::Variable(s_name),
                ty: scrutinee_ty,
                span,
            },
            arms: lowered_arms,
            span,
        };

        {
            let mut prelude = self.try_prelude.borrow_mut();
            *prelude = outer_prelude;
            prelude.extend(scrutinee_prelude);
            prelude.push(s_let);
            prelude.push(v_let);
            prelude.push(match_stmt);
        }

        Ok(IrExpr {
            kind: IrExprKind::Variable(v_name),
            ty: result_ty,
            span,
        })
    }

    /// The payload binding types of `variant` for a scrutinee of type
    /// `scrutinee_ty`. Handles user enums (nominal name) plus the built-in
    /// `option<U>` (`some(U)`) and `result<T, E>` (`ok(T)`/`err(E)`) generics,
    /// whose payloads are read from the scrutinee's type arguments.
    pub(crate) fn variant_binding_types(
        &self,
        scrutinee_ty: &TypeRef,
        variant: &str,
    ) -> Vec<TypeRef> {
        if let Some(payload) = scrutinee_ty.option_element() {
            return match variant {
                "some" => vec![payload],
                _ => Vec::new(),
            };
        }
        if let Some((ok_ty, err_ty)) = scrutinee_ty.result_args() {
            return match variant {
                "ok" => vec![ok_ty],
                "err" => vec![err_ty],
                _ => Vec::new(),
            };
        }
        // `scrutinee_ty` may be a concrete generic-enum instantiation spelling
        // such as `Opt<i64>`: its head (`Opt`) resolves the declaration and its
        // type arguments (`[i64]`) are substituted for the declaration's type
        // parameters, so a `present x` arm on `Opt<i64>` binds `x: i64`. A
        // non-generic enum's payload types are returned verbatim.
        let (head, args) = match scrutinee_ty.name.find('<') {
            Some(open)
                if scrutinee_ty.name.ends_with('>') && !scrutinee_ty.name.starts_with("fn(") =>
            {
                let head = scrutinee_ty.name[..open].to_string();
                let args = TypeRef::new(scrutinee_ty.name.clone())
                    .generic_args(&head)
                    .unwrap_or_default();
                (head, args)
            }
            _ => (scrutinee_ty.name.clone(), Vec::new()),
        };
        let Some(declaration) = self
            .program
            .enums
            .iter()
            .find(|declaration| declaration.name == head)
        else {
            return Vec::new();
        };
        let Some(payload) = declaration
            .variants
            .iter()
            .find(|v| v.name == variant)
            .map(|v| v.payload.clone())
        else {
            return Vec::new();
        };
        if declaration.type_params.is_empty() {
            return payload;
        }
        let mut subst: HashMap<String, TypeRef> = HashMap::new();
        for (param, arg) in declaration.type_params.iter().zip(args.iter()) {
            subst.insert(param.name.clone(), arg.clone());
        }
        payload
            .iter()
            .map(|ty| lullaby_semantics::substitute_type(ty, &subst))
            .collect()
    }

    pub(crate) fn lower_place(
        &self,
        place: &Place,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<IrPlace, IrLoweringError> {
        match place {
            Place::Field(field) => Ok(IrPlace::Field(field.clone())),
            Place::Index(expr) => Ok(IrPlace::Index(self.lower_expr(expr, scope)?)),
        }
    }

    pub(crate) fn lower_expr(
        &self,
        expr: &Expr,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<IrExpr, IrLoweringError> {
        self.lower_expr_expected(expr, None, scope)
    }

    /// Lower an expression, optionally carrying a contextual expected type. The
    /// expected type flows from `let` annotations and `return`/final-expression
    /// sites so `none`/`ok`/`err` — which cannot be typed from their payload
    /// alone — lower to the correct `option`/`result` type. Every other
    /// expression ignores `expected` and lowers exactly as before.
    pub(crate) fn lower_expr_expected(
        &self,
        expr: &Expr,
        expected: Option<&TypeRef>,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<IrExpr, IrLoweringError> {
        // Built-in `option`/`result` construction is context-directed; resolve it
        // before the generic expression rules.
        if let Some(result) = self.lower_builtin_construction(expr, expected, scope) {
            return result;
        }
        let (kind, ty) = match &expr.kind {
            ExprKind::Integer(value) => (IrExprKind::Integer(*value), TypeRef::new("i64")),
            ExprKind::Float(value) => (IrExprKind::Float(*value), TypeRef::new("f64")),
            ExprKind::Bool(value) => (IrExprKind::Bool(*value), TypeRef::new("bool")),
            ExprKind::String(value) => (IrExprKind::String(value.clone()), TypeRef::new("string")),
            ExprKind::Char(value) => (IrExprKind::Char(*value), TypeRef::new("char")),
            ExprKind::Array(values) => {
                let values = values
                    .iter()
                    .map(|value| self.lower_expr(value, scope))
                    .collect::<Result<Vec<_>, _>>()?;
                let element_type =
                    values
                        .first()
                        .map(|value| value.ty.clone())
                        .ok_or_else(|| {
                            IrLoweringError::new(
                                "cannot lower empty array literal",
                                Some(expr.span),
                            )
                        })?;
                (
                    IrExprKind::Array(values),
                    TypeRef::new(format!("array<{}>", element_type.name)),
                )
            }
            ExprKind::Variable(name) => {
                if let Some(ty) = scope.get(name).cloned() {
                    (IrExprKind::Variable(name.clone()), ty)
                } else if let Some(enum_name) = self.enum_of_variant(name) {
                    // A bare name that is not a local but is a known unit variant
                    // is enum construction. Lower it to a variant `Call` (no args)
                    // so the interpreter and VM build the enum value uniformly.
                    (
                        IrExprKind::Call {
                            name: name.clone(),
                            args: Vec::new(),
                        },
                        TypeRef::new(enum_name),
                    )
                } else if let Some(signature) = self.signatures.get(name) {
                    // A bare name that is a declared top-level function lowers to a
                    // first-class function value of type `fn(params) -> ret`. It
                    // stays a `Variable`, so the interpreter and VM turn it into a
                    // `Value::Func`.
                    (
                        IrExprKind::Variable(name.clone()),
                        function_type(&signature.params, &signature.return_type),
                    )
                } else {
                    return Err(IrLoweringError::new(
                        format!("unknown variable `{name}`"),
                        Some(expr.span),
                    ));
                }
            }
            ExprKind::Index { target, index } => {
                let target = self.lower_expr(target, scope)?;
                let index = self.lower_expr(index, scope)?;
                // `s[i]` on a string yields a `char`; otherwise the array element.
                let ty = if target.ty.name == "string" {
                    TypeRef::new("char")
                } else {
                    target.ty.array_element().ok_or_else(|| {
                        IrLoweringError::new(
                            "index target is not an array or string",
                            Some(target.span),
                        )
                    })?
                };
                (
                    IrExprKind::Index {
                        target: Box::new(target),
                        index: Box::new(index),
                    },
                    ty,
                )
            }
            ExprKind::Unary { op, expr } => {
                let inner = self.lower_expr(expr, scope)?;
                // Bitwise NOT preserves the operand's integer type (`i64` or any
                // fixed-width kind); logical NOT is `bool`.
                let ty = match op {
                    UnaryOp::Not => TypeRef::new("bool"),
                    UnaryOp::BitNot | UnaryOp::Negate => inner.ty.clone(),
                };
                (
                    IrExprKind::Unary {
                        op: *op,
                        expr: Box::new(inner),
                    },
                    ty,
                )
            }
            ExprKind::Binary { left, op, right } => {
                let left = self.lower_expr(left, scope)?;
                let right = self.lower_expr(right, scope)?;
                // `string + char` (either order) coerces the char to a string via
                // `to_string`, so every backend sees a plain two-string `+`.
                let (left, right) = self.coerce_string_char_add(*op, left, right);
                let ty = match op {
                    // `+` on two strings concatenates and yields a string.
                    BinaryOp::Add
                        if left.ty == TypeRef::new("string")
                            && right.ty == TypeRef::new("string") =>
                    {
                        TypeRef::new("string")
                    }
                    // Arithmetic preserves the operand's numeric type. Semantics
                    // guarantees both operands share one numeric type (i64/f64/f32
                    // or a fixed-width integer), so the result is that type; the
                    // two-string `+` concatenation is handled by the arm above.
                    // (The backends still derive float width/int kind structurally
                    // from leaf operands, so this only improves the node's recorded
                    // type — it does not change codegen eligibility or results.)
                    BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide => {
                        left.ty.clone()
                    }
                    // `%` (integer remainder) preserves the operand's integer type.
                    BinaryOp::Remainder => left.ty.clone(),
                    // Integer bitwise ops preserve the operand's integer type
                    // (`i64` or any fixed-width kind; both operands share it).
                    BinaryOp::BitAnd
                    | BinaryOp::BitOr
                    | BinaryOp::BitXor
                    | BinaryOp::Shl
                    | BinaryOp::Shr => left.ty.clone(),
                    BinaryOp::Equal
                    | BinaryOp::NotEqual
                    | BinaryOp::Less
                    | BinaryOp::LessEqual
                    | BinaryOp::Greater
                    | BinaryOp::GreaterEqual
                    | BinaryOp::And
                    | BinaryOp::Or => TypeRef::new("bool"),
                };
                (
                    IrExprKind::Binary {
                        left: Box::new(left),
                        op: *op,
                        right: Box::new(right),
                    },
                    ty,
                )
            }
            ExprKind::Call { name, args } => {
                let args = self.lower_call_args(name, args, expected, scope)?;
                // A call whose name is a function-typed local dispatches through
                // that function value; its result type is the function type's
                // return type. The `Call` stays name-based so the interpreter and
                // VM resolve the held `Value::Func` at runtime.
                let ty = match scope.get(name).and_then(TypeRef::function_signature) {
                    Some((_, return_type)) => return_type,
                    None => self.call_return_type(name, &args, expr.span)?,
                };
                (
                    IrExprKind::Call {
                        name: name.clone(),
                        args,
                    },
                    ty,
                )
            }
            ExprKind::StructLiteral { name, fields } => {
                // Reorder named fields into declared order and emit the same
                // positional construction the runtime already knows how to build.
                let order = self
                    .program
                    .structs
                    .iter()
                    .find(|declaration| &declaration.name == name)
                    .ok_or_else(|| {
                        IrLoweringError::new(
                            format!("`{name}` is not a struct type"),
                            Some(expr.span),
                        )
                    })?
                    .fields
                    .iter()
                    .map(|field| field.name.clone())
                    .collect::<Vec<_>>();
                let mut lowered = Vec::with_capacity(fields.len());
                for (field_name, value) in fields {
                    lowered.push((field_name.clone(), self.lower_expr(value, scope)?));
                }
                let args = order
                    .iter()
                    .map(|declared| {
                        lowered
                            .iter()
                            .find(|(n, _)| n == declared)
                            .map(|(_, value)| value.clone())
                            .ok_or_else(|| {
                                IrLoweringError::new(
                                    format!("missing field `{declared}` for `{name}`"),
                                    Some(expr.span),
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                (
                    IrExprKind::Call {
                        name: name.clone(),
                        args,
                    },
                    TypeRef::new(name.clone()),
                )
            }
            ExprKind::Field { target, field } => {
                let target = self.lower_expr(target, scope)?;
                let ty = self
                    .struct_field_type(&target.ty.name, field)
                    .ok_or_else(|| {
                        IrLoweringError::new(
                            format!("unknown field `{field}` on `{}`", target.ty.name),
                            Some(expr.span),
                        )
                    })?;
                (
                    IrExprKind::Field {
                        target: Box::new(target),
                        field: field.clone(),
                    },
                    ty,
                )
            }
            // A `match` in value position (a `let`/assignment RHS, a `return`, or
            // a nested tail) is desugared here into a hoisted temporary plus an
            // `IrStmt::Match` whose arms write that temporary, mirroring the `?`
            // and inline-conditional desugars. The result flows back as a
            // reference to the temporary, so no backend needs a match-expression
            // node. A `match` in bare statement/tail position is still lowered
            // directly to `IrStmt::Match` by `lower_statement`/`lower_match`.
            ExprKind::Match { scrutinee, arms } => {
                let temp = self.desugar_match(scrutinee, arms, expected, expr.span, scope)?;
                (temp.kind, temp.ty)
            }
            ExprKind::Await { expr: inner } => {
                // `await e` requires `e: Future<T>`; the awaited result type `T`
                // is the future's inner argument. Semantics has already checked
                // this, so a non-future operand here is a lowering bug.
                let inner = self.lower_expr(inner, scope)?;
                let ty = inner.ty.generic_arg("Future").ok_or_else(|| {
                    IrLoweringError::new(
                        format!("`await` operand has non-future type `{}`", inner.ty.name),
                        Some(expr.span),
                    )
                })?;
                (
                    IrExprKind::Await {
                        expr: Box::new(inner),
                    },
                    ty,
                )
            }
            // Postfix `EXPR?` is desugared here so no `Try` node ever reaches the
            // IR (and the native/WASM backends never see it). We hoist the
            // supporting `let`/`match`/`return` into the statement prelude and
            // rewrite this position to a reference to the success temporary. The
            // recursive `lower_expr` on the operand desugars any inner `?` first,
            // so nested `?` hoist inner-before-outer.
            ExprKind::Try(inner) => {
                let (kind, ty) = self.desugar_try(inner, expr.span, scope)?;
                (kind, ty)
            }
            // Inline conditional `THEN if COND else ELSE`. Desugared here into a
            // hoisted temporary plus an `if` statement so the IR interpreter,
            // bytecode VM, native, and WASM backends need no conditional node
            // (mirrors the `?` desugar). The result flows back as a reference to
            // the temporary.
            ExprKind::Conditional {
                cond,
                then_branch,
                else_branch,
            } => {
                let temp = self.desugar_conditional(
                    cond,
                    then_branch,
                    else_branch,
                    expected,
                    expr.span,
                    scope,
                )?;
                (temp.kind, temp.ty)
            }
            // Membership `VALUE in COLLECTION` desugars to a builtin call, so no
            // backend needs an `in` node: `contains(collection, value)` for a
            // string (a char value coerced via `to_string`), or
            // `list_contains(collection, value)` for a `list<T>`.
            ExprKind::In { value, collection } => {
                let coll = self.lower_expr(collection, scope)?;
                let val = self.lower_expr(value, scope)?;
                let call = if coll.ty.name == "string" {
                    let needle = if val.ty.name == "char" {
                        self.to_string_wrap(val)
                    } else {
                        val
                    };
                    IrExprKind::Call {
                        name: "contains".to_string(),
                        args: vec![coll, needle],
                    }
                } else {
                    IrExprKind::Call {
                        name: "list_contains".to_string(),
                        args: vec![coll, val],
                    }
                };
                (call, TypeRef::new("bool"))
            }
            // String slice `target[start:end]` desugars to
            // `substring(target, start, end)`. An omitted `start` is `0`; an
            // omitted `end` is `len(target)` — in which case `target` is bound to
            // a temp so it is evaluated exactly once, before `start`.
            ExprKind::Slice { target, start, end } => {
                let i64_ty = TypeRef::new("i64");
                let string_ty = TypeRef::new("string");
                let span = expr.span;
                let target_ir = self.lower_expr(target, scope)?;

                let (target_arg, len_end): (IrExpr, Option<IrExpr>) = if end.is_none() {
                    let id = self.next_cond_temp.get();
                    self.next_cond_temp.set(id + 1);
                    let temp = format!("__slice_{id}");
                    self.try_prelude.borrow_mut().push(IrStmt::Let {
                        name: temp.clone(),
                        ty: string_ty.clone(),
                        value: target_ir,
                        span,
                    });
                    let var = IrExpr {
                        kind: IrExprKind::Variable(temp),
                        ty: string_ty.clone(),
                        span,
                    };
                    let len_call = IrExpr {
                        kind: IrExprKind::Call {
                            name: "len".to_string(),
                            args: vec![var.clone()],
                        },
                        ty: i64_ty.clone(),
                        span,
                    };
                    (var, Some(len_call))
                } else {
                    (target_ir, None)
                };

                let start_ir = match start {
                    Some(start) => self.lower_expr(start, scope)?,
                    None => IrExpr {
                        kind: IrExprKind::Integer(0),
                        ty: i64_ty.clone(),
                        span,
                    },
                };
                let end_ir = match end {
                    Some(end) => self.lower_expr(end, scope)?,
                    None => len_end.expect("len bound for omitted slice end"),
                };

                (
                    IrExprKind::Call {
                        name: "substring".to_string(),
                        args: vec![target_arg, start_ir, end_ir],
                    },
                    string_ty,
                )
            }
            // Lower a closure literal: lower its body in a child scope that layers
            // the closure parameters over the enclosing scope, register the lowered
            // `(param names, body)` in the module's closure table keyed by the
            // parse-order id, and emit a body-less `Closure { id }` node whose type
            // is `fn(param types) -> typeof(body)`. This mirrors the semantics
            // typing so IR types agree with the checker.
            ExprKind::Closure { id, params, body } => {
                let mut body_scope = scope.clone();
                for param in params {
                    body_scope.insert(param.name.clone(), param.ty.clone());
                }
                let lowered_body = self.lower_expr(body, &body_scope)?;
                let param_types: Vec<TypeRef> =
                    params.iter().map(|param| param.ty.clone()).collect();
                let ty = function_type(&param_types, &lowered_body.ty);
                self.closures.borrow_mut().push(IrClosureDef {
                    id: *id,
                    params: params.iter().map(|param| param.name.clone()).collect(),
                    body: lowered_body,
                });
                (IrExprKind::Closure { id: *id }, ty)
            }
        };

        Ok(IrExpr {
            kind,
            ty,
            span: expr.span,
        })
    }

    /// Desugar a postfix `EXPR?`. Lowers the operand, hoists the propagation
    /// scaffolding into [`Lowerer::try_prelude`], and returns the `(kind, ty)` of
    /// a reference to the freshly bound success temporary `__try_v_N: T`.
    ///
    /// For a `result<T, E>` operand it emits, in order:
    ///
    /// ```text
    /// let __try_q_N = <operand>          # result<T, E>
    /// let __try_v_N: T = __try_q_N       # initial binding, overwritten below
    /// match __try_q_N
    ///     ok(__try_ok_N) -> __try_v_N = __try_ok_N
    ///     err(__try_err_N) -> return err(__try_err_N)
    /// ```
    ///
    /// and for an `option<T>` operand the analogous `some`/`none` shape. The
    /// initial binding is immediately overwritten by the `ok`/`some` arm before
    /// any read, and the failure arm `return`s first, so its value is never
    /// observed; the interpreters are dynamically typed and the native/WASM
    /// backends demote any function containing a `match` to the interpreter, so
    /// the desugared IR runs identically on every backend.
    pub(crate) fn desugar_try(
        &self,
        inner: &Expr,
        span: Span,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<(IrExprKind, TypeRef), IrLoweringError> {
        let operand = self.lower_expr(inner, scope)?;
        let operand_ty = operand.ty.clone();
        let return_type = self.current_return_type.borrow().clone();

        // Fresh, collision-free temp names for this `?` site.
        let id = self.next_try_temp.get();
        self.next_try_temp.set(id + 1);
        let q_name = format!("__try_q_{id}");
        let v_name = format!("__try_v_{id}");
        let bind_name = format!("__try_x_{id}");

        // Resolve `(success variant, failure variant, payload type T)` from the
        // operand type. Semantics guarantees the operand is an `option`/`result`
        // and the return type is compatible, so anything else is a lowering bug.
        let (success_variant, failure_variant, payload_ty) =
            if let Some((ok_ty, _)) = operand_ty.result_args() {
                ("ok", "err", ok_ty)
            } else if let Some(payload) = operand_ty.option_element() {
                ("some", "none", payload)
            } else {
                return Err(IrLoweringError::new(
                    format!(
                        "`?` operand has non-option/result type `{}`",
                        operand_ty.name
                    ),
                    Some(span),
                ));
            };

        // `let __try_q_N = <operand>`
        let q_let = IrStmt::Let {
            name: q_name.clone(),
            ty: operand_ty.clone(),
            value: operand,
            span,
        };

        // `let __try_v_N: T = __try_q_N` (initial binding, overwritten by the
        // success arm before any read).
        let v_let = IrStmt::Let {
            name: v_name.clone(),
            ty: payload_ty.clone(),
            value: IrExpr {
                kind: IrExprKind::Variable(q_name.clone()),
                ty: operand_ty.clone(),
                span,
            },
            span,
        };

        // Success arm: `variant(__try_x_N) -> __try_v_N = __try_x_N`.
        let success_arm = IrMatchArm {
            pattern: IrMatchPattern::Variant {
                name: success_variant.to_string(),
                bindings: vec![bind_name.clone()],
            },
            body: vec![IrStmt::Assign {
                name: v_name.clone(),
                path: Vec::new(),
                op: AssignOp::Replace,
                value: IrExpr {
                    kind: IrExprKind::Variable(bind_name.clone()),
                    ty: payload_ty.clone(),
                    span,
                },
                span,
            }],
        };

        // Failure arm: `err(__try_x_N) -> return err(__try_x_N)` (or the `none`
        // analogue), rebuilding the failure value at the function's return type.
        let failure_arm = if failure_variant == "err" {
            let err_bind = format!("__try_e_{id}");
            let (_, err_ty) = operand_ty.result_args().ok_or_else(|| {
                IrLoweringError::new(
                    format!("`?` operand `{}` is not a result", operand_ty.name),
                    Some(span),
                )
            })?;
            IrMatchArm {
                pattern: IrMatchPattern::Variant {
                    name: "err".to_string(),
                    bindings: vec![err_bind.clone()],
                },
                body: vec![IrStmt::Return(Some(IrExpr {
                    kind: IrExprKind::Call {
                        name: "err".to_string(),
                        args: vec![IrExpr {
                            kind: IrExprKind::Variable(err_bind),
                            ty: err_ty,
                            span,
                        }],
                    },
                    ty: return_type.clone(),
                    span,
                }))],
            }
        } else {
            // `none -> return none` (a unit variant lowered as a no-arg `Call`,
            // matching how bare `none` construction lowers elsewhere).
            IrMatchArm {
                pattern: IrMatchPattern::Variant {
                    name: "none".to_string(),
                    bindings: Vec::new(),
                },
                body: vec![IrStmt::Return(Some(IrExpr {
                    kind: IrExprKind::Call {
                        name: "none".to_string(),
                        args: Vec::new(),
                    },
                    ty: return_type.clone(),
                    span,
                }))],
            }
        };

        let match_stmt = IrStmt::Match {
            scrutinee: IrExpr {
                kind: IrExprKind::Variable(q_name),
                ty: operand_ty,
                span,
            },
            arms: vec![success_arm, failure_arm],
            span,
        };

        // Hoist the scaffolding, in order, ahead of the statement being lowered.
        {
            let mut prelude = self.try_prelude.borrow_mut();
            prelude.push(q_let);
            prelude.push(v_let);
            prelude.push(match_stmt);
        }

        Ok((IrExprKind::Variable(v_name), payload_ty))
    }

    /// Desugar an inline conditional `THEN if COND else ELSE` into a hoisted
    /// temporary plus an `if` statement, returning a reference to the temporary.
    ///
    /// ```text
    /// let __cond_N: T = <zero of T>      # dead init, overwritten by both arms
    /// if COND:
    ///     <THEN's own hoisted prelude>
    ///     __cond_N = THEN
    /// else:
    ///     <ELSE's own hoisted prelude>
    ///     __cond_N = ELSE
    /// ```
    ///
    /// `COND` is evaluated unconditionally, so its prelude stays in the outer
    /// statement prelude; each branch's own prelude (from a nested `?`/ternary)
    /// is captured and placed inside that branch so it runs only when taken. The
    /// temporary's zero initializer is never observed (both arms assign before
    /// any read); semantics restricts the result type to a scalar or `string`
    /// (`L0436`), so a correctly-typed zero always exists and every backend can
    /// compile the desugared `if`.
    pub(crate) fn desugar_conditional(
        &self,
        cond: &Expr,
        then_branch: &Expr,
        else_branch: &Expr,
        expected: Option<&TypeRef>,
        span: Span,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<IrExpr, IrLoweringError> {
        // The condition is always evaluated: lower it normally so any prelude it
        // produces stays ahead of the `if` in the outer statement prelude.
        let cond_ir = self.lower_expr(cond, scope)?;
        // Each branch is only evaluated when taken: capture its own prelude so
        // nested hoists land inside the branch body, not before the `if`.
        let (then_prelude, then_ir) = self.lower_captured(then_branch, expected, scope)?;
        let (else_prelude, else_ir) = self.lower_captured(else_branch, expected, scope)?;

        let result_ty = expected.cloned().unwrap_or_else(|| then_ir.ty.clone());
        let id = self.next_cond_temp.get();
        self.next_cond_temp.set(id + 1);
        let temp = format!("__cond_{id}");

        let zero = self.zero_ir_expr(&result_ty, span)?;
        self.try_prelude.borrow_mut().push(IrStmt::Let {
            name: temp.clone(),
            ty: result_ty.clone(),
            value: zero,
            span,
        });

        let mut then_body = then_prelude;
        then_body.push(IrStmt::Assign {
            name: temp.clone(),
            path: Vec::new(),
            op: AssignOp::Replace,
            value: then_ir,
            span,
        });
        let mut else_body = else_prelude;
        else_body.push(IrStmt::Assign {
            name: temp.clone(),
            path: Vec::new(),
            op: AssignOp::Replace,
            value: else_ir,
            span,
        });
        self.try_prelude.borrow_mut().push(IrStmt::If {
            branches: vec![IrIfBranch {
                condition: cond_ir,
                body: then_body,
            }],
            else_body,
            span,
        });

        Ok(IrExpr {
            kind: IrExprKind::Variable(temp),
            ty: result_ty,
            span,
        })
    }

    /// Wrap an expression in a `to_string(...)` call typed `string`. Used to
    /// coerce a `char` operand of string concatenation into a one-character
    /// string so every backend sees a plain string-valued operand.
    pub(crate) fn to_string_wrap(&self, expr: IrExpr) -> IrExpr {
        let span = expr.span;
        IrExpr {
            kind: IrExprKind::Call {
                name: "to_string".to_string(),
                args: vec![expr],
            },
            ty: TypeRef::new("string"),
            span,
        }
    }

    /// Coerce a `string + char` / `char + string` addition so the char operand
    /// becomes a `to_string(...)` string; any other operands pass through
    /// unchanged. Semantics has already accepted the operand types.
    pub(crate) fn coerce_string_char_add(
        &self,
        op: BinaryOp,
        left: IrExpr,
        right: IrExpr,
    ) -> (IrExpr, IrExpr) {
        if !matches!(op, BinaryOp::Add) {
            return (left, right);
        }
        let string = TypeRef::new("string");
        let char_ty = TypeRef::new("char");
        if left.ty == string && right.ty == char_ty {
            (left, self.to_string_wrap(right))
        } else if left.ty == char_ty && right.ty == string {
            (self.to_string_wrap(left), right)
        } else {
            (left, right)
        }
    }

    /// Lower an expression while capturing exactly the statement-prelude entries
    /// its own lowering produced (e.g. a nested `?` or inline conditional),
    /// leaving any earlier prelude in place. Used to keep a conditional branch's
    /// hoisted work inside the branch that guards it.
    pub(crate) fn lower_captured(
        &self,
        expr: &Expr,
        expected: Option<&TypeRef>,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<(Vec<IrStmt>, IrExpr), IrLoweringError> {
        let saved = self.try_prelude.borrow().len();
        let lowered = self.lower_expr_expected(expr, expected, scope)?;
        let prelude: Vec<IrStmt> = self.try_prelude.borrow_mut().drain(saved..).collect();
        Ok((prelude, lowered))
    }

    /// A type-correct zero value for `ty`, used as the dead initializer of an
    /// inline-conditional temporary. Only scalars and `string` are supported
    /// (semantics enforces this with `L0436`); anything else is a lowering bug.
    pub(crate) fn zero_ir_expr(&self, ty: &TypeRef, span: Span) -> Result<IrExpr, IrLoweringError> {
        let kind = match ty.name.as_str() {
            "i64" | "i8" | "i16" | "i32" | "u8" | "u16" | "u32" | "u64" | "isize" | "usize"
            | "byte" => IrExprKind::Integer(0),
            "bool" => IrExprKind::Bool(false),
            "char" => IrExprKind::Char('\0'),
            "f64" | "f32" => IrExprKind::Float(0.0),
            "string" => IrExprKind::String(String::new()),
            other => {
                return Err(IrLoweringError::new(
                    format!(
                        "inline conditional over `{other}` is not supported; use an `if` statement"
                    ),
                    Some(span),
                ));
            }
        };
        Ok(IrExpr {
            kind,
            ty: ty.clone(),
            span,
        })
    }

    /// Lower a built-in `option`/`result` constructor to a variant `Call` IR
    /// node whose type is resolved from the payload and/or the contextual
    /// expected type. Returns `None` when `expr` is not such a constructor so the
    /// caller falls through to the generic lowering rules. Semantics has already
    /// validated these sites, so the expected type is trusted here.
    pub(crate) fn lower_builtin_construction(
        &self,
        expr: &Expr,
        expected: Option<&TypeRef>,
        scope: &HashMap<String, TypeRef>,
    ) -> Option<Result<IrExpr, IrLoweringError>> {
        // `list_new()` has no payload; its element type comes solely from the
        // expected `list<...>` type, exactly like `none`. Semantics has already
        // validated that an expected `list<...>` type is present here.
        if let ExprKind::Call { name, args } = &expr.kind
            && name == "list_new"
            && args.is_empty()
        {
            let ty = match expected.cloned() {
                Some(ty) if ty.generic_args("list").is_some() => ty,
                _ => {
                    return Some(Err(IrLoweringError::new(
                        "cannot infer the element type of `list_new` without an expected `list<...>` type",
                        Some(expr.span),
                    )));
                }
            };
            return Some(Ok(IrExpr {
                kind: IrExprKind::Call {
                    name: name.clone(),
                    args: Vec::new(),
                },
                ty,
                span: expr.span,
            }));
        }

        // `map_new()` mirrors `list_new()`: its key/value types come solely from
        // the expected `map<...>` type. Semantics has already validated it.
        if let ExprKind::Call { name, args } = &expr.kind
            && name == "map_new"
            && args.is_empty()
        {
            let ty = match expected.cloned() {
                Some(ty) if ty.generic_args("map").is_some() => ty,
                _ => {
                    return Some(Err(IrLoweringError::new(
                        "cannot infer the key/value types of `map_new` without an expected `map<...>` type",
                        Some(expr.span),
                    )));
                }
            };
            return Some(Ok(IrExpr {
                kind: IrExprKind::Call {
                    name: name.clone(),
                    args: Vec::new(),
                },
                ty,
                span: expr.span,
            }));
        }

        let (name, payload_expr) = match &expr.kind {
            // Bare `none` (not shadowed by a local) is unit-variant construction.
            ExprKind::Variable(name) if name == "none" && !scope.contains_key(name) => {
                (name.as_str(), None)
            }
            ExprKind::Call { name, args } if name == "some" || name == "ok" || name == "err" => {
                (name.as_str(), args.first())
            }
            _ => return None,
        };

        // Lower the payload (if any), guided by the expected type so nested
        // `option`/`result` payloads type correctly.
        let payload_expected = match name {
            "some" => expected.and_then(|ty| ty.option_element()),
            "ok" => expected.and_then(|ty| ty.result_args()).map(|(ok, _)| ok),
            "err" => expected.and_then(|ty| ty.result_args()).map(|(_, err)| err),
            _ => None,
        };
        let lowered_payload = match payload_expr {
            Some(payload) => Some(
                match self.lower_expr_expected(payload, payload_expected.as_ref(), scope) {
                    Ok(value) => value,
                    Err(error) => return Some(Err(error)),
                },
            ),
            None => None,
        };

        // Resolve the constructed type. `some(v)` synthesizes `option<typeof v>`
        // when no expected type pins it; `none`/`ok`/`err` require the expected
        // type (guaranteed present by semantics).
        let ty = match name {
            "some" => expected.cloned().unwrap_or_else(|| {
                option_type(
                    lowered_payload
                        .as_ref()
                        .map(|value| &value.ty)
                        .unwrap_or(&TypeRef::new("void")),
                )
            }),
            "none" | "ok" | "err" => match expected.cloned() {
                Some(ty) => ty,
                None => {
                    return Some(Err(IrLoweringError::new(
                        format!("cannot infer the type of `{name}` without an expected type"),
                        Some(expr.span),
                    )));
                }
            },
            // `name` was bound to one of these four constructor names by the
            // `(name, payload_expr)` match at the top of this function.
            _ => unreachable!("constructor name is one of some/none/ok/err"),
        };

        let args = lowered_payload.into_iter().collect();
        Some(Ok(IrExpr {
            kind: IrExprKind::Call {
                name: name.to_string(),
                args,
            },
            ty,
            span: expr.span,
        }))
    }

    /// Lower a call's arguments, propagating context-directed expected types into
    /// argument position so a nested `list_new`/`map_new`/`none`/`ok`/`err` (or a
    /// value flowing into a collection element/key/value slot) re-derives the
    /// same type semantics assigned. Mirrors the argument-position inference in
    /// `lullaby_semantics::Analyzer::check_call`.
    pub(crate) fn lower_call_args(
        &self,
        name: &str,
        args: &[Expr],
        expected: Option<&TypeRef>,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<Vec<IrExpr>, IrLoweringError> {
        // Collection-growing builtins return the container type, so the outer
        // expected container type flows into the container argument and the
        // resolved element/key/value types flow into the value arguments.
        match name {
            "push" if args.len() == 2 => {
                let list = self.lower_expr_expected(&args[0], expected, scope)?;
                let element = list_element_type(&list.ty);
                let value = self.lower_expr_expected(&args[1], element.as_ref(), scope)?;
                return Ok(vec![list, value]);
            }
            "set" if args.len() == 3 => {
                let list = self.lower_expr_expected(&args[0], expected, scope)?;
                let index = self.lower_expr(&args[1], scope)?;
                let element = list_element_type(&list.ty);
                let value = self.lower_expr_expected(&args[2], element.as_ref(), scope)?;
                return Ok(vec![list, index, value]);
            }
            "pop" if args.len() == 1 => {
                return Ok(vec![self.lower_expr_expected(&args[0], expected, scope)?]);
            }
            "map_set" if args.len() == 3 => {
                let map = self.lower_expr_expected(&args[0], expected, scope)?;
                let (key_ty, value_ty) = map_kv_types(&map.ty);
                let key = self.lower_expr_expected(&args[1], key_ty.as_ref(), scope)?;
                let value = self.lower_expr_expected(&args[2], value_ty.as_ref(), scope)?;
                return Ok(vec![map, key, value]);
            }
            "map_del" if args.len() == 2 => {
                let map = self.lower_expr_expected(&args[0], expected, scope)?;
                let key = self.lower_expr(&args[1], scope)?;
                return Ok(vec![map, key]);
            }
            _ => {}
        }
        // A function-typed local: propagate its declared parameter types.
        if let Some((params, _)) = scope.get(name).and_then(TypeRef::function_signature) {
            if params.len() == args.len() {
                return args
                    .iter()
                    .zip(params.iter())
                    .map(|(arg, param)| self.lower_expr_expected(arg, Some(param), scope))
                    .collect();
            }
        } else if let Some(signature) = self.signatures.get(name) {
            // A user function: propagate each concrete (non-type-variable)
            // parameter type. A parameter that mentions a type variable is left
            // uncontextualized, matching `check_generic_call`.
            if signature.params.len() == args.len() {
                let empty = HashMap::new();
                return args
                    .iter()
                    .zip(signature.params.iter())
                    .map(|(arg, param)| {
                        let has_var = lullaby_semantics::first_unresolved_type_var(
                            param,
                            &signature.type_params,
                            &empty,
                        )
                        .is_some();
                        let expected = if has_var { None } else { Some(param) };
                        self.lower_expr_expected(arg, expected, scope)
                    })
                    .collect();
            }
        }
        // Default: no contextual expected type for any argument.
        args.iter().map(|arg| self.lower_expr(arg, scope)).collect()
    }

    pub(crate) fn call_return_type(
        &self,
        name: &str,
        args: &[IrExpr],
        span: Span,
    ) -> Result<TypeRef, IrLoweringError> {
        // A trait-method call: its result type is the trait method's return type
        // with `Self` = the receiver's type. Generics are erased, so a bounded
        // `v.show()` resolves the same way on the concrete or type-variable type.
        if let Some(method_sig) = self.trait_method_sig(name) {
            let receiver = args.first().ok_or_else(|| {
                IrLoweringError::new(
                    format!("trait method `{name}` call missing receiver"),
                    Some(span),
                )
            })?;
            return Ok(substitute_self_type(&method_sig.return_type, &receiver.ty));
        }
        // A call whose name is a known enum variant is enum construction; its
        // type is the owning enum's nominal type.
        if let Some(enum_name) = self.enum_of_variant(name) {
            return Ok(TypeRef::new(enum_name));
        }
        // A call whose name is a declared struct is a struct construction.
        if self.is_struct(name) {
            return Ok(TypeRef::new(name));
        }
        Ok(match name {
            "alloc" => {
                let value = args.first().ok_or_else(|| {
                    IrLoweringError::new("alloc call missing value argument", Some(span))
                })?;
                TypeRef::new(format!("ptr_{}", value.ty.name))
            }
            "load" => {
                let ptr = args.first().ok_or_else(|| {
                    IrLoweringError::new("load call missing pointer argument", Some(span))
                })?;
                ptr.ty
                    .name
                    .strip_prefix("ptr_")
                    .map(TypeRef::new)
                    .ok_or_else(|| {
                        IrLoweringError::new("load call argument is not a pointer", Some(span))
                    })?
            }
            "store" | "dealloc" | "write_file" | "append_file" | "write_bytes" | "make_dir"
            | "remove_file" | "remove_dir" | "print" | "println" | "warn" | "wasm_log"
            | "console_log" | "dom_set_text" | "flush" | "sleep_millis" | "assert"
            | "rc_release" | "ptr_write" | "volatile_store" | "region_create" | "tcp_close"
            | "tcp_shutdown" => TypeRef::new("void"),
            // Raw-memory layout queries fold to `i64` constants; the pointer
            // cast `ptr_to_int` likewise yields the integer handle.
            "size_of" | "align_of" | "offset_of" | "ptr_to_int" => TypeRef::new("i64"),
            // `int_to_ptr(n)` reconstructs a raw pointer. The concrete pointee is
            // fixed by the surrounding `let`/parameter annotation; the call node
            // itself carries the generic `ptr<i64>` handle spelling.
            "int_to_ptr" => TypeRef::new("ptr<i64>"),
            // `volatile_load(p)` reads the pointer's element type, like `load`.
            "volatile_load" => {
                let ptr = args.first().ok_or_else(|| {
                    IrLoweringError::new("volatile_load call missing pointer argument", Some(span))
                })?;
                ptr.ty.pointer_target().ok_or_else(|| {
                    IrLoweringError::new("volatile_load call argument is not a pointer", Some(span))
                })?
            }
            // Network builtins report failures as runtime `result` values.
            "tcp_connect" | "tcp_listen" | "tcp_accept" | "udp_bind" => {
                generic_type("result", &[TypeRef::new("Socket"), TypeRef::new("string")])
            }
            "tcp_read" | "udp_recv" | "http_get" | "http_post" | "from_bytes" | "proc_stdout"
            | "proc_stderr" => {
                generic_type("result", &[TypeRef::new("string"), TypeRef::new("string")])
            }
            // Non-blocking accept yields `result<option<Socket>, string>` where
            // the `ok(none)` arm is a would-block signal.
            "tcp_accept_nb" => generic_type(
                "result",
                &[option_type(&TypeRef::new("Socket")), TypeRef::new("string")],
            ),
            // Non-blocking read/recv yields `result<option<string>, string>`;
            // `ok(none)` is a would-block signal.
            "tcp_read_nb" | "udp_recv_nb" => generic_type(
                "result",
                &[option_type(&TypeRef::new("string")), TypeRef::new("string")],
            ),
            "tcp_write" | "udp_send_to" | "set_nonblocking" | "parse_i64" | "proc_wait"
            | "proc_kill" => generic_type("result", &[TypeRef::new("i64"), TypeRef::new("string")]),
            "parse_f64" => generic_type("result", &[TypeRef::new("f64"), TypeRef::new("string")]),
            // Process spawn returns a `process` handle in the `ok` arm.
            "proc_spawn" => {
                generic_type("result", &[TypeRef::new("process"), TypeRef::new("string")])
            }
            "read_file" | "read_all" | "sys_output" | "to_string" | "substring" | "join"
            | "trim" | "replace" | "upper" | "lower" | "repeat" => TypeRef::new("string"),
            "read_lines" | "list_dir" => {
                generic_type("list", std::slice::from_ref(&TypeRef::new("string")))
            }
            "read_bytes" | "to_bytes" => {
                generic_type("list", std::slice::from_ref(&TypeRef::new("byte")))
            }
            "file_exists" | "is_file" | "is_dir" | "contains" | "starts_with" | "ends_with"
            | "map_has" | "is_digit" | "is_alpha" | "is_alnum" | "is_whitespace" | "is_upper"
            | "is_lower" | "list_contains" => TypeRef::new("bool"),
            "sys_status" | "file_size" | "len" | "find" | "count" | "map_len" | "char_code"
            | "byte_val" | "byte_len" | "mono_now" | "wall_now" | "list_index_of" | "to_i64"
            | "sign" | "gcd" => TypeRef::new("i64"),
            "to_i8" => TypeRef::new("i8"),
            "to_u8" => TypeRef::new("u8"),
            "to_i16" => TypeRef::new("i16"),
            "to_i32" => TypeRef::new("i32"),
            "to_u16" => TypeRef::new("u16"),
            "to_u32" => TypeRef::new("u32"),
            "to_u64" => TypeRef::new("u64"),
            "to_isize" => TypeRef::new("isize"),
            "to_usize" => TypeRef::new("usize"),
            "to_f32" => TypeRef::new("f32"),
            // saturating/wrapping arithmetic returns the operand width `T`.
            "saturating_add" | "saturating_sub" | "saturating_mul" | "wrapping_add"
            | "wrapping_sub" | "wrapping_mul" => args
                .first()
                .map(|operand| operand.ty.clone())
                .ok_or_else(|| {
                    IrLoweringError::new(format!("{name} call missing operand"), Some(span))
                })?,
            // checked arithmetic returns `option<T>`.
            "checked_add" | "checked_sub" | "checked_mul" => {
                let operand = args
                    .first()
                    .map(|operand| operand.ty.clone())
                    .ok_or_else(|| {
                        IrLoweringError::new(format!("{name} call missing operand"), Some(span))
                    })?;
                generic_type("option", std::slice::from_ref(&operand))
            }
            "char_from" => TypeRef::new("char"),
            "byte" => TypeRef::new("byte"),
            // `push`/`set`/`pop`/`reverse`/`concat`/`slice` return a new `list<T>`
            // of the same type as their (first) list argument (spelled `list<T>`).
            "push" | "set" | "pop" | "reverse" | "sort" | "sort_by" | "concat" | "slice" => {
                args.first().map(|list| list.ty.clone()).ok_or_else(|| {
                    IrLoweringError::new(format!("{name} call missing list argument"), Some(span))
                })?
            }
            // `array_fill(n, value)` yields `array<T>` where `T` is the value's type.
            "array_fill" => {
                let value = args.get(1).ok_or_else(|| {
                    IrLoweringError::new("array_fill call missing value argument", Some(span))
                })?;
                TypeRef::new(format!("array<{}>", value.ty.name))
            }
            // `list_map(l list<T>, f fn(T) -> U)` yields `list<U>`, where `U` is
            // the mapping function's return type.
            "list_map" => {
                let func = args.get(1).ok_or_else(|| {
                    IrLoweringError::new("list_map call missing function argument", Some(span))
                })?;
                let (_, ret) = func.ty.function_signature().ok_or_else(|| {
                    IrLoweringError::new(
                        "list_map function argument is not a function type",
                        Some(span),
                    )
                })?;
                generic_type("list", std::slice::from_ref(&ret))
            }
            // `list_filter(l list<T>, pred fn(T) -> bool)` yields `list<T>`, the
            // same type as its list argument.
            "list_filter" => args.first().map(|list| list.ty.clone()).ok_or_else(|| {
                IrLoweringError::new("list_filter call missing list argument", Some(span))
            })?,
            // `list_reduce(l list<T>, init U, f fn(U, T) -> U)` yields `U`, the
            // accumulator type carried by the `init` argument.
            "list_reduce" => args.get(1).map(|init| init.ty.clone()).ok_or_else(|| {
                IrLoweringError::new("list_reduce call missing init argument", Some(span))
            })?,
            // `get(l, i)` returns the element type `T` of its `list<T>` argument.
            "get" => {
                let list = args.first().ok_or_else(|| {
                    IrLoweringError::new("get call missing list argument", Some(span))
                })?;
                list.ty
                    .generic_args("list")
                    .filter(|args| args.len() == 1)
                    .map(|mut args| args.remove(0))
                    .ok_or_else(|| {
                        IrLoweringError::new("get call argument is not a list", Some(span))
                    })?
            }
            // `map_set`/`map_del` return a new `map<K, V>` of the same type as
            // their map argument (already spelled `map<K, V>`).
            "map_set" | "map_del" => args.first().map(|map| map.ty.clone()).ok_or_else(|| {
                IrLoweringError::new(format!("{name} call missing map argument"), Some(span))
            })?,
            // `map_get(m, k)` returns `option<V>` where `V` is the value type of
            // its `map<K, V>` argument.
            "map_get" => {
                let map = args.first().ok_or_else(|| {
                    IrLoweringError::new("map_get call missing map argument", Some(span))
                })?;
                let value = map
                    .ty
                    .generic_args("map")
                    .filter(|args| args.len() == 2)
                    .map(|mut args| args.remove(1))
                    .ok_or_else(|| {
                        IrLoweringError::new("map_get call argument is not a map", Some(span))
                    })?;
                generic_type("option", std::slice::from_ref(&value))
            }
            // `map_keys(m) -> list<K>` and `map_values(m) -> list<V>`.
            "map_keys" | "map_values" => {
                let map = args.first().ok_or_else(|| {
                    IrLoweringError::new(format!("{name} call missing map argument"), Some(span))
                })?;
                let mut kv = map
                    .ty
                    .generic_args("map")
                    .filter(|args| args.len() == 2)
                    .ok_or_else(|| {
                        IrLoweringError::new(
                            format!("{name} call argument is not a map"),
                            Some(span),
                        )
                    })?;
                let element = if name == "map_keys" {
                    kv.remove(0)
                } else {
                    kv.remove(1)
                };
                generic_type("list", std::slice::from_ref(&element))
            }
            "split" | "words" => TypeRef::new("array<string>"),
            "chars" => generic_type("list", std::slice::from_ref(&TypeRef::new("char"))),
            "string_from_chars" => TypeRef::new("string"),
            // `env(name)` yields `option<string>`; `args()` yields `list<string>`.
            "env" => generic_type("option", std::slice::from_ref(&TypeRef::new("string"))),
            "args" => generic_type("list", std::slice::from_ref(&TypeRef::new("string"))),
            // `os_random(len)` yields `result<list<byte>, string>`.
            "os_random" => generic_type(
                "result",
                &[
                    generic_type("list", std::slice::from_ref(&TypeRef::new("byte"))),
                    TypeRef::new("string"),
                ],
            ),
            // `parallel_map(f, list<i64>)` maps `fn(i64) -> i64` over the list,
            // yielding a `list<i64>` in input order.
            "parallel_map" => generic_type("list", std::slice::from_ref(&TypeRef::new("i64"))),
            // Concurrency builtins: opaque handle producers and readers.
            "chan_new" => TypeRef::new("Chan"),
            "spawn" => TypeRef::new("Task"),
            "mutex_new" => TypeRef::new("Mutex"),
            "recv" | "mutex_get" | "mutex_add" => TypeRef::new("i64"),
            "try_recv" => generic_type("option", std::slice::from_ref(&TypeRef::new("i64"))),
            // `read_line() -> option<string>`: `none` at end-of-input.
            "read_line" => generic_type("option", std::slice::from_ref(&TypeRef::new("string"))),
            "send" | "task_join" | "mutex_set" => TypeRef::new("void"),
            // Atomic (`atomic_i64`) builtins: the constructor yields the handle,
            // `atomic_store` is `void`, and every access/RMW yields `i64`.
            "atomic_new" => TypeRef::new("atomic_i64"),
            "atomic_store" | "atomic_store_ordered" | "fence" => TypeRef::new("void"),
            "atomic_load"
            | "atomic_swap"
            | "atomic_cas"
            | "atomic_add"
            | "atomic_sub"
            | "atomic_and"
            | "atomic_or"
            | "atomic_xor"
            | "atomic_load_ordered"
            | "atomic_swap_ordered"
            | "atomic_cas_ordered"
            | "atomic_add_ordered"
            | "atomic_sub_ordered"
            | "atomic_and_ordered"
            | "atomic_or_ordered"
            | "atomic_xor_ordered" => TypeRef::new("i64"),
            "sqrt" | "floor" | "ceil" | "round" | "sin" | "cos" | "tan" | "atan" | "exp" | "ln"
            | "log10" | "atan2" | "to_f64" => TypeRef::new("f64"),
            // Bit intrinsics on i64: rotations, popcount, leading/trailing zero
            // counts, and byte swap all return i64.
            "rotate_left" | "rotate_right" | "count_ones" | "leading_zeros" | "trailing_zeros"
            | "reverse_bytes" => TypeRef::new("i64"),
            "abs" | "min" | "max" | "pow" | "clamp" => {
                let value = args.first().ok_or_else(|| {
                    IrLoweringError::new(format!("{name} call missing argument"), Some(span))
                })?;
                TypeRef::new(value.ty.name.clone())
            }
            // `list_sum(l)` returns the numeric element type `T` of its `list<T>`.
            "list_sum" => {
                let list = args.first().ok_or_else(|| {
                    IrLoweringError::new("list_sum call missing list argument", Some(span))
                })?;
                list.ty
                    .generic_args("list")
                    .filter(|args| args.len() == 1)
                    .map(|mut args| args.remove(0))
                    .ok_or_else(|| {
                        IrLoweringError::new("list_sum call argument is not a list", Some(span))
                    })?
            }
            // `list_min(l)` / `list_max(l)` return `option<T>` over a `list<T>`.
            "list_min" | "list_max" => {
                let list = args.first().ok_or_else(|| {
                    IrLoweringError::new(format!("{name} call missing list argument"), Some(span))
                })?;
                let element = list
                    .ty
                    .generic_args("list")
                    .filter(|args| args.len() == 1)
                    .map(|mut args| args.remove(0))
                    .ok_or_else(|| {
                        IrLoweringError::new(
                            format!("{name} call argument is not a list"),
                            Some(span),
                        )
                    })?;
                generic_type("option", std::slice::from_ref(&element))
            }
            "rc_new" => {
                let value = args.first().ok_or_else(|| {
                    IrLoweringError::new("rc_new call missing value argument", Some(span))
                })?;
                TypeRef::new(format!("rc<{}>", value.ty.name))
            }
            "rc_clone" => args
                .first()
                .map(|handle| handle.ty.clone())
                .ok_or_else(|| {
                    IrLoweringError::new("rc_clone call missing handle argument", Some(span))
                })?,
            "rc_get" => reference_inner(args, "rc", span)?,
            "ref_get" => reference_inner(args, "ref", span)?,
            "rc_borrow" => {
                TypeRef::new(format!("ref<{}>", reference_inner(args, "rc", span)?.name))
            }
            "ptr_read" => {
                let ptr = args.first().ok_or_else(|| {
                    IrLoweringError::new("ptr_read call missing pointer argument", Some(span))
                })?;
                ptr.ty.pointer_target().ok_or_else(|| {
                    IrLoweringError::new("ptr_read call argument is not a pointer", Some(span))
                })?
            }
            _ => {
                let signature = self.signatures.get(name).ok_or_else(|| {
                    IrLoweringError::new(format!("unknown function `{name}`"), Some(span))
                })?;
                if signature.type_params.is_empty() {
                    // Calling an `async fn` yields a `Future<return_type>`, matching
                    // the semantic type; `await` later resolves the inner `T`.
                    if signature.is_async {
                        generic_type("Future", std::slice::from_ref(&signature.return_type))
                    } else {
                        signature.return_type.clone()
                    }
                } else {
                    // Generic function: re-run the same call-site inference as
                    // semantics against the lowered argument types so the IR
                    // result type matches. Generics are erased, so this only
                    // determines the static result type; the emitted call is an
                    // ordinary call by name.
                    let arg_types: Vec<TypeRef> = args.iter().map(|arg| arg.ty.clone()).collect();
                    lullaby_semantics::infer_generic_return(signature, &arg_types).map_err(
                        |error| {
                            IrLoweringError::new(
                                format!("generic call `{name}` inference failed: {error:?}"),
                                Some(span),
                            )
                        },
                    )?
                }
            }
        })
    }
}

/// Canonical `option<T>` type spelling.
pub(crate) fn option_type(payload: &TypeRef) -> TypeRef {
    generic_type("option", std::slice::from_ref(payload))
}

/// The element type `T` of a `list<T>` spelling, if any.
pub(crate) fn list_element_type(ty: &TypeRef) -> Option<TypeRef> {
    ty.generic_args("list")
        .filter(|args| args.len() == 1)
        .map(|mut args| args.remove(0))
}

/// The `(K, V)` type pair of a `map<K, V>` spelling, split into optional parts.
pub(crate) fn map_kv_types(ty: &TypeRef) -> (Option<TypeRef>, Option<TypeRef>) {
    match ty.generic_args("map").filter(|args| args.len() == 2) {
        Some(mut args) => {
            let value = args.remove(1);
            let key = args.remove(0);
            (Some(key), Some(value))
        }
        None => (None, None),
    }
}

/// Extract the inner type `T` of the first argument's `<ctor><T>` reference type.
pub(crate) fn reference_inner(
    args: &[IrExpr],
    ctor: &str,
    span: Span,
) -> Result<TypeRef, IrLoweringError> {
    args.first()
        .and_then(|arg| arg.ty.generic_arg(ctor))
        .ok_or_else(|| {
            IrLoweringError::new(
                format!("{ctor} reference call argument is invalid"),
                Some(span),
            )
        })
}

/// Replace `Self` in a type with the receiver's concrete type, recursing into
/// compound generic types. Used to compute a trait method's IR result type.
pub(crate) fn substitute_self_type(ty: &TypeRef, self_ty: &TypeRef) -> TypeRef {
    if ty.name == "Self" {
        return self_ty.clone();
    }
    for ctor in [
        "array", "list", "option", "result", "map", "ptr", "ref", "rc",
    ] {
        if let Some(args) = ty.generic_args(ctor) {
            let mapped: Vec<TypeRef> = args
                .iter()
                .map(|arg| substitute_self_type(arg, self_ty))
                .collect();
            return generic_type(ctor, &mapped);
        }
    }
    ty.clone()
}
