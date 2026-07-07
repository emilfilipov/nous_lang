//! Deterministic stack-frame layout analysis for the Alpha 1 subset.
//!
//! This module assigns every parameter, local binding, and loop variable of an
//! [`IrFunction`] a stable stack slot, computes the total frame size, and
//! produces a per-scope cleanup plan. The layout is a pure function of the IR:
//! the same function always produces the same slots, offsets, and cleanup
//! ordering, so it can back both native code generation and debugging metadata.
//!
//! Scope model (matching the runtime's `push_scope`/`pop_scope` behavior):
//! parameters and top-level bindings live in the function scope (depth 0); each
//! `if`/`while`/`loop`/`for` body opens a child scope at depth + 1; a `for`
//! loop variable lives in its body scope. Cleanup at scope exit runs in reverse
//! declaration order.

use lullaby_parser::TypeRef;
use serde::{Deserialize, Serialize};

use crate::native_contract::alpha1_value_layout;
use crate::{IrFunction, IrStmt};

/// Stack alignment for the whole frame, in bytes.
const FRAME_ALIGN: usize = 16;
/// Per-slot alignment, in bytes. Every slot occupies at least one 8-byte word.
const SLOT_ALIGN: usize = 8;

/// The role a slot plays in the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotClass {
    Parameter,
    Local,
    LoopVariable,
}

/// A single stack slot with a deterministic offset from the frame base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSlot {
    pub name: String,
    pub ty: TypeRef,
    pub class: SlotClass,
    /// Byte offset from the frame base. Slots never overlap.
    pub offset: usize,
    /// Slot size in bytes (word-aligned).
    pub size: usize,
    /// Lexical scope depth where the binding is introduced (0 = function scope).
    pub scope_depth: usize,
}

/// The cleanup plan for one lexical scope, in the order cleanup must run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeCleanup {
    pub scope_depth: usize,
    /// Bindings introduced in this scope, in reverse declaration order.
    pub cleanup_order: Vec<String>,
}

/// The full frame layout for a function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameLayout {
    pub function: String,
    pub slots: Vec<FrameSlot>,
    /// Total frame size in bytes, aligned to the stack alignment.
    pub frame_size: usize,
    /// Cleanup plans, ordered by scope-exit order (inner scopes before the
    /// function scope). Scopes with no bindings are omitted.
    pub scopes: Vec<ScopeCleanup>,
}

impl FrameLayout {
    /// Look up a slot by binding name.
    pub fn slot(&self, name: &str) -> Option<&FrameSlot> {
        self.slots.iter().find(|slot| slot.name == name)
    }
}

/// Compute the frame layout for a single function.
pub fn analyze_frame_layout(function: &IrFunction) -> FrameLayout {
    let mut builder = LayoutBuilder::new(function.name.clone());

    let mut function_scope: Vec<String> = Vec::new();
    for param in &function.params {
        builder.add_slot(&param.name, &param.ty, SlotClass::Parameter, 0);
        function_scope.push(param.name.clone());
    }
    builder.walk_block(&function.body, 0, &mut function_scope);
    builder.finish_scope(0, function_scope);

    builder.finish()
}

/// Compute frame layouts for every function in a module, in module order.
pub fn analyze_module_frames(module: &crate::IrModule) -> Vec<FrameLayout> {
    module.functions.iter().map(analyze_frame_layout).collect()
}

struct LayoutBuilder {
    function: String,
    slots: Vec<FrameSlot>,
    scopes: Vec<ScopeCleanup>,
    next_offset: usize,
}

impl LayoutBuilder {
    fn new(function: String) -> Self {
        Self {
            function,
            slots: Vec::new(),
            scopes: Vec::new(),
            next_offset: 0,
        }
    }

    fn add_slot(&mut self, name: &str, ty: &TypeRef, class: SlotClass, scope_depth: usize) {
        let size = slot_size(ty);
        let offset = align_up(self.next_offset, SLOT_ALIGN);
        self.next_offset = offset + size;
        self.slots.push(FrameSlot {
            name: name.to_string(),
            ty: ty.clone(),
            class,
            offset,
            size,
            scope_depth,
        });
    }

