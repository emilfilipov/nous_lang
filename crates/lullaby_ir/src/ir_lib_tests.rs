
use std::path::{Path, PathBuf};

use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_runtime::run_main as run_ast_main;
use lullaby_semantics::{validate, validate_executable};

use super::*;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn lower_source(source: &str) -> IrModule {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate(&program).expect("semantic");
    lower(&checked).expect("lower")
}

fn run_all_backends(source: &str) -> (Value, Value, Value) {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate_executable(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    let bytecode = lower_to_bytecode(&ir);
    (
        run_ast_main(&program).expect("ast run"),
        run_main(&ir).expect("ir run"),
        run_bytecode_main(&bytecode).expect("bytecode run"),
    )
}

/// Recursively collect every slot-resolved `Local { name, packed }` and every
/// remaining name-scanned `Variable(name)` read in a resolved block. Used by the
/// slot-resolution tests to assert both that reads are rewritten and that their
/// `(depth, slot)` encoding is exactly right.
fn collect_reads(body: &[IrStmt], locals: &mut Vec<(String, u32)>, vars: &mut Vec<String>) {
    fn walk_expr(e: &IrExpr, locals: &mut Vec<(String, u32)>, vars: &mut Vec<String>) {
        match &e.kind {
            IrExprKind::Local { name, packed } => locals.push((name.clone(), *packed)),
            IrExprKind::Variable(name) => vars.push(name.clone()),
            IrExprKind::Binary { left, right, .. } => {
                walk_expr(left, locals, vars);
                walk_expr(right, locals, vars);
            }
            IrExprKind::Unary { expr: inner, .. } | IrExprKind::Await { expr: inner } => {
                walk_expr(inner, locals, vars);
            }
            IrExprKind::Index { target, index } => {
                walk_expr(target, locals, vars);
                walk_expr(index, locals, vars);
            }
            IrExprKind::Field { target, .. } => walk_expr(target, locals, vars),
            IrExprKind::Call { args, .. } => {
                for arg in args {
                    walk_expr(arg, locals, vars);
                }
            }
            IrExprKind::Array(items) => {
                for item in items {
                    walk_expr(item, locals, vars);
                }
            }
            _ => {}
        }
    }
    for stmt in body {
        match stmt {
            IrStmt::Let { value, .. } | IrStmt::Expr(value) | IrStmt::Throw { value, .. } => {
                walk_expr(value, locals, vars)
            }
            IrStmt::Return(Some(value)) => walk_expr(value, locals, vars),
            IrStmt::Assign { value, path, .. } => {
                walk_expr(value, locals, vars);
                for place in path {
                    if let IrPlace::Index(index) = place {
                        walk_expr(index, locals, vars);
                    }
                }
            }
            IrStmt::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    walk_expr(&branch.condition, locals, vars);
                    collect_reads(&branch.body, locals, vars);
                }
                collect_reads(else_body, locals, vars);
            }
            IrStmt::While {
                condition, body, ..
            } => {
                walk_expr(condition, locals, vars);
                collect_reads(body, locals, vars);
            }
            IrStmt::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                walk_expr(start, locals, vars);
                walk_expr(end, locals, vars);
                if let Some(step) = step {
                    walk_expr(step, locals, vars);
                }
                collect_reads(body, locals, vars);
            }
            IrStmt::Loop { body, .. } => collect_reads(body, locals, vars),
            IrStmt::Try {
                body, catch_body, ..
            } => {
                collect_reads(body, locals, vars);
                collect_reads(catch_body, locals, vars);
            }
            IrStmt::Match {
                scrutinee, arms, ..
            } => {
                walk_expr(scrutinee, locals, vars);
                for arm in arms {
                    collect_reads(&arm.body, locals, vars);
                }
            }
            _ => {}
        }
    }
}

#[test]
fn slot_resolution_rewrites_local_reads_with_exact_slots() {
    let mut module = lower_source("fn add x i64 y i64 -> i64\n    let s i64 = x + y\n    s\n");
    resolve_module_slots(&mut module);
    let mut locals = Vec::new();
    let mut vars = Vec::new();
    collect_reads(&module.functions[0].body, &mut locals, &mut vars);
    // Params `x`,`y` are slots 0,1 of the function scope; `s` is slot 2. Every
    // read is resolved (function scope is the innermost, so depth 0).
    assert!(locals.contains(&("x".to_string(), pack_slot(0, 0).unwrap())));
    assert!(locals.contains(&("y".to_string(), pack_slot(0, 1).unwrap())));
    assert!(locals.contains(&("s".to_string(), pack_slot(0, 2).unwrap())));
    assert!(
        vars.is_empty(),
        "every local read should be slot-resolved, found name-scanned {vars:?}"
    );
}

#[test]
fn slot_resolution_models_the_two_scope_for_loop() {
    // The `for` loop variable lives one scope above the body (the evaluator
    // pushes a loop-var scope, then a separate body scope each iteration), so
    // from inside the body `i` is at depth 1 slot 0 and the outer `t` — the
    // only function-scope local — is at depth 2 slot 0.
    let mut module = lower_source(
        "fn main -> i64\n    let t i64 = 0\n    for i from 1 to 3\n        t = t + i\n    t\n",
    );
    resolve_module_slots(&mut module);
    let mut locals = Vec::new();
    let mut vars = Vec::new();
    collect_reads(&module.functions[0].body, &mut locals, &mut vars);
    assert!(
        locals.contains(&("i".to_string(), pack_slot(1, 0).unwrap())),
        "loop var `i` should resolve to depth 1 slot 0, got {locals:?}"
    );
    assert!(
        locals.contains(&("t".to_string(), pack_slot(2, 0).unwrap())),
        "outer `t` read inside the loop body should be depth 2 slot 0, got {locals:?}"
    );
}

#[test]
fn slot_resolution_keeps_shadowing_correct_across_backends() {
    // The inner `x` (10) shadows the outer `x` (1) only inside the `if` body,
    // so `s` accumulates 10 there and 1 after — 11 total. A wrong `(depth,
    // slot)` for either `x` would read the wrong binding; every backend
    // (including the slot-resolved IR/bytecode) must still return 11.
    let source = "fn main -> i64\n    let x i64 = 1\n    let s i64 = 0\n    if x == 1\n        let x i64 = 10\n        s = s + x\n    s = s + x\n    s\n";
    let (ast, ir, bytecode) = run_all_backends(source);
    assert_eq!(ast, Value::I64(11));
    assert_eq!(ir, Value::I64(11));
    assert_eq!(bytecode, Value::I64(11));
}

#[test]
fn vm_eligibility_gate_excludes_unsupported_constructs() {
    // A plain scalar/loop function compiles to a flat VM program; a function
    // containing a `match` (which the VM does not lower) is ineligible and
    // returns `None`, so it falls back to the tree-walker.
    let simple = lower_source(
        "fn sum n i64 -> i64\n    let acc i64 = 0\n    for i from 1 to n\n        acc = acc + i\n    acc\n",
    );
    assert!(
        compile_function_to_vm(&simple.functions[0]).is_some(),
        "a scalar for-loop function should be VM-eligible"
    );
    let with_match = lower_source(
        "enum E\n    A\n    B\n\nfn pick e E -> i64\n    match e\n        A -> 1\n        B -> 2\n",
    );
    let pick = with_match
        .functions
        .iter()
        .find(|f| f.name == "pick")
        .expect("pick");
    assert!(
        compile_function_to_vm(pick).is_none(),
        "a function containing `match` must be VM-ineligible (falls back)"
    );
}

#[test]
fn vm_matches_tree_walkers_on_mixed_program() {
    // A program exercising recursion, a range `for`, a `while`, nested `if`,
    // early `return`, a struct field read, and string concatenation — all
    // through the VM on the bytecode tier — must return the identical value the
    // AST and IR tree-walkers do.
    let source = concat!(
        "struct P\n    x i64\n    y i64\n\n",
        "fn fib n i64 -> i64\n    if n < 2\n        return n\n    return fib(n - 1) + fib(n - 2)\n\n",
        "fn tri n i64 -> i64\n    let acc i64 = 0\n    for i from 1 to n\n        acc = acc + i\n    acc\n\n",
        "fn countdown n i64 -> i64\n    let steps i64 = 0\n    while n > 0\n        n = n - 1\n        steps = steps + 1\n    steps\n\n",
        "fn main -> i64\n    let p P = P(3, 4)\n    fib(10) + tri(5) + countdown(7) + p.x * p.y\n",
    );
    let (ast, ir, bytecode) = run_all_backends(source);
    // fib(10)=55, tri(5)=15, countdown(7)=7, p.x*p.y=12 -> 89.
    assert_eq!(ast, Value::I64(89));
    assert_eq!(ir, ast);
    assert_eq!(bytecode, ast);
}

