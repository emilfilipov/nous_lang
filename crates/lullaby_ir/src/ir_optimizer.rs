//! IR-to-IR optimization passes (inlining, constant folding, common
//! subexpression elimination, loop-invariant code motion, copy propagation, dead
//! code elimination). Each pass transforms an `IrModule` and is driven, in order,
//! by the crate's `optimize` fn (in `lib.rs`).
//!
//! Each pass lives in its own submodule (`ir_optimizer_<pass>.rs`); this file is
//! the parent that wires them together and holds the few items shared across
//! passes: the `ExprSignature` type and `combine_signatures` helper (used by both
//! CSE and LICM) and the `expr_requires_optimizer_barrier` predicate (used by
//! both CSE and copy propagation). Uses the crate's IR types via `use super::*`.

use super::*;

#[path = "ir_optimizer_constfold.rs"]
mod ir_optimizer_constfold;
#[path = "ir_optimizer_copyprop.rs"]
mod ir_optimizer_copyprop;
#[path = "ir_optimizer_cse.rs"]
mod ir_optimizer_cse;
#[path = "ir_optimizer_dce.rs"]
mod ir_optimizer_dce;
#[path = "ir_optimizer_inline.rs"]
mod ir_optimizer_inline;
#[path = "ir_optimizer_licm.rs"]
mod ir_optimizer_licm;

pub(crate) use ir_optimizer_constfold::ConstantFolder;
pub(crate) use ir_optimizer_copyprop::CopyPropagator;
pub(crate) use ir_optimizer_cse::CommonSubexpressionEliminator;
pub(crate) use ir_optimizer_dce::DeadCodeEliminator;
pub(crate) use ir_optimizer_inline::Inliner;
pub(crate) use ir_optimizer_licm::LoopInvariantMover;

/// The structural fingerprint of a pure expression: a canonical `key` string
/// (equal keys denote structurally identical pure expressions) plus the set of
/// variable names the expression depends on (`dependencies`). Shared by CSE
/// (which reuses an available binding when keys match) and LICM (which hoists a
/// binding when none of its dependencies are declared or mutated in the loop).
#[derive(Debug, Clone)]
struct ExprSignature {
    key: String,
    dependencies: HashSet<String>,
}

fn combine_signatures(
    prefix: &str,
    ty: &str,
    signatures: Vec<ExprSignature>,
) -> (String, HashSet<String>) {
    let mut dependencies = HashSet::new();
    let mut parts = Vec::new();
    for signature in signatures {
        dependencies.extend(signature.dependencies);
        parts.push(signature.key);
    }
    (format!("{prefix}:{ty}({})", parts.join(",")), dependencies)
}

fn expr_requires_optimizer_barrier(expr: &IrExpr) -> bool {
    match &expr.kind {
        IrExprKind::Call { .. } => true,
        // `await` spawns/joins a thread, so it is never removable dead code.
        IrExprKind::Await { .. } => true,
        IrExprKind::Array(values) => values.iter().any(expr_requires_optimizer_barrier),
        IrExprKind::Index { .. } => true,
        // Field access is pure; only its target can require a barrier.
        IrExprKind::Field { target, .. } => expr_requires_optimizer_barrier(target),
        IrExprKind::Unary { expr, .. } => expr_requires_optimizer_barrier(expr),
        IrExprKind::Binary { left, right, .. } => {
            expr_requires_optimizer_barrier(left) || expr_requires_optimizer_barrier(right)
        }
        // Constructing a closure value only snapshots locals (no side effect), so
        // it is not an optimizer barrier — an unused closure binding is removable.
        IrExprKind::Closure { .. }
        | IrExprKind::Integer(_)
        | IrExprKind::Float(_)
        | IrExprKind::Bool(_)
        | IrExprKind::String(_)
        | IrExprKind::Char(_)
        | IrExprKind::Variable(_)
        | IrExprKind::Local { .. } => false,
    }
}