    /// Walk a block, appending any bindings it declares to `scope_names` (the
    /// declaration accumulator for the block's own scope depth).
    fn walk_block(&mut self, body: &[IrStmt], depth: usize, scope_names: &mut Vec<String>) {
        for stmt in body {
            match stmt {
                IrStmt::Let { name, ty, .. } => {
                    self.add_slot(name, ty, SlotClass::Local, depth);
                    scope_names.push(name.clone());
                }
                IrStmt::If {
                    branches,
                    else_body,
                    ..
                } => {
                    for branch in branches {
                        self.walk_child_scope(&branch.body, depth + 1, None);
                    }
                    self.walk_child_scope(else_body, depth + 1, None);
                }
                IrStmt::While { body, .. } | IrStmt::Loop { body, .. } => {
                    self.walk_child_scope(body, depth + 1, None);
                }
                IrStmt::For { name, body, .. } => {
                    // The loop variable lives in the loop body scope.
                    self.walk_child_scope(body, depth + 1, Some((name, TypeRef::new("i64"))));
                }
                IrStmt::Try {
                    body,
                    catch_name,
                    catch_body,
                    ..
                } => {
                    // The try body is its own scope; the caught message binds a
                    // string local in the catch body scope.
                    self.walk_child_scope(body, depth + 1, None);
                    self.walk_child_scope(
                        catch_body,
                        depth + 1,
                        Some((catch_name, TypeRef::new("string"))),
                    );
                }
                IrStmt::Match { arms, .. } => {
                    // Each arm body is its own scope; a variant arm's payload
                    // bindings are locals declared in that arm's scope. Payload
                    // types are not carried on the pattern, so bindings take a
                    // default word-sized slot.
                    for arm in arms {
                        let seeds = match &arm.pattern {
                            crate::IrMatchPattern::Variant { bindings, .. } => bindings.clone(),
                            crate::IrMatchPattern::Wildcard => Vec::new(),
                        };
                        self.walk_child_scope_seeded(&arm.body, depth + 1, &seeds);
                    }
                }
                IrStmt::Assign { .. }
                | IrStmt::Return(_)
                | IrStmt::Break(_)
                | IrStmt::Continue(_)
                | IrStmt::Throw { .. }
                | IrStmt::Expr(_) => {}
            }
        }
    }

    /// Walk a nested block as its own scope, optionally seeding a loop variable
    /// declared before the body, then record the scope's cleanup plan.
    fn walk_child_scope(
        &mut self,
        body: &[IrStmt],
        depth: usize,
        loop_var: Option<(&str, TypeRef)>,
    ) {
        let mut names: Vec<String> = Vec::new();
        if let Some((name, ty)) = loop_var {
            self.add_slot(name, &ty, SlotClass::LoopVariable, depth);
            names.push(name.to_string());
        }
        self.walk_block(body, depth, &mut names);
        self.finish_scope(depth, names);
    }

    /// Walk a nested block as its own scope, seeding zero or more bindings (a
    /// match arm's payload bindings) declared before the body.
    fn walk_child_scope_seeded(&mut self, body: &[IrStmt], depth: usize, seeds: &[String]) {
        let mut names: Vec<String> = Vec::new();
        for seed in seeds {
            self.add_slot(seed, &TypeRef::new("enum_payload"), SlotClass::Local, depth);
            names.push(seed.clone());
        }
        self.walk_block(body, depth, &mut names);
        self.finish_scope(depth, names);
    }

    fn finish_scope(&mut self, scope_depth: usize, mut names: Vec<String>) {
        if names.is_empty() {
            return;
        }
        names.reverse();
        self.scopes.push(ScopeCleanup {
            scope_depth,
            cleanup_order: names,
        });
    }

    fn finish(self) -> FrameLayout {
        FrameLayout {
            function: self.function,
            slots: self.slots,
            frame_size: align_up(self.next_offset, FRAME_ALIGN),
            scopes: self.scopes,
        }
    }
}

fn slot_size(ty: &TypeRef) -> usize {
    let raw = alpha1_value_layout(ty)
        .map(|layout| usize::from(layout.size_bytes))
        .unwrap_or(SLOT_ALIGN);
    align_up(raw.max(1), SLOT_ALIGN)
}