fn run_all_backend_variants(source: &str) -> (Value, Value, Value, Value, Value) {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate_executable(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    let bytecode = lower_to_bytecode(&ir);
    let (optimized, _) = optimize(&ir, &OptimizationConfig::full());
    let optimized_bytecode = lower_to_bytecode(&optimized);

    (
        run_ast_main(&program).expect("ast run"),
        run_main(&ir).expect("ir run"),
        run_bytecode_main(&bytecode).expect("bytecode run"),
        run_main(&optimized).expect("optimized ir run"),
        run_bytecode_main(&optimized_bytecode).expect("optimized bytecode run"),
    )
}

fn executable_fixture_source(path: &Path) -> Option<String> {
    let source = fs::read_to_string(path).expect("fixture source");
    let tokens = lex(&source).expect("fixture lex");
    let program = parse(&tokens).expect("fixture parse");
    validate_executable(&program).ok().map(|_| {
        source.replace(
            "target/lullaby_fixture_io.txt",
            "target/lullaby_ir_fixture_io.txt",
        )
    })
}

fn cleanup_parity_files() {
    fs::create_dir_all("target").expect("target directory");
    let _ = fs::remove_file("target/lullaby_ir_fixture_io.txt");
}

#[test]
fn lowers_functions_with_typed_params_and_return() {
    let module = lower_source("fn add x i64 y i64 -> i64\n    x + y\n");
    assert_eq!(module.functions.len(), 1);
    let function = &module.functions[0];
    assert_eq!(function.name, "add");
    assert_eq!(function.params[0].ty, TypeRef::new("i64"));
    assert_eq!(function.return_type, TypeRef::new("i64"));
    let IrStmt::Expr(expr) = &function.body[0] else {
        panic!("expected expression statement");
    };
    assert_eq!(expr.ty, TypeRef::new("i64"));
}

#[test]
fn lowers_arrays_and_index_expression_types() {
    let module =
        lower_source("fn main -> i64\n    let values array<i64> = [1, 2, 3]\n    values[1]\n");
    let function = &module.functions[0];
    let IrStmt::Let { value, .. } = &function.body[0] else {
        panic!("expected let statement");
    };
    assert_eq!(value.ty, TypeRef::new("array<i64>"));
    let IrStmt::Expr(expr) = &function.body[1] else {
        panic!("expected expression statement");
    };
    assert_eq!(expr.ty, TypeRef::new("i64"));
}

#[test]
fn lowers_control_flow_and_builtins() {
    let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + total\n";
    let module = lower_source(source);
    let function = &module.functions[0];
    assert!(matches!(function.body[1], IrStmt::Expr(_)));
    assert!(matches!(function.body[3], IrStmt::For { .. }));
    let IrStmt::Let { value, .. } = &function.body[4] else {
        panic!("expected load binding");
    };
    assert_eq!(value.ty, TypeRef::new("i64"));
}

#[test]
fn memory_analysis_reports_memory_operations_and_safety_metadata() {
    let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let values array<i64> = [1, 2, 3]\n    let selected i64 = values[1]\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + selected\n";
    let module = lower_source(source);
    let operations = analyze_memory_operations(&module);

    assert_eq!(operations.len(), 5);
    assert_eq!(
        operations
            .iter()
            .map(|operation| operation.sequence)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );
    assert_eq!(operations[0].function, "main");
    assert!(matches!(
        operations[0].kind,
        IrMemoryOperationKind::Allocate { .. }
    ));
    assert_eq!(
        operations[0].safety.cleanup_role,
        IrCleanupRole::CreatesResource
    );
    assert!(operations[0].safety.mutates_memory);

    assert!(matches!(
        operations[1].kind,
        IrMemoryOperationKind::Store { .. }
    ));
    assert_eq!(
        operations[1].safety.cleanup_role,
        IrCleanupRole::UsesResource
    );
    assert!(operations[1].safety.requires_live_resource);
    assert!(operations[1].safety.unsafe_boundary);

    assert!(matches!(
        operations[2].kind,
        IrMemoryOperationKind::BoundsCheck { .. }
    ));
    assert_eq!(
        operations[2].safety.cleanup_role,
        IrCleanupRole::CheckedAccess
    );
    assert!(operations[2].safety.requires_bounds_check);
    assert!(!operations[2].safety.mutates_memory);

    assert!(matches!(
        operations[3].kind,
        IrMemoryOperationKind::Load { .. }
    ));
    assert!(operations[3].safety.requires_live_resource);
    assert!(!operations[3].safety.mutates_memory);

    assert!(matches!(
        operations[4].kind,
        IrMemoryOperationKind::Deallocate { .. }
    ));
    assert_eq!(
        operations[4].safety.cleanup_role,
        IrCleanupRole::ReleasesResource
    );
    assert!(operations[4].safety.mutates_memory);
}

#[test]
fn memory_analysis_covers_region_copy_and_cleanup_end_to_end() {
    // A single program exercising region creation, reference copy
    // (rc_clone), and compiler-visible cleanup (rc_release) produces all
    // three memory-operation kinds from parseable source, lowered end to end.
    let module = lower_source(
        "fn main -> i64\n    region pool: size=64, align=8\n    let h rc<i64> = rc_new(7)\n    let s rc<i64> = rc_clone(h)\n    rc_release(s)\n    rc_release(h)\n    0\n",
    );
    let operations = analyze_memory_operations(&module);
    use IrMemoryOperationKind::*;
    assert!(
        operations
            .iter()
            .any(|op| matches!(op.kind, RegionCreate { .. }))
    );
    assert!(operations.iter().any(|op| matches!(op.kind, Copy { .. })));
    assert!(
        operations
            .iter()
            .any(|op| matches!(op.kind, Cleanup { .. }))
    );
    // Every reported operation carries safety metadata for optimizer/codegen.
    assert!(operations.iter().all(|op| matches!(
        op.safety.cleanup_role,
        IrCleanupRole::CreatesResource
            | IrCleanupRole::UsesResource
            | IrCleanupRole::ReleasesResource
            | IrCleanupRole::CheckedAccess
    )));
}

#[test]
fn memory_analysis_reports_region_creation() {
    let module = lower_source("fn main -> i64\n    region pool: size=4096, align=16\n    0\n");
    let operations = analyze_memory_operations(&module);
    let region = operations
        .iter()
        .find(|op| matches!(op.kind, IrMemoryOperationKind::RegionCreate { .. }))
        .expect("region create op");
    let IrMemoryOperationKind::RegionCreate { region_type } = &region.kind else {
        unreachable!()
    };
    assert_eq!(region_type.name, "region<pool>");
}

#[test]
fn memory_analysis_reports_reference_operations() {
    let source = "fn main -> i64\n    let h rc<i64> = rc_new(1)\n    let s rc<i64> = rc_clone(h)\n    let v i64 = rc_get(h)\n    rc_release(s)\n    rc_release(h)\n    v\n";
    let module = lower_source(source);
    let operations = analyze_memory_operations(&module);

    assert!(
        operations
            .iter()
            .any(|op| matches!(op.kind, IrMemoryOperationKind::Allocate { .. })),
        "rc_new should be an allocation"
    );
    assert!(
        operations
            .iter()
            .any(|op| matches!(op.kind, IrMemoryOperationKind::Copy { .. })),
        "rc_clone should be a copy/share"
    );
    assert!(
        operations
            .iter()
            .any(|op| matches!(op.kind, IrMemoryOperationKind::Load { .. })),
        "rc_get should be a load"
    );
    let cleanups = operations
        .iter()
        .filter(|op| matches!(op.kind, IrMemoryOperationKind::Cleanup { .. }))
        .count();
    assert_eq!(cleanups, 2, "two rc_release calls should be cleanups");
}

#[test]
fn planned_memory_operation_kinds_have_safety_metadata() {
    let cases = [
        (
            IrMemoryOperationKind::RegionCreate {
                region_type: TypeRef::new("region"),
            },
            IrCleanupRole::CreatesResource,
            false,
            true,
            false,
        ),
        (
            IrMemoryOperationKind::RegionResize {
                region_type: TypeRef::new("region"),
            },
            IrCleanupRole::UsesResource,
            true,
            true,
            true,
        ),
        (
            IrMemoryOperationKind::Copy {
                source_type: TypeRef::new("ptr_i64"),
                target_type: TypeRef::new("ptr_i64"),
            },
            IrCleanupRole::UsesResource,
            true,
            true,
            true,
        ),
        (
            IrMemoryOperationKind::Cleanup {
                resource_type: TypeRef::new("ptr_i64"),
            },
            IrCleanupRole::ReleasesResource,
            true,
            true,
            false,
        ),
    ];

    for (kind, role, requires_live_resource, mutates_memory, unsafe_boundary) in cases {
        let safety = memory_safety_for_kind(&kind).expect("planned memory safety");
        assert_eq!(safety.cleanup_role, role);
        assert_eq!(safety.requires_live_resource, requires_live_resource);
        assert_eq!(safety.mutates_memory, mutates_memory);
        assert_eq!(safety.unsafe_boundary, unsafe_boundary);
        assert!(!safety.requires_bounds_check);
    }
}

#[test]
fn constant_folding_rewrites_pure_literal_expressions() {
    let module = lower_source(
        "fn main -> i64\n    let value i64 = (2 + 3) * (10 - 6)\n    if not false and 1 < 2\n        value + 22\n    else\n        0\n",
    );

    let (optimized, report) = optimize(&module, &OptimizationConfig::constant_folding());
    assert_eq!(
        report.applied_passes,
        vec![OptimizationPass::ConstantFolding]
    );
    assert!(report.folded_expressions >= 5);

    let function = &optimized.functions[0];
    let IrStmt::Let { value, .. } = &function.body[0] else {
        panic!("expected let statement");
    };
    assert_eq!(value.kind, IrExprKind::Integer(20));
    let IrStmt::If { branches, .. } = &function.body[1] else {
        panic!("expected if statement");
    };
    assert_eq!(branches[0].condition.kind, IrExprKind::Bool(true));
}

#[test]
fn constant_folding_preserves_runtime_divide_by_zero() {
    let module = lower_source("fn main -> i64\n    1 / 0\n");
    let (optimized, report) = optimize(&module, &OptimizationConfig::constant_folding());

    assert_eq!(report.folded_expressions, 0);
    assert_eq!(
        run_main(&optimized).expect_err("division by zero").code,
        "L0404"
    );
}

#[test]
fn optimization_passes_can_be_disabled() {
    let module = lower_source("fn main -> i64\n    40 + 2\n");
    let (optimized, report) = optimize(&module, &OptimizationConfig::none());

    assert_eq!(optimized, module);
    assert!(report.applied_passes.is_empty());
    assert_eq!(report.folded_expressions, 0);
}

#[test]
fn dead_code_elimination_removes_statements_after_return() {
    let module = lower_source("fn main -> i64\n    return 42\n    0\n");
    let (optimized, report) = optimize(&module, &OptimizationConfig::dead_code_elimination());

    assert_eq!(
        report.applied_passes,
        vec![OptimizationPass::DeadCodeElimination]
    );
    assert_eq!(report.removed_dead_statements, 1);
    assert_eq!(optimized.functions[0].body.len(), 1);
    assert!(matches!(optimized.functions[0].body[0], IrStmt::Return(_)));
}

#[test]
fn dead_code_elimination_rewrites_nested_blocks() {
    let module = lower_source(
        "fn main -> i64\n    let total i64 = 0\n    if true\n        return 1\n        total + 1\n    else\n        return 2\n        total + 2\n    loop\n        break\n        total += 1\n    total\n",
    );
    let (optimized, report) = optimize(&module, &OptimizationConfig::dead_code_elimination());

    assert_eq!(report.removed_dead_statements, 3);
    let IrStmt::If {
        branches,
        else_body,
        ..
    } = &optimized.functions[0].body[1]
    else {
        panic!("expected if statement");
    };
    assert_eq!(branches[0].body.len(), 1);
    assert_eq!(else_body.len(), 1);

    let IrStmt::Loop { body, .. } = &optimized.functions[0].body[2] else {
        panic!("expected loop statement");
    };
    assert_eq!(body.len(), 1);
    assert!(matches!(body[0], IrStmt::Break(_)));
}

#[test]
fn copy_propagation_rewrites_straight_line_aliases() {
    let module = lower_source(
        "fn main -> i64\n    let base i64 = 40\n    let copy i64 = base\n    let second i64 = copy\n    second + 2\n",
    );
    let (optimized, report) = optimize(&module, &OptimizationConfig::copy_propagation());

    assert_eq!(
        report.applied_passes,
        vec![OptimizationPass::CopyPropagation]
    );
    assert_eq!(report.propagated_copies, 2);

    let IrStmt::Let { value, .. } = &optimized.functions[0].body[2] else {
        panic!("expected propagated let binding");
    };
    assert_eq!(value.kind, IrExprKind::Variable("base".to_string()));

    let IrStmt::Expr(expr) = &optimized.functions[0].body[3] else {
        panic!("expected final expression");
    };
    let IrExprKind::Binary { left, .. } = &expr.kind else {
        panic!("expected binary expression");
    };
    assert_eq!(left.kind, IrExprKind::Variable("base".to_string()));
}

#[test]
fn copy_propagation_invalidates_aliases_after_source_assignment() {
    let source = "fn main -> i64\n    let source i64 = 1\n    let copy i64 = source\n    source = 2\n    copy\n";
    let module = lower_source(source);
    let (optimized, report) = optimize(&module, &OptimizationConfig::copy_propagation());

    assert_eq!(report.propagated_copies, 0);
    assert_eq!(
        run_main(&optimized).expect("optimized run"),
        run_all_backends(source).0
    );

    let IrStmt::Expr(expr) = &optimized.functions[0].body[3] else {
        panic!("expected final expression");
    };
    assert_eq!(expr.kind, IrExprKind::Variable("copy".to_string()));
}

#[test]
fn common_subexpression_elimination_reuses_prior_pure_binding() {
    let source = "fn main -> i64\n    let base i64 = 4\n    let first i64 = (base + 1) * (base + 2)\n    let second i64 = (base + 1) * (base + 2)\n    first + second\n";
    let module = lower_source(source);
    let (optimized, report) = optimize(
        &module,
        &OptimizationConfig::common_subexpression_elimination(),
    );

    assert_eq!(
        report.applied_passes,
        vec![OptimizationPass::CommonSubexpressionElimination]
    );
    assert_eq!(report.eliminated_common_subexpressions, 1);
    assert_eq!(
        run_main(&optimized).expect("optimized run"),
        run_all_backends(source).0
    );

    let IrStmt::Let { value, .. } = &optimized.functions[0].body[2] else {
        panic!("expected second binding");
    };
    assert_eq!(value.kind, IrExprKind::Variable("first".to_string()));
}

#[test]
fn common_subexpression_elimination_invalidates_after_assignment() {
    let source = "fn main -> i64\n    let source i64 = 1\n    let first i64 = source + 1\n    source = 2\n    let second i64 = source + 1\n    first + second\n";
    let module = lower_source(source);
    let (optimized, report) = optimize(
        &module,
        &OptimizationConfig::common_subexpression_elimination(),
    );

    assert_eq!(report.eliminated_common_subexpressions, 0);
    assert_eq!(
        run_main(&optimized).expect("optimized run"),
        run_all_backends(source).0
    );

    let IrStmt::Let { value, .. } = &optimized.functions[0].body[3] else {
        panic!("expected second binding");
    };
    assert!(matches!(value.kind, IrExprKind::Binary { .. }));
}

#[test]
fn loop_invariant_motion_hoists_safe_binding_from_for_body() {
    let source = "fn main -> i64\n    let base i64 = 3\n    let total i64 = 0\n    for i from 1 to 3\n        let invariant i64 = (base + 1) * 2\n        total += invariant + i\n    total\n";
    let module = lower_source(source);
    let (optimized, report) = optimize(&module, &OptimizationConfig::loop_invariant_motion());

    assert_eq!(
        report.applied_passes,
        vec![OptimizationPass::LoopInvariantMotion]
    );
    assert_eq!(report.hoisted_loop_invariants, 1);
    assert_eq!(
        run_main(&optimized).expect("optimized run"),
        run_all_backends(source).0
    );

    let function = &optimized.functions[0];
    let IrStmt::Let {
        name: temp_name,
        value: temp_value,
        ..
    } = &function.body[2]
    else {
        panic!("expected hoisted temp binding");
    };
    assert!(temp_name.starts_with("__lullaby_loop_invariant_"));
    assert!(matches!(temp_value.kind, IrExprKind::Binary { .. }));

    let IrStmt::For { body, .. } = &function.body[3] else {
        panic!("expected for loop after hoisted binding");
    };
    let IrStmt::Let { value, .. } = &body[0] else {
        panic!("expected rewritten loop binding");
    };
    assert_eq!(value.kind, IrExprKind::Variable(temp_name.clone()));
}

#[test]
fn loop_invariant_motion_keeps_loop_variable_dependency_in_place() {
    let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        let value i64 = i + 1\n        total += value\n    total\n";
    let module = lower_source(source);
    let (optimized, report) = optimize(&module, &OptimizationConfig::loop_invariant_motion());

    assert_eq!(report.hoisted_loop_invariants, 0);
    assert_eq!(
        run_main(&optimized).expect("optimized run"),
        run_all_backends(source).0
    );

    let IrStmt::For { body, .. } = &optimized.functions[0].body[1] else {
        panic!("expected for loop");
    };
    let IrStmt::Let { value, .. } = &body[0] else {
        panic!("expected loop-local binding");
    };
    assert!(matches!(value.kind, IrExprKind::Binary { .. }));
}

#[test]
fn loop_invariant_motion_does_not_hoist_potential_runtime_failure() {
    let source = "fn main -> i64\n    while false\n        let value i64 = 1 / 0\n    42\n";
    let module = lower_source(source);
    let (optimized, report) = optimize(&module, &OptimizationConfig::loop_invariant_motion());

    assert_eq!(report.hoisted_loop_invariants, 0);
    assert_eq!(run_main(&optimized).expect("optimized run"), Value::I64(42));

    let IrStmt::While { body, .. } = &optimized.functions[0].body[0] else {
        panic!("expected while loop");
    };
    let IrStmt::Let { value, .. } = &body[0] else {
        panic!("expected loop-local binding");
    };
    assert!(matches!(
        value.kind,
        IrExprKind::Binary {
            op: BinaryOp::Divide,
            ..
        }
    ));
}

#[test]
fn full_optimizer_runs_full_pass_pipeline() {
    let module =
        lower_source("fn main -> i64\n    let value i64 = 40 + 2\n    return value\n    0\n");
    let (optimized, report) = optimize(&module, &OptimizationConfig::full());

    assert_eq!(
        report.applied_passes,
        vec![
            OptimizationPass::Inlining,
            OptimizationPass::ConstantFolding,
            OptimizationPass::CommonSubexpressionElimination,
            OptimizationPass::LoopInvariantMotion,
            OptimizationPass::CopyPropagation,
            OptimizationPass::DeadCodeElimination
        ]
    );
    assert_eq!(report.folded_expressions, 1);
    assert_eq!(report.removed_dead_statements, 1);
    let IrStmt::Let { value, .. } = &optimized.functions[0].body[0] else {
        panic!("expected let statement");
    };
    assert_eq!(value.kind, IrExprKind::Integer(42));
    assert_eq!(optimized.functions[0].body.len(), 2);
}

#[test]
fn inlining_replaces_leaf_helper_calls_and_preserves_results() {
    // `rem` is a pure leaf helper; its call inside `f` should be inlined to
    // `n - (n / 3) * 3`, and the program's result must be unchanged.
    let module = lower_source(
        "fn rem a i64 b i64 -> i64\n    a - (a / b) * b\nfn f n i64 -> i64\n    rem(n, 3)\nfn main -> i64\n    f(17)\n",
    );
    let before = run_main(&module).expect("run before");
    let (optimized, report) = optimize(&module, &OptimizationConfig::inlining());
    assert_eq!(report.inlined_calls, 1, "the rem call should be inlined");
    // `f`'s body no longer contains a Call node.
    let f = optimized
        .functions
        .iter()
        .find(|fun| fun.name == "f")
        .expect("f exists");
    let body = match &f.body[0] {
        IrStmt::Return(Some(body)) | IrStmt::Expr(body) => body,
        other => panic!("expected a single yielded expression, got {other:?}"),
    };
    assert!(
        !contains_call(body),
        "inlined body must have no call: {body:?}"
    );
    assert_eq!(run_main(&optimized).expect("run after"), before);
    assert_eq!(before, Value::I64(2)); // 17 % 3 == 2
}

fn contains_call(expr: &IrExpr) -> bool {
    match &expr.kind {
        IrExprKind::Call { .. } => true,
        IrExprKind::Unary { expr, .. } => contains_call(expr),
        IrExprKind::Binary { left, right, .. } => contains_call(left) || contains_call(right),
        _ => false,
    }
}

#[test]
fn bytecode_artifact_round_trips_and_executes() {
    let ir = lower_source("fn main -> i64\n    40 + 2\n");
    let bytecode = lower_to_bytecode(&ir);
    let encoded = encode_bytecode_artifact(&bytecode).expect("encode artifact");
    let artifact = decode_bytecode_artifact(&encoded).expect("decode artifact");

    assert_eq!(artifact.format, BYTECODE_ARTIFACT_FORMAT);
    assert_eq!(artifact.version, BYTECODE_ARTIFACT_VERSION);
    assert_eq!(artifact.metadata.target, "lullaby-vm");
    assert_eq!(artifact.metadata.payload, "instruction-bytecode");
    assert_eq!(artifact.entry, "main");
    assert_eq!(artifact.function_table.len(), 1);
    assert_eq!(artifact.function_table[0].name, "main");
    assert!(matches!(
        artifact.module.functions[0].instructions[0],
        BytecodeInstruction::Expr(_)
    ));
    assert_eq!(
        run_bytecode_main(&artifact.module).expect("run artifact bytecode"),
        Value::I64(42)
    );
}

#[test]
fn bytecode_artifact_preserves_memory_operation_metadata() {
    let ir = lower_source(
        "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let values array<i64> = [1, 2, 3]\n    let selected i64 = values[1]\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + selected\n",
    );
    let bytecode = lower_to_bytecode(&ir);
    let encoded = encode_bytecode_artifact(&bytecode).expect("encode artifact");
    let artifact = decode_bytecode_artifact(&encoded).expect("decode artifact");

    assert!(encoded.contains("\"memory_operations\""));
    assert_eq!(
        artifact.memory_operations,
        analyze_bytecode_memory_operations(&artifact.module)
    );
    assert_eq!(artifact.memory_operations.len(), 5);
    assert!(matches!(
        artifact.memory_operations[0].kind,
        IrMemoryOperationKind::Allocate { .. }
    ));
    assert!(matches!(
        artifact.memory_operations[1].kind,
        IrMemoryOperationKind::Store { .. }
    ));
    assert!(matches!(
        artifact.memory_operations[2].kind,
        IrMemoryOperationKind::BoundsCheck { .. }
    ));
    assert!(matches!(
        artifact.memory_operations[3].kind,
        IrMemoryOperationKind::Load { .. }
    ));
    assert!(matches!(
        artifact.memory_operations[4].kind,
        IrMemoryOperationKind::Deallocate { .. }
    ));
}

#[test]
fn bytecode_artifact_rejects_wrong_version() {
    let invalid = format!(
        "{{\"format\":\"{BYTECODE_ARTIFACT_FORMAT}\",\"version\":999,\"entry\":\"main\",\"module\":{{\"functions\":[]}}}}"
    );
    let error = decode_bytecode_artifact(&invalid).expect_err("invalid version");

    assert!(
        error
            .message
            .contains("unsupported bytecode artifact version")
    );
}

#[test]
fn bytecode_artifact_rejects_old_structured_payload_version() {
    let invalid = format!(
        "{{\"format\":\"{BYTECODE_ARTIFACT_FORMAT}\",\"version\":2,\"entry\":\"main\",\"metadata\":{{\"producer\":\"test\",\"target\":\"lullaby-vm\",\"payload\":\"structured-bytecode\"}},\"function_table\":[],\"module\":{{\"functions\":[]}}}}"
    );
    let error = decode_bytecode_artifact(&invalid).expect_err("old artifact");

    assert!(
        error
            .message
            .contains("unsupported bytecode artifact version `2`")
    );
}

#[test]
fn bytecode_artifact_rejects_missing_entry_function() {
    let invalid = format!(
        "{{\"format\":\"{BYTECODE_ARTIFACT_FORMAT}\",\"version\":{BYTECODE_ARTIFACT_VERSION},\"entry\":\"main\",\"metadata\":{{\"producer\":\"test\",\"target\":\"lullaby-vm\",\"payload\":\"instruction-bytecode\"}},\"function_table\":[],\"module\":{{\"functions\":[]}}}}"
    );
    let error = decode_bytecode_artifact(&invalid).expect_err("missing entry");

    assert!(
        error
            .message
            .contains("entry `main` is not present in the module")
    );
}

#[test]
fn bytecode_artifact_rejects_function_table_mismatch() {
    let ir = lower_source("fn main -> i64\n    42\n");
    let bytecode = lower_to_bytecode(&ir);
    let encoded = encode_bytecode_artifact(&bytecode).expect("encode artifact");
    let mut value: serde_json::Value = serde_json::from_str(&encoded).expect("json artifact");
    value["function_table"] = serde_json::json!([]);
    let invalid = serde_json::to_string(&value).expect("invalid json artifact");

    let error = decode_bytecode_artifact(&invalid).expect_err("table mismatch");

    assert!(error.message.contains("function_table does not match"));
}

#[test]
fn bytecode_artifact_rejects_memory_operation_mismatch() {
    let ir = lower_source(
        "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value\n",
    );
    let bytecode = lower_to_bytecode(&ir);
    let encoded = encode_bytecode_artifact(&bytecode).expect("encode artifact");
    let mut value: serde_json::Value = serde_json::from_str(&encoded).expect("json artifact");
    value["memory_operations"] = serde_json::json!([]);
    let invalid = serde_json::to_string(&value).expect("invalid json artifact");

    let error = decode_bytecode_artifact(&invalid).expect_err("memory mismatch");

    assert!(error.message.contains("memory_operations does not match"));
}

#[test]
fn bytecode_artifact_rejects_parameterized_entrypoint() {
    let span = Span::new(1, 1);
    let module = BytecodeModule {
        structs: Vec::new(),
        enums: Vec::new(),
        impls: Vec::new(),
        trait_methods: Vec::new(),
        async_functions: Vec::new(),
        extern_functions: Vec::new(),
        extern_signatures: Vec::new(),
        export_functions: Vec::new(),
        closures: Vec::new(),
        functions: vec![BytecodeFunction {
            name: "main".to_string(),
            params: vec![IrParam {
                name: "argc".to_string(),
                ty: TypeRef::new("i64"),
            }],
            return_type: TypeRef::new("i64"),
            instructions: vec![BytecodeInstruction::Return(Some(BytecodeExpr {
                kind: BytecodeExprKind::Variable("argc".to_string()),
                ty: TypeRef::new("i64"),
                span,
            }))],
            span,
        }],
    };
    let encoded = serde_json::to_string(&BytecodeArtifact::new(module)).expect("encode");
    let error = decode_bytecode_artifact(&encoded).expect_err("parameterized entry");

    assert!(
        error
            .message
            .contains("entry `main` must not require parameters")
    );
}

#[test]
fn bytecode_artifact_rejects_break_outside_loop_instruction() {
    let span = Span::new(1, 1);
    let module = BytecodeModule {
        structs: Vec::new(),
        enums: Vec::new(),
        impls: Vec::new(),
        trait_methods: Vec::new(),
        async_functions: Vec::new(),
        extern_functions: Vec::new(),
        extern_signatures: Vec::new(),
        export_functions: Vec::new(),
        closures: Vec::new(),
        functions: vec![BytecodeFunction {
            name: "main".to_string(),
            params: Vec::new(),
            return_type: TypeRef::new("i64"),
            instructions: vec![BytecodeInstruction::Break(span)],
            span,
        }],
    };
    let encoded = serde_json::to_string(&BytecodeArtifact::new(module)).expect("encode");
    let error = decode_bytecode_artifact(&encoded).expect_err("invalid break");

    assert!(
        error
            .message
            .contains("instruction `break` outside loop in function `main`")
    );
}

#[test]
fn optimized_ir_and_bytecode_match_ast_execution() {
    let source =
        "fn main -> i64\n    let folded i64 = (6 * 7) + (10 / 2)\n    return folded - 5\n    0\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    let (optimized, report) = optimize(&ir, &OptimizationConfig::full());
    let bytecode = lower_to_bytecode(&optimized);

    assert!(report.folded_expressions > 0);
    assert_eq!(report.removed_dead_statements, 1);
    assert_eq!(
        run_main(&optimized).expect("optimized ir run"),
        run_ast_main(&program).expect("ast run")
    );
    assert_eq!(
        run_bytecode_main(&bytecode).expect("optimized bytecode run"),
        run_ast_main(&program).expect("ast run")
    );
}

#[test]
fn ir_and_bytecode_match_ast_for_core_execution() {
    let sources = [
        "fn add x i64 y i64 -> i64\n    x + y\n\nfn main -> i64\n    let value i64 = add(40, 2)\n    value\n",
        "fn main -> i64\n    let x i64 = 0\n    while x < 4\n        x += 1\n    x\n",
        "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    total\n",
        "fn main -> bool\n    false and (1 / 0 == 0) or true\n",
        "fn main -> i64\n    let values array<i64> = [1, 2, 3]\n    values[0] + values[2]\n",
    ];

    for source in sources {
        let (ast, ir, bytecode) = run_all_backends(source);
        assert_eq!(ir, ast);
        assert_eq!(bytecode, ast);
    }
}

#[test]
fn ir_and_bytecode_match_ast_for_error_handling() {
    let source = "fn checked n i64 -> string\n    try\n        if n < 0\n            throw \"neg\"\n        \"ok:\" + to_string(n)\n    catch message\n        \"err:\" + message\n\nfn main -> string\n    checked(5) + \" \" + checked(0 - 1)\n";
    let (ast, ir, bytecode) = run_all_backends(source);
    assert_eq!(ast, Value::String(("ok:5 err:neg".to_string()).into()));
    assert_eq!(ir, ast);
    assert_eq!(bytecode, ast);
}

#[test]
fn ir_and_bytecode_match_ast_for_pattern_matching() {
    let source = concat!(
        "enum Shape\n    Circle i64\n    Rect i64 i64\n    Empty\n\n",
        "enum Color\n    Red\n    Green\n    Blue\n\n",
        "fn area s Shape -> i64\n",
        "    match s\n",
        "        Circle(r) -> r * r\n",
        "        Rect(w, h) -> w * h\n",
        "        Empty -> 0\n\n",
        "fn rank c Color -> i64\n",
        "    match c\n",
        "        Green -> 10\n",
        "        _ -> 1\n\n",
        "fn main -> i64\n",
        "    area(Circle(3)) + area(Rect(4, 5)) + area(Empty) + rank(Green) + rank(Red)\n",
    );
    let (ast, ir, bytecode, optimized_ir, optimized_bytecode) = run_all_backend_variants(source);
    assert_eq!(ast, Value::I64(40));
    assert_eq!(ir, ast);
    assert_eq!(bytecode, ast);
    assert_eq!(optimized_ir, ast);
    assert_eq!(optimized_bytecode, ast);
}

#[test]
fn ir_and_bytecode_match_ast_for_memory_builtins() {
    let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
    let (ast, ir, bytecode) = run_all_backends(source);
    assert_eq!(ir, ast);
    assert_eq!(bytecode, ast);
}

#[test]
fn write_bytes_read_bytes_round_trip_matches_across_backends() {
    // Each backend writes and reads back the same file sequentially, so the
    // fixed path is deterministic. The program reconstructs the byte sum.
    let path = "target/lullaby_ir_bytes_roundtrip.bin";
    let _ = fs::create_dir_all("target");
    let _ = fs::remove_file(path);
    let source = format!(
        "fn main -> i64\n    \
             let data list<byte> = list_new()\n    \
             data = push(data, byte(7))\n    \
             data = push(data, byte(11))\n    \
             write_bytes(\"{path}\", data)\n    \
             let back list<byte> = read_bytes(\"{path}\")\n    \
             byte_val(get(back, 0)) + byte_val(get(back, 1)) + len(back)\n"
    );
    let (ast, ir, bytecode) = run_all_backends(&source);
    // 7 + 11 + 2 == 20
    assert_eq!(ast, Value::I64(20));
    assert_eq!(ir, ast);
    assert_eq!(bytecode, ast);
    let _ = fs::remove_file(path);
}

#[test]
fn to_bytes_from_bytes_byte_len_match_across_backend_variants() {
    // Round-trips "Hi" through `to_bytes`/`from_bytes`, checks byte values,
    // and contrasts `byte_len` (UTF-8 bytes) with `len` (characters) on a
    // multi-byte string. 72 + 105 + len("Hi")=2 + (byte_len=5 - len=4)=1.
    let source = concat!(
        "fn main -> i64\n",
        "    let bytes list<byte> = to_bytes(\"Hi\")\n",
        "    let first i64 = byte_val(get(bytes, 0))\n",
        "    let second i64 = byte_val(get(bytes, 1))\n",
        "    match from_bytes(bytes)\n",
        "        ok(s) -> first + second + len(s) + (byte_len(\"café\") - len(\"café\"))\n",
        "        err(m) -> 0 - len(m)\n",
    );
    let (ast, ir, bytecode, optimized_ir, optimized_bytecode) = run_all_backend_variants(source);
    assert_eq!(ast, Value::I64(180));
    assert_eq!(ir, ast);
    assert_eq!(bytecode, ast);
    assert_eq!(optimized_ir, ast);
    assert_eq!(optimized_bytecode, ast);
}

#[test]
fn from_bytes_invalid_utf8_err_matches_across_backend_variants() {
    // A lone `0xFF` byte is invalid UTF-8: every backend takes the `err`
    // branch identically (no panic, no lossy replacement).
    let source = concat!(
        "fn main -> i64\n",
        "    let bad list<byte> = push(list_new(), byte(255))\n",
        "    match from_bytes(bad)\n",
        "        ok(s) -> len(s)\n",
        "        err(m) -> 1\n",
    );
    let (ast, ir, bytecode, optimized_ir, optimized_bytecode) = run_all_backend_variants(source);
    assert_eq!(ast, Value::I64(1));
    assert_eq!(ir, ast);
    assert_eq!(bytecode, ast);
    assert_eq!(optimized_ir, ast);
    assert_eq!(optimized_bytecode, ast);
}

#[test]
fn os_random_structural_result_matches_across_backend_variants() {
    // `os_random` bytes are non-deterministic, so this asserts only
    // structural, backend-invariant facts: `os_random(16)` yields 16 bytes,
    // `os_random(0)` yields an empty list, and `os_random(-1)` yields `err`
    // (never a panic). Fixed total: 1 + 1 + 1 = 3 on every backend.
    let source = concat!(
        "fn ok_len n i64 -> i64\n",
        "    match os_random(n)\n",
        "        ok(bytes) -> len(bytes)\n",
        "        err(_) -> 0 - 1\n\n",
        "fn main -> i64\n",
        "    let a i64 = 0\n",
        "    if ok_len(16) == 16\n",
        "        a = 1\n",
        "    let b i64 = 0\n",
        "    if ok_len(0) == 0\n",
        "        b = 1\n",
        "    let c i64 = 0\n",
        "    if ok_len(0 - 1) == 0 - 1\n",
        "        c = 1\n",
        "    a + b + c\n",
    );
    let (ast, ir, bytecode, optimized_ir, optimized_bytecode) = run_all_backend_variants(source);
    assert_eq!(ast, Value::I64(3));
    assert_eq!(ir, ast);
    assert_eq!(bytecode, ast);
    assert_eq!(optimized_ir, ast);
    assert_eq!(optimized_bytecode, ast);
}

// ---- move-on-functional-update optimization: correctness parity ----
//
// These exercise the `x = f(x, …)` accumulation idiom, whose target argument
// is now MOVED (not cloned) out of the environment on the interpreter fast
// path. Every case runs through all five interpreter variants
// (AST / IR / bytecode / optimized IR / optimized bytecode) and asserts the
// result equals what the pre-optimization clone path produced, proving the
// move changes no observable behavior.

fn assert_all_variants_eq(source: &str, expected: i64) {
    let (ast, ir, bytecode, optimized_ir, optimized_bytecode) = run_all_backend_variants(source);
    assert_eq!(ast, Value::I64(expected), "AST result");
    assert_eq!(ir, ast, "IR result differs from AST");
    assert_eq!(bytecode, ast, "bytecode result differs from AST");
    assert_eq!(optimized_ir, ast, "optimized IR result differs from AST");
    assert_eq!(
        optimized_bytecode, ast,
        "optimized bytecode result differs from AST"
    );
}

#[test]
fn move_update_builds_list_correctly_across_backends() {
    // `l = push(l, i)` fifty times, then sum every element back. The moved
    // accumulation must yield the exact same list a clone would: sum of
    // 0..=49 is 1225, and the length is 50.
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    for i from 0 to 49\n",
        "        l = push(l, i)\n",
        "    let total i64 = 0\n",
        "    let n i64 = len(l)\n",
        "    for i from 0 to n - 1\n",
        "        total += get(l, i)\n",
        "    total * 100 + len(l)\n",
    );
    // 1225 * 100 + 50 = 122550
    assert_all_variants_eq(source, 122550);
}

