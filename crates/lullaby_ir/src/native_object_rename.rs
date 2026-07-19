//! Scope-correct alpha-renaming of shadowing local bindings, run once per function
//! at the head of [`lower_native_function`] before any frame/slot planning.
//!
//! # Why this exists
//!
//! The native frame planner ([`collect_native_locals`])
//! keys every local by its bare source name in one flat `HashMap`, and skips a
//! `let` whose name is already present. That is correct only while names are
//! unique. It is **wrong** the moment an inner scope re-`let`s a name that an
//! enclosing scope still has live: the two distinct bindings collapse onto ONE
//! stack slot, so the inner write clobbers the outer value. The three interpreters
//! model each block as its own scope (the bytecode VM's `Env` gives every binding a
//! fresh unique slot, resolved innermost/newest-first), so they keep the two apart
//! and native diverged from all three — a wrong value with the arena off, and a
//! use-after-free with the safe-tier arena on (the outer read lands on the inner
//! slot, which the loop's per-iteration rewind has reclaimed).
//!
//! # What it does
//!
//! It walks the function's instruction tree with the SAME lexical-scope structure
//! the bytecode VM compiler uses (`compile_scoped_block` pushes a scope for each
//! `if`/`elif`/`else` body, `while`/`loop` body, `for` counter *and* its body,
//! `match` arm, and `try` body/catch), and gives any binding that **shadows a name
//! from a strictly-enclosing scope** a fresh, source-illegal name (`v#sN` — `#`
//! cannot appear in a Lullaby identifier, so it can never collide with a user name
//! or with the `__end`/`__step` for-loop temporaries). Every read/write of that
//! binding inside its scope is rewritten to the fresh name, so the existing
//! flat-map planner then assigns it its own slot with no further changes.
//!
//! A binding that does NOT shadow keeps its original name, so a function without
//! cross-scope shadowing is rewritten to a structurally-identical tree and its
//! generated code is byte-for-byte unchanged. Same-scope re-`let` (`let x = x + 1`
//! rebinding a parameter, two `let v`s in one block) also keeps the name: the older
//! binding is dead for the rest of the scope, so sharing its slot is sound and
//! matches the pre-existing behavior — only *cross-scope* shadowing was broken.
//!
//! # Closures
//!
//! A closure literal's captured names are read back by
//! [`lower_closure_literal`] from the
//! separately-synthesized closure body, which still spells each capture by its
//! original source name. If a captured name is shadowed at the capture site, this
//! pass cannot rename it soundly (the body would still name the original), so it
//! REFUSES — the enclosing function skips cleanly to the interpreters (`L0339`)
//! rather than capture the wrong (outer) slot. This case already miscompiled, so a
//! clean skip is strictly safer.

use super::*;
use std::collections::HashSet;

/// One lexical scope's worth of renaming state.
#[derive(Default)]
struct ScopeFrame {
    /// `(name, previous active mapping)` for every name this scope (re)bound, so
    /// leaving the scope restores the enclosing binding exactly.
    saved: Vec<(String, Option<String>)>,
    /// Names declared **in this scope**, to distinguish a same-scope re-`let`
    /// (slot reuse, unchanged) from a shadow of an enclosing scope (fresh slot).
    introduced: HashSet<String>,
}

/// Alpha-renaming walker: an active `name -> emitted name` map plus a stack of
/// scope frames that restore it on scope exit, mirroring the bytecode VM compiler's
/// `Env`.
struct Renamer<'a> {
    active: HashMap<String, String>,
    scopes: Vec<ScopeFrame>,
    /// Monotonic across the whole function, so every fresh name is unique.
    counter: usize,
    closure_layouts: &'a HashMap<usize, ClosureLayout>,
}