fn align_up(value: usize, align: usize) -> usize {
    value.div_ceil(align) * align
}

#[cfg(test)]
mod tests {
    use lullaby_lexer::lex;
    use lullaby_parser::parse;
    use lullaby_semantics::validate;

    use super::*;
    use crate::lower;

    fn layout(source: &str) -> FrameLayout {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let checked = validate(&program).expect("semantic");
        let module = lower(&checked).expect("lower");
        analyze_frame_layout(module.functions.first().expect("one function"))
    }

    #[test]
    fn assigns_params_then_locals_deterministically() {
        let source = "fn add x i64 y i64 -> i64\n    let sum i64 = x + y\n    sum\n";
        let a = layout(source);
        let b = layout(source);
        assert_eq!(a, b, "layout must be a pure function of the IR");

        let names: Vec<_> = a.slots.iter().map(|slot| slot.name.as_str()).collect();
        assert_eq!(names, ["x", "y", "sum"]);
        assert_eq!(a.slots[0].class, SlotClass::Parameter);
        assert_eq!(a.slots[2].class, SlotClass::Local);
        // Slots never overlap and are word-aligned.
        assert_eq!(a.slots[0].offset, 0);
        assert_eq!(a.slots[1].offset, 8);
        assert_eq!(a.slots[2].offset, 16);
        assert_eq!(a.frame_size % FRAME_ALIGN, 0);
        assert!(a.frame_size >= 24);
    }

    #[test]
    fn nested_blocks_get_child_scopes_with_reverse_cleanup() {
        let source = "fn main -> i64\n    let a i64 = 1\n    if true\n        let b i64 = 2\n        let c i64 = 3\n        b + c\n    else\n        0\n    a\n";
        let layout = layout(source);

        // `a` is function scope (depth 0); `b`/`c` are in the then-branch child.
        assert_eq!(layout.slot("a").expect("a").scope_depth, 0);
        assert_eq!(layout.slot("b").expect("b").scope_depth, 1);
        assert_eq!(layout.slot("c").expect("c").scope_depth, 1);

        // The then-branch scope cleans up in reverse declaration order.
        let then_scope = layout
            .scopes
            .iter()
            .find(|scope| scope.cleanup_order.contains(&"b".to_string()))
            .expect("then scope");
        assert_eq!(then_scope.cleanup_order, ["c", "b"]);
        assert_eq!(then_scope.scope_depth, 1);
    }

    #[test]
    fn for_loop_variable_lives_in_body_scope() {
        let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    total\n";
        let layout = layout(source);

        let i = layout.slot("i").expect("loop var");
        assert_eq!(i.class, SlotClass::LoopVariable);
        assert_eq!(i.scope_depth, 1);
        assert_eq!(layout.slot("total").expect("total").scope_depth, 0);
    }

    #[test]
    fn while_loop_locals_are_scoped_and_cleaned_up() {
        let source = "fn main -> i64\n    let x i64 = 0\n    while x < 3\n        let step i64 = 1\n        x += step\n    x\n";
        let layout = layout(source);
        let while_scope = layout
            .scopes
            .iter()
            .find(|scope| scope.cleanup_order.contains(&"step".to_string()))
            .expect("while scope");
        assert_eq!(while_scope.scope_depth, 1);
        assert_eq!(while_scope.cleanup_order, ["step"]);
    }

    #[test]
    fn early_return_still_yields_complete_static_cleanup_plan() {
        // An early return does not change the static layout: every declared
        // binding still has a slot and every non-empty scope has a cleanup plan.
        let source = "fn main -> i64\n    let a i64 = 1\n    if a == 1\n        let b i64 = 2\n        return b\n    let c i64 = 3\n    c\n";
        let layout = layout(source);

        for name in ["a", "b", "c"] {
            assert!(layout.slot(name).is_some(), "missing slot for {name}");
        }
        // Function scope (a, c) cleans up in reverse declaration order.
        let function_scope = layout
            .scopes
            .iter()
            .find(|scope| scope.scope_depth == 0)
            .expect("function scope");
        assert_eq!(function_scope.cleanup_order, ["c", "a"]);
    }
}