#[test]
fn move_update_does_not_corrupt_aliased_binding_across_backends() {
    // Aliasing safety: `let b = a` clones `a`; a subsequent `a = push(a, 9)`
    // moves `a`'s slot but must leave the independent `b` untouched.
    let source = concat!(
        "fn main -> i64\n",
        "    let a list<i64> = list_new()\n",
        "    a = push(a, 1)\n",
        "    a = push(a, 2)\n",
        "    a = push(a, 3)\n",
        "    let b list<i64> = a\n",
        "    a = push(a, 9)\n",
        "    let bsum i64 = get(b, 0) + get(b, 1) + get(b, 2)\n",
        "    len(a) * 10000 + get(a, 3) * 100 + len(b) * 10 + bsum\n",
    );
    // a = [1,2,3,9] (len 4, a[3]=9); b = [1,2,3] (len 3, sum 6).
    // 4*10000 + 9*100 + 3*10 + 6 = 40936. A corrupted (aliased) b would make
    // len(b)=4 and bsum include the 9, changing the result.
    assert_all_variants_eq(source, 40936);
}

#[test]
fn multi_occurrence_concat_is_not_optimized_but_correct_across_backends() {
    // `l = concat(l, l)` has the target in two arguments, so the move must NOT
    // fire; the result must still double the list correctly.
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, 1)\n",
        "    l = push(l, 2)\n",
        "    l = concat(l, l)\n",
        "    len(l) * 10 + get(l, 0) + get(l, 1) + get(l, 2) + get(l, 3)\n",
    );
    // l = [1,2,1,2]: 4*10 + 1+2+1+2 = 46.
    assert_all_variants_eq(source, 46);
}