impl<'a> Renamer<'a> {
    fn new(closure_layouts: &'a HashMap<usize, ClosureLayout>) -> Self {
        Self {
            active: HashMap::new(),
            // The function scope: parameters and top-level `let`s share it, exactly
            // like the interpreters (a top-level `let x` re-using a parameter name is
            // a same-scope rebind, not a shadow).
            scopes: vec![ScopeFrame::default()],
            counter: 0,
            closure_layouts,
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(ScopeFrame::default());
    }

    fn pop_scope(&mut self) {
        let frame = self.scopes.pop().expect("a scope is always open");
        // Restore in reverse insertion order so repeated rebinds of one name in the
        // same scope unwind to the correct enclosing binding.
        for (name, prev) in frame.saved.into_iter().rev() {
            match prev {
                Some(emitted) => {
                    self.active.insert(name, emitted);
                }
                None => {
                    self.active.remove(&name);
                }
            }
        }
    }

    /// Seat a parameter as an identity binding in the function scope.
    fn bind_param(&mut self, name: &str) {
        let prev = self.active.insert(name.to_string(), name.to_string());
        let frame = self.scopes.last_mut().expect("function scope open");
        frame.saved.push((name.to_string(), prev));
        frame.introduced.insert(name.to_string());
    }

    /// Introduce a `let`/`for`-counter/`match`-binding/`catch` binding, returning the
    /// name the native planner should slot it under. A binding that shadows a name
    /// from a strictly-enclosing scope gets a fresh unique name; a same-scope rebind
    /// or a first binding keeps its source name.
    fn declare(&mut self, name: &str) -> String {
        // Same-scope rebind: keep the current emitted name (slot reuse). The older
        // binding is dead for the rest of the scope, so this is sound and preserves
        // the pre-existing (correct) behavior for this case.
        if self
            .scopes
            .last()
            .expect("a scope is open")
            .introduced
            .contains(name)
        {
            return self
                .active
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.to_string());
        }
        let shadows_enclosing = self.active.contains_key(name);
        let emitted = if shadows_enclosing {
            self.counter += 1;
            format!("{name}#s{}", self.counter)
        } else {
            name.to_string()
        };
        let prev = self.active.insert(name.to_string(), emitted.clone());
        let frame = self.scopes.last_mut().expect("a scope is open");
        frame.saved.push((name.to_string(), prev));
        frame.introduced.insert(name.to_string());
        emitted
    }