#[test]
fn target_in_nested_argument_is_not_optimized_but_correct_across_backends() {
    // `l = set(l, len(l) - 1, 99)`: the target appears bare in arg0 and nested
    // inside arg1 (`len(l)`), so the move must NOT fire; setting the last
    // element must still work.
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, 1)\n",
        "    l = push(l, 2)\n",
        "    l = push(l, 3)\n",
        "    l = set(l, len(l) - 1, 99)\n",
        "    get(l, 0) * 100 + get(l, 1) * 10 + get(l, 2)\n",
    );
    // l = [1,2,99]: 1*100 + 2*10 + 99 = 219.
    assert_all_variants_eq(source, 219);
}

#[test]
fn move_update_sort_across_backends() {
    // `l = sort(l)` moves the list into the sort; the ordering must be exact.
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, 3)\n",
        "    l = push(l, 1)\n",
        "    l = push(l, 2)\n",
        "    l = sort(l)\n",
        "    get(l, 0) * 100 + get(l, 1) * 10 + get(l, 2)\n",
    );
    // sorted [1,2,3]: 123.
    assert_all_variants_eq(source, 123);
}

#[test]
fn move_update_map_set_accumulation_across_backends() {
    // `m = map_set(m, k, v)` accumulation with a repeated key overwrite; the
    // moved map must read back the final values.
    let source = concat!(
        "fn get_or m map<string, i64> k string fallback i64 -> i64\n",
        "    match map_get(m, k)\n",
        "        some(v) -> v\n",
        "        none -> fallback\n\n",
        "fn main -> i64\n",
        "    let m map<string, i64> = map_new()\n",
        "    m = map_set(m, \"a\", 1)\n",
        "    m = map_set(m, \"b\", 2)\n",
        "    m = map_set(m, \"a\", 10)\n",
        "    get_or(m, \"a\", 0) * 1000 + get_or(m, \"b\", 0) * 10 + map_len(m)\n",
    );
    // a=10, b=2, map_len=2 => 10000 + 20 + 2 = 10022.
    assert_all_variants_eq(source, 10022);
}

#[test]
fn move_update_string_replace_across_backends() {
    // `s = replace(s, from, to)` moves the string into the builtin; the result
    // must match the clone path exactly.
    let source = concat!(
        "fn main -> i64\n",
        "    let s string = \"aaa\"\n",
        "    s = replace(s, \"a\", \"b\")\n",
        "    s = replace(s, \"b\", \"cc\")\n",
        "    len(s)\n",
    );
    // "aaa" -> "bbb" -> "cccccc": length 6.
    assert_all_variants_eq(source, 6);
}

// ---- move-on-functional-update: the `+` OPERATOR string-building form ----
//
// `s = s + piece` in a loop now moves the accumulator out of the environment
// to serve as the left operand, so `eval_binary`'s string concat reuses its
// heap buffer (`push_str`) instead of cloning-then-reallocating. That makes
// building O(n) rather than O(n^2). These fixtures prove the move changes no
// observable result across all five interpreter variants.

#[test]
fn move_update_string_plus_builds_fifty_chars_across_backends() {
    // `s = s + "x"` fifty times: the buffer-reusing move must yield exactly
    // fifty characters, identical to the clone path.
    let source = concat!(
        "fn main -> i64\n",
        "    let s string = \"\"\n",
        "    for i from 1 to 50\n",
        "        s = s + \"x\"\n",
        "    len(s)\n",
    );
    assert_all_variants_eq(source, 50);
}