    /// Resolve a name USE to the innermost active binding's emitted name; an unbound
    /// name (a global function, an enum variant used as a value) is returned
    /// unchanged.
    fn resolve(&self, name: &str) -> String {
        self.active
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    fn rewrite_stmts(
        &mut self,
        body: &[BytecodeInstruction],
    ) -> Result<Vec<BytecodeInstruction>, String> {
        body.iter().map(|stmt| self.rewrite_stmt(stmt)).collect()
    }

    /// Rewrite a block that introduces its own lexical scope (an `if`/`else` body, a
    /// loop body, a `try` body): push a frame, rewrite, pop.
    fn rewrite_scoped(
        &mut self,
        body: &[BytecodeInstruction],
    ) -> Result<Vec<BytecodeInstruction>, String> {
        self.push_scope();
        let out = self.rewrite_stmts(body);
        self.pop_scope();
        out
    }

    fn rewrite_stmt(&mut self, stmt: &BytecodeInstruction) -> Result<BytecodeInstruction, String> {
        Ok(match stmt {
            BytecodeInstruction::Let {
                name,
                ty,
                value,
                span,
            } => {
                // The initializer is evaluated in the scope BEFORE this binding takes
                // effect (so `let v = v + 1` reads the OUTER `v`): rewrite it first,
                // then declare the new name.
                let value = self.rewrite_expr(value)?;
                let name = self.declare(name);
                BytecodeInstruction::Let {
                    name,
                    ty: ty.clone(),
                    value,
                    span: *span,
                }
            }
            BytecodeInstruction::Assign {
                name,
                path,
                op,
                value,
                span,
            } => {
                let value = self.rewrite_expr(value)?;
                let path = path
                    .iter()
                    .map(|place| self.rewrite_place(place))
                    .collect::<Result<Vec<_>, _>>()?;
                BytecodeInstruction::Assign {
                    name: self.resolve(name),
                    path,
                    op: *op,
                    value,
                    span: *span,
                }
            }
            BytecodeInstruction::Return(Some(expr)) => {
                BytecodeInstruction::Return(Some(self.rewrite_expr(expr)?))
            }
            BytecodeInstruction::Return(None) => BytecodeInstruction::Return(None),
            BytecodeInstruction::Break(span) => BytecodeInstruction::Break(*span),
            BytecodeInstruction::Continue(span) => BytecodeInstruction::Continue(*span),
            BytecodeInstruction::Expr(expr) => BytecodeInstruction::Expr(self.rewrite_expr(expr)?),
            BytecodeInstruction::If {
                branches,
                else_body,
                span,
            } => {
                let branches = branches
                    .iter()
                    .map(|branch| {
                        // A branch condition is evaluated in the enclosing scope,
                        // before the branch body's scope is entered.
                        let condition = self.rewrite_expr(&branch.condition)?;
                        let body = self.rewrite_scoped(&branch.body)?;
                        Ok(BytecodeIfBranch { condition, body })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                let else_body = self.rewrite_scoped(else_body)?;
                BytecodeInstruction::If {
                    branches,
                    else_body,
                    span: *span,
                }
            }
            BytecodeInstruction::While {
                condition,
                body,
                span,
            } => {
                let condition = self.rewrite_expr(condition)?;
                let body = self.rewrite_scoped(body)?;
                BytecodeInstruction::While {
                    condition,
                    body,
                    span: *span,
                }
            }
            BytecodeInstruction::Loop { body, span } => BytecodeInstruction::Loop {
                body: self.rewrite_scoped(body)?,
                span: *span,
            },
            // A region block introduces its own lexical scope, exactly like a loop or
            // `if` body: push a frame so a block-local binding that shadows an
            // enclosing name is renamed to a fresh, source-illegal name and gets its
            // own slot. Without this the flat-map planner would collapse the shadow
            // onto the outer slot (a wrong value now, a use-after-free once the
            // region's sub-region is reclaimed).
            BytecodeInstruction::RegionBlock { body, span } => BytecodeInstruction::RegionBlock {
                body: self.rewrite_scoped(body)?,
                span: *span,
            },
            BytecodeInstruction::For {
                name,
                start,
                end,
                step,
                body,
                span,
            } => {
                // Bounds are evaluated before the counter binds, in the enclosing
                // scope. The counter then lives in its own scope, with the body a
                // further-nested scope inside it — matching the VM, so a body `let`
                // that shadows the counter (or the counter shadowing an outer name)
                // is renamed apart.
                let start = self.rewrite_expr(start)?;
                let end = self.rewrite_expr(end)?;
                let step = step.as_ref().map(|s| self.rewrite_expr(s)).transpose()?;
                self.push_scope();
                let name = self.declare(name);
                let body = self.rewrite_scoped(body)?;
                self.pop_scope();
                BytecodeInstruction::For {
                    name,
                    start,
                    end,
                    step,
                    body,
                    span: *span,
                }
            }
            BytecodeInstruction::Asm { bytes, span } => BytecodeInstruction::Asm {
                bytes: bytes.clone(),
                span: *span,
            },
            BytecodeInstruction::Throw { value, span } => BytecodeInstruction::Throw {
                value: self.rewrite_expr(value)?,
                span: *span,
            },
            BytecodeInstruction::Try {
                body,
                catch_name,
                catch_body,
                span,
            } => {
                // The try body is its own scope; the catch name binds in the catch
                // scope alongside the catch body.
                let body = self.rewrite_scoped(body)?;
                self.push_scope();
                let catch_name = self.declare(catch_name);
                let catch_body = self.rewrite_stmts(catch_body)?;
                self.pop_scope();
                BytecodeInstruction::Try {
                    body,
                    catch_name,
                    catch_body,
                    span: *span,
                }
            }
            BytecodeInstruction::Match {
                scrutinee,
                arms,
                span,
            } => {
                let scrutinee = self.rewrite_expr(scrutinee)?;
                let arms = arms
                    .iter()
                    .map(|arm| {
                        self.push_scope();
                        let pattern = match &arm.pattern {
                            BytecodeMatchPattern::Variant { name, bindings } => {
                                let bindings = bindings
                                    .iter()
                                    .map(|binding| self.declare(binding))
                                    .collect();
                                BytecodeMatchPattern::Variant {
                                    name: name.clone(),
                                    bindings,
                                }
                            }
                            BytecodeMatchPattern::Wildcard => BytecodeMatchPattern::Wildcard,
                        };
                        let body = self.rewrite_stmts(&arm.body);
                        self.pop_scope();
                        Ok(BytecodeMatchArm {
                            pattern,
                            body: body?,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                BytecodeInstruction::Match {
                    scrutinee,
                    arms,
                    span: *span,
                }
            }
        })
    }

    fn rewrite_place(&self, place: &BytecodePlace) -> Result<BytecodePlace, String> {
        Ok(match place {
            BytecodePlace::Field(field) => BytecodePlace::Field(field.clone()),
            BytecodePlace::Index(expr) => BytecodePlace::Index(self.rewrite_expr(expr)?),
        })
    }

    fn rewrite_expr(&self, expr: &BytecodeExpr) -> Result<BytecodeExpr, String> {
        let kind = match &expr.kind {
            BytecodeExprKind::Integer(v) => BytecodeExprKind::Integer(*v),
            BytecodeExprKind::Float(v) => BytecodeExprKind::Float(*v),
            BytecodeExprKind::Bool(v) => BytecodeExprKind::Bool(*v),
            BytecodeExprKind::String(v) => BytecodeExprKind::String(v.clone()),
            BytecodeExprKind::Char(v) => BytecodeExprKind::Char(*v),
            BytecodeExprKind::Array(elements) => BytecodeExprKind::Array(
                elements
                    .iter()
                    .map(|e| self.rewrite_expr(e))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            BytecodeExprKind::Variable(name) => BytecodeExprKind::Variable(self.resolve(name)),
            BytecodeExprKind::Index { target, index } => BytecodeExprKind::Index {
                target: Box::new(self.rewrite_expr(target)?),
                index: Box::new(self.rewrite_expr(index)?),
            },
            BytecodeExprKind::Unary { op, expr } => BytecodeExprKind::Unary {
                op: *op,
                expr: Box::new(self.rewrite_expr(expr)?),
            },
            BytecodeExprKind::Binary { left, op, right } => BytecodeExprKind::Binary {
                left: Box::new(self.rewrite_expr(left)?),
                op: *op,
                right: Box::new(self.rewrite_expr(right)?),
            },
            BytecodeExprKind::Call { name, args } => {
                // A call THROUGH a local (a first-class function / closure value)
                // resolves like any other use, so a shadowed closure local calls the
                // right binding. A global function name is never active, so it is
                // returned unchanged.
                let name = if self.active.contains_key(name) {
                    self.resolve(name)
                } else {
                    name.clone()
                };
                let args = args
                    .iter()
                    .map(|a| self.rewrite_expr(a))
                    .collect::<Result<Vec<_>, _>>()?;
                BytecodeExprKind::Call { name, args }
            }
            BytecodeExprKind::Field { target, field } => BytecodeExprKind::Field {
                target: Box::new(self.rewrite_expr(target)?),
                field: field.clone(),
            },
            BytecodeExprKind::Await { expr } => BytecodeExprKind::Await {
                expr: Box::new(self.rewrite_expr(expr)?),
            },
            BytecodeExprKind::Closure { id } => {
                // The closure body (synthesized separately) reads its captures by
                // their original source spelling. If any capture is shadowed here, we
                // cannot rename it soundly, so refuse and let the function skip.
                if let Some(layout) = self.closure_layouts.get(id) {
                    for (capture, _) in &layout.captures {
                        if self.resolve(capture) != *capture {
                            return Err(format!(
                                "closure #{id} captures `{capture}`, which is shadowed at the \
                                 capture site; deferring to keep the capture sound"
                            ));
                        }
                    }
                }
                BytecodeExprKind::Closure { id: *id }
            }
        };
        Ok(BytecodeExpr {
            kind,
            ty: expr.ty.clone(),
            span: expr.span,
        })
    }
}

/// Produce a copy of `function` in which every binding that shadows a name from a
/// strictly-enclosing scope carries a fresh, source-illegal name (`v#sN`), with all
/// its in-scope reads/writes rewritten to match. Non-shadowing functions are
/// returned structurally identical (byte-identical codegen). Returns `Err` — a
/// clean native skip — when a closure captures a name that is shadowed at its
/// literal (see the module docs).
pub(crate) fn alpha_rename_shadowing_bindings(
    function: &BytecodeFunction,
    closure_layouts: &HashMap<usize, ClosureLayout>,
) -> Result<BytecodeFunction, String> {
    let mut renamer = Renamer::new(closure_layouts);
    for param in &function.params {
        renamer.bind_param(&param.name);
    }
    let instructions = renamer.rewrite_stmts(&function.instructions)?;
    Ok(BytecodeFunction {
        name: function.name.clone(),
        params: function.params.clone(),
        return_type: function.return_type.clone(),
        instructions,
        span: function.span,
    })
}