#[test]
fn move_update_string_plus_to_string_across_backends() {
    // `s = s + to_string(i)` for i in 0..=9 builds "0123456789"; the other
    // operand is a call evaluated before the move. Length 10.
    let source = concat!(
        "fn main -> i64\n",
        "    let s string = \"\"\n",
        "    for i from 0 to 9\n",
        "        s = s + to_string(i)\n",
        "    len(s)\n",
    );
    assert_all_variants_eq(source, 10);
}

#[test]
fn move_update_string_plus_does_not_corrupt_aliased_binding_across_backends() {
    // Aliasing safety for the `+` form: `let b = a` clones `a`; a later
    // `a = a + "c"` moves `a`'s slot but must leave `b` untouched.
    let source = concat!(
        "fn main -> i64\n",
        "    let a string = \"ab\"\n",
        "    let b string = a\n",
        "    a = a + \"c\"\n",
        "    len(a) * 10 + len(b)\n",
    );
    // a = "abc" (len 3), b = "ab" (len 2) => 32. A corrupted b would read len 3.
    assert_all_variants_eq(source, 32);
}

#[test]
fn multi_occurrence_string_plus_is_not_optimized_but_correct_across_backends() {
    // `s = s + s` has the target as BOTH operands, so the move must NOT fire;
    // the doubling must still be correct.
    let source = concat!(
        "fn main -> i64\n",
        "    let s string = \"ab\"\n",
        "    s = s + s\n",
        "    s = s + s\n",
        "    len(s)\n",
    );
    // "ab" -> "abab" -> "abababab": length 8.
    assert_all_variants_eq(source, 8);
}

#[test]
fn target_nested_with_others_in_string_plus_is_not_optimized_but_correct_across_backends() {
    // `s = pre + s + suf` parses as `(pre + s) + suf`: the target appears
    // alongside other operands (never as the single bare operand of the top
    // op), so the move must NOT fire; wrapping must still be correct.
    let source = concat!(
        "fn main -> i64\n",
        "    let s string = \"X\"\n",
        "    let pre string = \"[\"\n",
        "    let suf string = \"]\"\n",
        "    s = pre + s + suf\n",
        "    s = pre + s + suf\n",
        "    len(s)\n",
    );
    // "X" -> "[X]" -> "[[X]]": length 5.
    assert_all_variants_eq(source, 5);
}

#[test]
fn move_update_string_plus_right_operand_target_across_backends() {
    // Right-operand-is-target: `s = p + s` has `s` as the single bare RIGHT
    // operand, so the move fires on that operand (prepending). Three
    // iterations: "!" -> "ab!" -> "abab!" -> "ababab!", length 7.
    let source = concat!(
        "fn main -> i64\n",
        "    let s string = \"!\"\n",
        "    let p string = \"ab\"\n",
        "    for i from 1 to 3\n",
        "        s = p + s\n",
        "    len(s)\n",
    );
    assert_all_variants_eq(source, 7);
}

#[test]
fn move_update_string_plus_error_in_other_operand_leaves_target_intact_across_backends() {
    // Error-path safety for the `+` form: in `s = s + boom()` the non-target
    // operand (`boom()`) is evaluated BEFORE the target `s` is moved out, so a
    // throw there leaves `s` intact and a surrounding `catch` observes the
    // original string — never a moved-out placeholder.
    let source = concat!(
        "fn boom -> string\n",
        "    throw \"boom\"\n\n",
        "fn main -> i64\n",
        "    let s string = \"ab\"\n",
        "    let caught i64 = 0\n",
        "    try\n",
        "        s = s + boom()\n",
        "    catch m\n",
        "        caught = 1\n",
        "    caught * 100 + len(s)\n",
    );
    // boom() throws before the move: s stays "ab" (len 2); caught=1 => 102.
    assert_all_variants_eq(source, 102);
}

#[test]
fn move_update_numeric_plus_one_is_correct_across_backends() {
    // `n = n + 1` (numeric, `Copy`-cheap operand) exercises the same binary
    // move path and must stay exactly correct.
    let source = concat!(
        "fn main -> i64\n",
        "    let n i64 = 0\n",
        "    for i from 1 to 10\n",
        "        n = n + 1\n",
        "    n\n",
    );
    assert_all_variants_eq(source, 10);
}

#[test]
fn move_update_error_in_other_argument_leaves_target_intact_across_backends() {
    // Error-path safety: in `l = push(l, boom())` the *other* argument throws
    // BEFORE the target `l` is moved out. The move fast path evaluates other
    // arguments first, so `l` stays intact and a surrounding `catch` observes
    // the original list — never a moved-out placeholder.
    let source = concat!(
        "fn boom -> i64\n",
        "    throw \"boom\"\n\n",
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, 1)\n",
        "    l = push(l, 2)\n",
        "    let caught i64 = 0\n",
        "    try\n",
        "        l = push(l, boom())\n",
        "    catch m\n",
        "        caught = 1\n",
        "    caught * 1000 + len(l) * 10 + get(l, 0) + get(l, 1)\n",
    );
    // boom() throws before the move: l stays [1,2]; caught=1.
    // 1*1000 + 2*10 + 1 + 2 = 1023. A corrupted l would fail len/get or differ.
    assert_all_variants_eq(source, 1023);
}

#[test]
fn user_function_accumulation_is_correct_across_backends() {
    // `acc = add_one(acc)` calls a *user* function, which the fast path does
    // NOT move (user code could raise a catchable error). It must still be
    // correct via the clone path.
    let source = concat!(
        "fn double x list<i64> -> list<i64>\n",
        "    push(x, len(x))\n\n",
        "fn main -> i64\n",
        "    let acc list<i64> = list_new()\n",
        "    acc = double(acc)\n",
        "    acc = double(acc)\n",
        "    acc = double(acc)\n",
        "    len(acc) * 100 + get(acc, 0) * 10 + get(acc, 1) + get(acc, 2)\n",
    );
    // acc: [] -> [0] -> [0,1] -> [0,1,2]: len 3, 3*100 + 0 + 1 + 2 = 303.
    assert_all_variants_eq(source, 303);
}

#[test]
fn executable_valid_fixtures_match_across_backend_variants() {
    cleanup_parity_files();
    let fixture_dir = workspace_root().join("tests/fixtures/valid");
    let mut fixtures = fs::read_dir(&fixture_dir)
        .expect("valid fixture directory")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("lby"))
        .collect::<Vec<_>>();
    fixtures.sort();

    let mut covered = Vec::new();
    for fixture in fixtures {
        let Some(source) = executable_fixture_source(&fixture) else {
            continue;
        };
        cleanup_parity_files();
        let (ast, ir, bytecode, optimized_ir, optimized_bytecode) =
            run_all_backend_variants(&source);
        let name = fixture
            .file_name()
            .and_then(|name| name.to_str())
            .expect("fixture name");
        assert_eq!(ir, ast, "{name}: IR result differs from AST");
        assert_eq!(bytecode, ast, "{name}: bytecode result differs from AST");
        assert_eq!(
            optimized_ir, ast,
            "{name}: optimized IR result differs from AST"
        );
        assert_eq!(
            optimized_bytecode, ast,
            "{name}: optimized bytecode result differs from AST"
        );
        covered.push(name.to_string());
    }
    cleanup_parity_files();

    assert!(
        covered.len() >= 10,
        "expected broad executable fixture coverage, got {covered:?}"
    );
    assert!(covered.contains(&"run_file_io.lby".to_string()));
    assert!(covered.contains(&"run_store.lby".to_string()));
    assert!(covered.contains(&"run_for_step.lby".to_string()));
    assert!(covered.contains(&"run_option_result.lby".to_string()));
    // The `?` error-propagation fixture exercises the AST early-return signal
    // and the IR `?`-desugar at parity across all five backend variants.
    assert!(covered.contains(&"run_error_propagation.lby".to_string()));
}

#[test]
fn lowers_nested_constructor_in_call_argument_position() {
    // The IR lowerer re-derives types, so a nested `list_new()` in argument
    // position must take its element type from the surrounding context (the
    // outer `list<byte>` flowing through `push`), and all backends agree.
    let source = concat!(
        "fn count o option<i64> -> i64\n",
        "    match o\n",
        "        some(v) -> v\n",
        "        none -> 0\n\n",
        "fn main -> i64\n",
        "    let data list<byte> = push(list_new(), byte(65))\n",
        "    let a i64 = count(none)\n",
        "    byte_val(get(data, 0)) + a\n",
    );
    let module = lower_source(source);
    let main = module
        .functions
        .iter()
        .find(|function| function.name == "main")
        .expect("main function");
    let IrStmt::Let { value, .. } = &main.body[0] else {
        panic!("expected list binding");
    };
    // The nested `list_new()` inferred `list<byte>`, so `push` returns it.
    assert_eq!(value.ty, TypeRef::new("list<byte>"));

    let (ast, ir, bytecode, optimized_ir, optimized_bytecode) = run_all_backend_variants(source);
    assert_eq!(ir, ast);
    assert_eq!(bytecode, ast);
    assert_eq!(optimized_ir, ast);
    assert_eq!(optimized_bytecode, ast);
}

#[test]
fn try_operator_desugars_to_let_match_return_and_runs_at_parity() {
    // The IR never contains a `?`/`Try` node (there is no such IrExprKind):
    // `?` is desugared during lowering into a `let __try_q`, a typed
    // `let __try_v`, and a `match` whose failure arm `return`s. The success
    // temporary is what the original position references.
    let source = concat!(
        "fn checked n i64 -> result<i64, string>\n",
        "    if n < 0\n",
        "        return err(\"neg\")\n",
        "    ok(n)\n\n",
        "fn use_it a i64 -> result<i64, string>\n",
        "    let x i64 = checked(a)?\n",
        "    ok(x + 1)\n\n",
        "fn unwrap r result<i64, string> -> i64\n",
        "    match r\n",
        "        ok(v) -> v\n",
        "        err(m) -> 0 - len(m)\n\n",
        "fn main -> i64\n",
        "    unwrap(use_it(4)) + unwrap(use_it(-1))\n",
    );
    let module = lower_source(source);
    let use_it = module
        .functions
        .iter()
        .find(|function| function.name == "use_it")
        .expect("use_it function");

    // The `?`-desugar scaffolding was hoisted ahead of the `let x` binding:
    // `let __try_q_*`, then a typed `let __try_v_*`, then a `match` with a
    // `return`ing failure arm.
    assert!(
        matches!(&use_it.body[0], IrStmt::Let { name, .. } if name.starts_with("__try_q_")),
        "first hoisted statement binds the operand temp: {:?}",
        use_it.body[0]
    );
    let IrStmt::Let {
        name: v_name,
        ty: v_ty,
        ..
    } = &use_it.body[1]
    else {
        panic!(
            "expected the success temp binding, got {:?}",
            use_it.body[1]
        );
    };
    assert!(
        v_name.starts_with("__try_v_"),
        "success temp name: {v_name}"
    );
    assert_eq!(
        *v_ty,
        TypeRef::new("i64"),
        "success temp is typed as the payload"
    );
    let IrStmt::Match { arms, .. } = &use_it.body[2] else {
        panic!("expected the propagation match, got {:?}", use_it.body[2]);
    };
    // Two arms: `ok(..) -> __try_v = ..` and `err(..) -> return err(..)`.
    assert_eq!(arms.len(), 2, "ok + err arms");
    let has_returning_err_arm = arms.iter().any(|arm| {
        matches!(&arm.pattern, IrMatchPattern::Variant { name, .. } if name == "err")
            && matches!(arm.body.as_slice(), [IrStmt::Return(Some(_))])
    });
    assert!(has_returning_err_arm, "err arm returns the failure value");
    // The original `let x` position now references the success temp.
    let IrStmt::Let { value, .. } = &use_it.body[3] else {
        panic!("expected the rewritten `let x`, got {:?}", use_it.body[3]);
    };
    assert_eq!(value.kind, IrExprKind::Variable(v_name.clone()));

    // All five backend variants agree on the observable result.
    // unwrap(use_it(4)) = 5; unwrap(use_it(-1)) = -len("neg") = -3.
    let (ast, ir, bytecode, optimized_ir, optimized_bytecode) = run_all_backend_variants(source);
    assert_eq!(ast, Value::I64(2));
    assert_eq!(ir, ast);
    assert_eq!(bytecode, ast);
    assert_eq!(optimized_ir, ast);
    assert_eq!(optimized_bytecode, ast);
}

#[test]
fn closure_lowers_to_id_node_and_registers_body_table() {
    // A closure literal lowers to a body-less `IrExprKind::Closure { id }` node
    // typed `fn(i64) -> i64`, and its body is registered in the module's
    // closure table keyed by that id.
    let source = concat!(
        "fn apply f fn(i64) -> i64 v i64 -> i64\n",
        "    f(v)\n\n",
        "fn main -> i64\n",
        "    let n i64 = 10\n",
        "    let add_n fn(i64) -> i64 = fn x i64 -> x + n\n",
        "    apply(add_n, 5) + add_n(2)\n",
    );
    let module = lower_source(source);
    assert_eq!(module.closures.len(), 1, "one closure body registered");
    let def = &module.closures[0];
    assert_eq!(def.params, vec!["x".to_string()]);

    let main = module
        .functions
        .iter()
        .find(|function| function.name == "main")
        .expect("main");
    let IrStmt::Let { value, .. } = &main.body[1] else {
        panic!("expected the `let add_n` binding");
    };
    let IrExprKind::Closure { id } = &value.kind else {
        panic!("expected a closure node, got {:?}", value.kind);
    };
    assert_eq!(*id, def.id, "the node id keys the closure table");
    assert_eq!(
        value.ty,
        function_type(&[TypeRef::new("i64")], &TypeRef::new("i64"))
    );
}

#[test]
fn closure_runs_at_parity_across_all_backend_variants() {
    // The canonical capture example returns 27 identically on the AST, IR, and
    // bytecode interpreters plus their optimized variants.
    let source = concat!(
        "fn apply f fn(i64) -> i64 v i64 -> i64\n",
        "    f(v)\n\n",
        "fn main -> i64\n",
        "    let n i64 = 10\n",
        "    let add_n fn(i64) -> i64 = fn x i64 -> x + n\n",
        "    apply(add_n, 5) + add_n(2)\n",
    );
    let (ast, ir, bytecode, optimized_ir, optimized_bytecode) = run_all_backend_variants(source);
    assert_eq!(ast, Value::I64(27));
    assert_eq!(ir, ast);
    assert_eq!(bytecode, ast);
    assert_eq!(optimized_ir, ast);
    assert_eq!(optimized_bytecode, ast);
}

#[test]
fn ir_and_bytecode_preserve_runtime_errors() {
    let source = "fn main -> i64\n    let values array<i64> = [1]\n    values[2]\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    let bytecode = lower_to_bytecode(&ir);

    let ast_error = run_ast_main(&program).expect_err("ast error");
    let ir_error = run_main(&ir).expect_err("ir error");
    let bytecode_error = run_bytecode_main(&bytecode).expect_err("bytecode error");

    assert_eq!(ir_error.code, ast_error.code);
    assert_eq!(bytecode_error.code, ast_error.code);
    assert_eq!(ir_error.span, ast_error.span);
    assert_eq!(bytecode_error.span, ast_error.span);
}
