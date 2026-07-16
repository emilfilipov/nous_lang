//! Native codegen for closures — Stage 1 (scalar captures, direct non-escaping
//! call). Split out of native_object.rs; sees the parent's items via
//! `use super::*`.
//!
//! A closure literal `fn PARAMS -> EXPR` lowers (in the interpreters) to a
//! `Value::Closure { id, captured }` whose body lives in `BytecodeModule::closures`
//! keyed by the parse-order `id`. The native backend compiles the **narrow,
//! provably-sound** slice of that model:
//!
//! - The closure is created by a direct literal (`let f fn(...) = fn x -> ...`),
//!   captures only **integer-cell scalars** (`i64`/fixed-width/`bool`/`char`/
//!   `byte`), takes at most three integer-cell scalar parameters, returns an
//!   integer-cell scalar, and its single-expression body neither touches the heap
//!   nor calls a user/`extern` function.
//! - The closure local is used **only** as the callee of a direct call
//!   (`f(args)`); it is never passed to a function, returned, reassigned, stored,
//!   or read as a bare value.
//!
//! Everything else — a `string`/`list`/`map`/aggregate capture, a float capture,
//! a closure passed to a higher-order function, a returned/escaping closure, a
//! mutable capture, or more than three parameters — makes the enclosing function
//! **skip cleanly to the interpreters** (`L0339`), never miscompiled.
//!
//! ## Object layout and call ABI
//!
//! A closure value is a heap block `[code_ptr][capture0][capture1]…]` allocated by
//! the shared bump/RC allocator (`__lullaby_alloc`), so it is reclaimed by the
//! arena rewind (a per-iteration sub-region for a non-escaping loop closure) or the
//! RC/free-list path exactly like any other heap block. Word 0 holds the address of
//! the synthesized closure-body function `__closure_{id}` (materialized with a
//! `lea rax,[rip+__closure_{id}]` + REL32 relocation); words 1.. hold the captured
//! scalar values in capture order.
//!
//! A direct call `f(args)` loads word 0 (the code pointer), puts the **env pointer**
//! (the block base) in `rcx`, the arguments in `rdx`/`r8`/`r9` (Win64), and issues
//! an indirect `call rax`. The synthesized body seats `rcx` (env) and its
//! parameters into frame slots; a captured name resolves to `[env + 8*(k+1)]`.

use super::*;

/// The `.text` symbol name of the synthesized body for closure `id`.
pub(crate) fn closure_symbol(id: usize) -> String {
    format!("__closure_{id}")
}

/// Whether a source type is an integer-cell native scalar — `i64`, a fixed-width
/// integer (`i8`…`usize`, stored as a normalized `i64` cell), or `bool`/`char`/
/// `byte`. These are the only capture/parameter/return types Stage 1 supports; a
/// float (`f64`/`f32`), a heap value (`string`/`list`/`map`), or an aggregate is
/// deferred so the enclosing function skips cleanly.
pub(crate) fn is_i64_cell_scalar(ty: &TypeRef) -> bool {
    let name = ty.name.as_str();
    name == "i64" || fixed_int_kind(name).is_some() || matches!(name, "bool" | "char" | "byte")
}

/// The resolved native layout of a Stage-1 closure: its parse-order `id`, its
/// captured free variables (name + integer-cell layout, in capture order), and its
/// parameter/return layouts. A closure with any non-supported piece has no layout
/// (`compute_closure_layout` returns `None`), so an enclosing function referencing
/// it skips gracefully.
#[derive(Debug, Clone)]
pub(crate) struct ClosureLayout {
    /// Captured free variables in capture (first-seen) order: `(name, layout)`.
    /// Each is a single integer-cell word stored at env offset `8*(index+1)`.
    pub(crate) captures: Vec<(String, NativeType)>,
    /// Parameter count (all integer-cell scalars; at most three).
    pub(crate) param_count: usize,
    /// The closure's parameter names, in order (from the closure def).
    pub(crate) param_names: Vec<String>,
}

/// A closure body's env binding while it is being lowered: the frame slot holding
/// the env pointer (block base; word 0 is the code pointer, captures follow) and
/// each captured name's byte offset within the env block.
pub(crate) struct ClosureEnv {
    pub(crate) env_slot: i32,
    pub(crate) captures: HashMap<String, i32>,
}

/// Collect a closure body's **free variables** — the `Variable` reads that are not
/// one of the closure's own parameters — in first-seen (deterministic pre-order)
/// order, each paired with its static type. The body is a single expression with
/// no inner bindings, so every non-parameter variable is a captured enclosing
/// local. Order fixes both the heap block layout and the env offsets, and is shared
/// by the literal lowering (which stores captures) and the body synthesis (which
/// reads them), so they always agree.
fn free_variables(body: &BytecodeExpr, params: &[String]) -> Vec<(String, TypeRef)> {
    fn visit(
        expr: &BytecodeExpr,
        params: &[String],
        seen: &mut std::collections::HashSet<String>,
        out: &mut Vec<(String, TypeRef)>,
    ) {
        if let BytecodeExprKind::Variable(name) = &expr.kind
            && !params.contains(name)
            && seen.insert(name.clone())
        {
            out.push((name.clone(), expr.ty.clone()));
        }
        // Recursing over `expr_children` never descends into a `Closure` node (it
        // carries only an id), so a body containing a nested closure contributes no
        // free variables here and is rejected later by the body-lowering trial.
        for child in expr_children(expr) {
            visit(child, params, seen, out);
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    visit(body, params, &mut seen, &mut out);
    out
}

/// Compute the Stage-1 native layout of a closure definition, or `None` if any
/// piece is outside the supported slice (a non-scalar capture/param/return, more
/// than three params). The body's lowerability (heap-free, no user calls, actually
/// compilable) is verified separately by [`synthesize_closure_body`]; a failure
/// there demotes the enclosing function, so a layout existing here does not on its
/// own promise the body compiles.
pub(crate) fn compute_closure_layout(
    def: &BytecodeClosureDef,
    fn_signature: &(Vec<TypeRef>, TypeRef),
) -> Option<ClosureLayout> {
    let (param_types, ret_ty) = fn_signature;
    // Parameters: at most three integer-cell scalars (the env pointer consumes
    // `rcx`, leaving `rdx`/`r8`/`r9` for arguments).
    if param_types.len() > 3 || def.params.len() != param_types.len() {
        return None;
    }
    if !param_types.iter().all(is_i64_cell_scalar) {
        return None;
    }
    // Return: an integer-cell scalar (leaves the closure body in `rax`).
    if !is_i64_cell_scalar(ret_ty) {
        return None;
    }
    // Captures: every free variable must be an integer-cell scalar.
    let mut captures = Vec::new();
    for (name, ty) in free_variables(&def.body, &def.params) {
        if !is_i64_cell_scalar(&ty) {
            return None;
        }
        captures.push((name, NativeType::I64));
    }
    Some(ClosureLayout {
        captures,
        param_count: param_types.len(),
        param_names: def.params.clone(),
    })
}

/// Whether every use of a closure-bound local `name` in `function` is a
/// **supported** use: the value initializer of its own `let` (a direct closure
/// literal) or the callee of a direct call `name(args)`. Default-deny — a bare
/// value read, being passed as a call argument, a return, a reassignment, a field/
/// index, or any other position makes the closure escape/higher-order, so the
/// enclosing function must skip. This is what keeps `apply(f, x)`, a returned
/// closure, and a stored closure out of the native slice.
pub(crate) fn closure_local_ok(function: &BytecodeFunction, name: &str) -> bool {
    body_closure_use_ok(&function.instructions, name)
}

fn body_closure_use_ok(body: &[BytecodeInstruction], name: &str) -> bool {
    body.iter().all(|stmt| stmt_closure_use_ok(stmt, name))
}

fn stmt_closure_use_ok(stmt: &BytecodeInstruction, name: &str) -> bool {
    match stmt {
        BytecodeInstruction::Let {
            name: bound, value, ..
        } => {
            // The closure local's own declaration binds it to a closure literal —
            // that is its one allowed defining occurrence. Any OTHER `let` must not
            // reference `name` except as a direct call callee inside its value.
            if bound == name {
                matches!(value.kind, BytecodeExprKind::Closure { .. })
            } else {
                expr_closure_use_ok(value, name)
            }
        }
        // A reassignment of the closure local is a mutable-closure rebind (deferred);
        // any other assignment must use `name` only as a call callee in its value/
        // path indices.
        BytecodeInstruction::Assign {
            name: target,
            path,
            value,
            ..
        } => {
            target != name
                && path.iter().all(|p| match p {
                    BytecodePlace::Index(i) => expr_closure_use_ok(i, name),
                    BytecodePlace::Field(_) => true,
                })
                && expr_closure_use_ok(value, name)
        }
        BytecodeInstruction::Return(Some(e))
        | BytecodeInstruction::Expr(e)
        | BytecodeInstruction::Throw { value: e, .. } => expr_closure_use_ok(e, name),
        BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_)
        | BytecodeInstruction::Asm { .. } => true,
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().all(|b| {
                expr_closure_use_ok(&b.condition, name) && body_closure_use_ok(&b.body, name)
            }) && body_closure_use_ok(else_body, name)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => expr_closure_use_ok(condition, name) && body_closure_use_ok(body, name),
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_closure_use_ok(start, name)
                && expr_closure_use_ok(end, name)
                && step.as_ref().is_none_or(|s| expr_closure_use_ok(s, name))
                && body_closure_use_ok(body, name)
        }
        BytecodeInstruction::Loop { body, .. } => body_closure_use_ok(body, name),
        BytecodeInstruction::Try {
            body, catch_body, ..
        } => body_closure_use_ok(body, name) && body_closure_use_ok(catch_body, name),
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => {
            expr_closure_use_ok(scrutinee, name)
                && arms.iter().all(|arm| body_closure_use_ok(&arm.body, name))
        }
    }
}

/// Whether every occurrence of the closure local `name` inside `expr` is the
/// callee of a direct call `name(args)`. A bare `Variable(name)` anywhere else is
/// an escaping/value use and is rejected.
fn expr_closure_use_ok(expr: &BytecodeExpr, name: &str) -> bool {
    match &expr.kind {
        // A bare read of the closure local (as a value) is an escape unless it is
        // the callee position handled by the `Call` arm below.
        BytecodeExprKind::Variable(n) => n != name,
        BytecodeExprKind::Call { args, .. } => {
            // `name(args)` — the closure local as a direct callee is allowed; the
            // callee name itself is not an argument, so it never leaks as a value.
            // Its arguments must not leak `name` (that would catch `f(f)`).
            args.iter().all(|a| expr_closure_use_ok(a, name))
        }
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Closure { .. } => true,
        BytecodeExprKind::Array(elems) => elems.iter().all(|e| expr_closure_use_ok(e, name)),
        BytecodeExprKind::Unary { expr, .. } | BytecodeExprKind::Await { expr } => {
            expr_closure_use_ok(expr, name)
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            expr_closure_use_ok(left, name) && expr_closure_use_ok(right, name)
        }
        BytecodeExprKind::Field { target, .. } => expr_closure_use_ok(target, name),
        BytecodeExprKind::Index { target, index } => {
            expr_closure_use_ok(target, name) && expr_closure_use_ok(index, name)
        }
    }
}

/// Collect every closure `id` referenced by a `Closure { id }` literal anywhere in
/// a function body, in deterministic order. Used after the main lowering fixpoint
/// to synthesize exactly the closure bodies the compiled functions reference.
pub(crate) fn referenced_closure_ids(function: &BytecodeFunction) -> Vec<usize> {
    let mut ids = Vec::new();
    fn visit_expr(expr: &BytecodeExpr, ids: &mut Vec<usize>) {
        if let BytecodeExprKind::Closure { id } = &expr.kind
            && !ids.contains(id)
        {
            ids.push(*id);
        }
        for child in expr_children(expr) {
            visit_expr(child, ids);
        }
    }
    fn visit_body(body: &[BytecodeInstruction], ids: &mut Vec<usize>) {
        for stmt in body {
            match stmt {
                BytecodeInstruction::Let { value, .. }
                | BytecodeInstruction::Assign { value, .. }
                | BytecodeInstruction::Return(Some(value))
                | BytecodeInstruction::Expr(value)
                | BytecodeInstruction::Throw { value, .. } => visit_expr(value, ids),
                BytecodeInstruction::If {
                    branches,
                    else_body,
                    ..
                } => {
                    for b in branches {
                        visit_expr(&b.condition, ids);
                        visit_body(&b.body, ids);
                    }
                    visit_body(else_body, ids);
                }
                BytecodeInstruction::While {
                    condition, body, ..
                } => {
                    visit_expr(condition, ids);
                    visit_body(body, ids);
                }
                BytecodeInstruction::For {
                    start,
                    end,
                    step,
                    body,
                    ..
                } => {
                    visit_expr(start, ids);
                    visit_expr(end, ids);
                    if let Some(s) = step {
                        visit_expr(s, ids);
                    }
                    visit_body(body, ids);
                }
                BytecodeInstruction::Loop { body, .. } => visit_body(body, ids),
                BytecodeInstruction::Match {
                    scrutinee, arms, ..
                } => {
                    visit_expr(scrutinee, ids);
                    for arm in arms {
                        visit_body(&arm.body, ids);
                    }
                }
                BytecodeInstruction::Try {
                    body, catch_body, ..
                } => {
                    visit_body(body, ids);
                    visit_body(catch_body, ids);
                }
                _ => {}
            }
        }
    }
    visit_body(&function.instructions, &mut ids);
    ids
}

/// Compute the native layout of every Stage-1-supported closure in the module,
/// keyed by parse-order `id`. Each closure literal node carries its full
/// `fn(param types) -> R` static type, so the parameter/return types are read from
/// the literal (the `BytecodeClosureDef` stores only parameter *names* and the
/// body). A closure whose literal type is missing/malformed or whose layout is
/// outside the Stage-1 subset simply gets no entry, so any function binding it
/// skips cleanly.
pub(crate) fn compute_module_closure_layouts(
    module: &BytecodeModule,
) -> HashMap<usize, ClosureLayout> {
    // id -> (param types, return type), read from each `Closure { id }` literal's
    // static function type across every function body.
    let mut types: HashMap<usize, (Vec<TypeRef>, TypeRef)> = HashMap::new();
    for function in &module.functions {
        collect_closure_types_in_body(&function.instructions, &mut types);
    }
    let mut layouts = HashMap::new();
    for def in &module.closures {
        let Some(signature) = types.get(&def.id) else {
            continue;
        };
        if let Some(layout) = compute_closure_layout(def, signature) {
            layouts.insert(def.id, layout);
        }
    }
    layouts
}

fn collect_closure_types_in_body(
    body: &[BytecodeInstruction],
    out: &mut HashMap<usize, (Vec<TypeRef>, TypeRef)>,
) {
    for stmt in body {
        match stmt {
            BytecodeInstruction::Let { value, .. }
            | BytecodeInstruction::Assign { value, .. }
            | BytecodeInstruction::Return(Some(value))
            | BytecodeInstruction::Expr(value)
            | BytecodeInstruction::Throw { value, .. } => collect_closure_types_in_expr(value, out),
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for b in branches {
                    collect_closure_types_in_expr(&b.condition, out);
                    collect_closure_types_in_body(&b.body, out);
                }
                collect_closure_types_in_body(else_body, out);
            }
            BytecodeInstruction::While {
                condition, body, ..
            } => {
                collect_closure_types_in_expr(condition, out);
                collect_closure_types_in_body(body, out);
            }
            BytecodeInstruction::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                collect_closure_types_in_expr(start, out);
                collect_closure_types_in_expr(end, out);
                if let Some(s) = step {
                    collect_closure_types_in_expr(s, out);
                }
                collect_closure_types_in_body(body, out);
            }
            BytecodeInstruction::Loop { body, .. } => collect_closure_types_in_body(body, out),
            BytecodeInstruction::Match {
                scrutinee, arms, ..
            } => {
                collect_closure_types_in_expr(scrutinee, out);
                for arm in arms {
                    collect_closure_types_in_body(&arm.body, out);
                }
            }
            BytecodeInstruction::Try {
                body, catch_body, ..
            } => {
                collect_closure_types_in_body(body, out);
                collect_closure_types_in_body(catch_body, out);
            }
            _ => {}
        }
    }
}

fn collect_closure_types_in_expr(
    expr: &BytecodeExpr,
    out: &mut HashMap<usize, (Vec<TypeRef>, TypeRef)>,
) {
    if let BytecodeExprKind::Closure { id } = &expr.kind
        && let Some(signature) = expr.ty.function_signature()
    {
        out.entry(*id).or_insert(signature);
    }
    for child in expr_children(expr) {
        collect_closure_types_in_expr(child, out);
    }
}

// -- Closure literal + call lowering (inside an enclosing function) -----------

/// Lower a closure literal `Closure { id }` in binding position: allocate the
/// `[code_ptr][captures…]` heap block, store the code pointer (a
/// `lea rax,[rip+__closure_{id}]` REL32 relocation) and each captured scalar, and
/// leave the block pointer in `rax`. The captures are read from the enclosing
/// function's frame locals of the same name (which must be integer-cell scalars).
pub(crate) fn lower_closure_literal(
    ctx: &mut NativeCtx,
    id: usize,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let layout = ctx
        .closure_layouts
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("closure #{id} is not in the native Stage-1 subset"))?;
    let word_count = 1 + layout.captures.len();

    // Allocate the block: `mov rcx, word_count*8 ; call __lullaby_alloc` → rax = block.
    let byte_size = (word_count as i64) * 8;
    emit_mov_rax_imm(code, byte_size);
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_call_symbol(ctx, HEAP_ALLOC_SYMBOL, code);

    // Save the block pointer into a scratch slot so it survives the capture loads.
    let saved_scratch = ctx.scratch_next;
    let blk_slot = ctx.alloc_scratch(1);
    store_local(code, blk_slot); // mov [rbp - blk_slot], rax

    // Word 0 = code pointer: `lea rax,[rip+__closure_{id}]` (REL32), then store.
    code.extend_from_slice(&[0x48, 0x8D, 0x05]); // lea rax, [rip + disp32]
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: site as u32,
        symbol: closure_symbol(id),
    });
    emit_mov_rcx_from_slot(code, blk_slot); // mov rcx, [rbp - blk_slot] (block base)
    code.extend_from_slice(&[0x48, 0x89, 0x01]); // mov [rcx], rax  (word 0 = code ptr)

    // Words 1.. = captured scalars, in capture order.
    for (index, (cap_name, _)) in layout.captures.iter().enumerate() {
        let cap_slot = ctx.local_slot(cap_name).map_err(|_| {
            format!("closure #{id} captures `{cap_name}`, which is not a native local")
        })?;
        // A closure-using function is excluded from register promotion, so the
        // capture always lives in its frame slot. Guard against a stray promotion.
        if ctx.promoted_reg(cap_slot).is_some() {
            ctx.scratch_next = saved_scratch;
            return Err(format!(
                "closure #{id} captures promoted local `{cap_name}`; unsupported"
            ));
        }
        load_local(code, cap_slot); // mov rax, [rbp - cap_slot] (captured value)
        emit_mov_rcx_from_slot(code, blk_slot); // mov rcx, [rbp - blk_slot]
        let offset = ((index + 1) * 8) as i32;
        // mov [rcx + offset], rax
        code.extend_from_slice(&[0x48, 0x89, 0x81]);
        code.extend_from_slice(&offset.to_le_bytes());
    }

    // Result: the block pointer.
    load_local(code, blk_slot); // mov rax, [rbp - blk_slot]
    ctx.scratch_next = saved_scratch;
    Ok(())
}

/// Lower a direct call `name(args)` where `name` is a closure-bound local: load the
/// env pointer (the block base), put it in `rcx`, the arguments (integer-cell
/// scalars) in `rdx`/`r8`/`r9`, load word 0 (the code pointer) into `rax`, and
/// issue an indirect `call rax`. The result (an integer-cell scalar) is left in
/// `rax`.
pub(crate) fn lower_closure_call(
    ctx: &mut NativeCtx,
    name: &str,
    args: &[BytecodeExpr],
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let id = *ctx
        .closure_locals
        .get(name)
        .ok_or_else(|| format!("`{name}` is not a closure local"))?;
    let layout = ctx
        .closure_layouts
        .get(&id)
        .ok_or_else(|| format!("closure #{id} has no native layout"))?;
    if args.len() != layout.param_count {
        return Err(format!(
            "closure `{name}` expects {} argument(s) but got {}",
            layout.param_count,
            args.len()
        ));
    }
    if args.len() > 3 {
        return Err("native closures accept at most three arguments".to_string());
    }

    // Stage the env pointer and every argument onto the machine stack (mirroring the
    // binary-operand and named-call staging elsewhere in the backend), then pop each
    // into its Win64 register just before the indirect call. This keeps a later
    // argument's evaluation from clobbering an already-placed register.
    let slot = ctx.local_slot(name)?; // env pointer lives in the closure local's slot
    load_local(code, slot); // rax = env pointer (block base)
    code.push(0x50); // push rax (env)
    for arg in args {
        lower_native_expr(ctx, arg, code)?; // rax = arg value
        code.push(0x50); // push rax
    }
    // Argument registers, in Win64 order after the env pointer consumes `rcx`.
    // pop into rdx / r8 / r9 for args 0 / 1 / 2. Pop in reverse (top of stack is the
    // last argument).
    const ARG_POP: [&[u8]; 3] = [
        &[0x5A],       // pop rdx (arg 0)
        &[0x41, 0x58], // pop r8  (arg 1)
        &[0x41, 0x59], // pop r9  (arg 2)
    ];
    for index in (0..args.len()).rev() {
        code.extend_from_slice(ARG_POP[index]);
    }
    code.push(0x59); // pop rcx (env pointer)
    // rax = [rcx] (code pointer at word 0), then `call rax`.
    code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
    code.extend_from_slice(&[0xFF, 0xD0]); // call rax
    Ok(())
}

// -- Closure body synthesis ---------------------------------------------------

/// Synthesize the native `.text` body of a closure definition: a function
/// `__closure_{id}` receiving the env pointer in `rcx` and its parameters in
/// `rdx`/`r8`/`r9`, seating them into frame slots, resolving each captured name to
/// `[env + 8*(k+1)]`, and returning the single-expression body's scalar value in
/// `rax`. The body must be heap-free and free of user/`extern` calls (so the
/// closure allocates nothing and retains no pointer — keeping the enclosing arena
/// reasoning a true leaf); a violation returns `Err`, demoting the enclosing
/// function.
#[allow(clippy::too_many_arguments)]
pub(crate) fn synthesize_closure_body(
    def: &BytecodeClosureDef,
    layout: &ClosureLayout,
    callable: &std::collections::HashSet<&str>,
    extern_sigs: &HashMap<&str, &crate::IrExternSignature>,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    strings: &mut StringPool,
    signatures: &HashMap<String, NativeSignature>,
    closure_layouts: &HashMap<usize, ClosureLayout>,
) -> Result<LoweredNativeFunction, String> {
    // Default-deny body shape: heap-free (no allocation, so nothing to reclaim
    // inside the closure) and free of user/`extern` calls (a leaf w.r.t. user code,
    // so the enclosing arena function stays a true leaf). Inline scalar builtins are
    // fine (they emit no `call`).
    let empty_aggs = std::collections::HashSet::new();
    if expr_touches_heap(&def.body, &empty_aggs) {
        return Err(format!(
            "closure #{} body touches the heap; native closures are scalar-only in Stage 1",
            def.id
        ));
    }
    let user_names: std::collections::HashSet<&str> = callable.iter().copied().collect();
    if expr_calls_user(&def.body, &user_names) {
        return Err(format!(
            "closure #{} body calls a user/extern function; deferred in Stage 1",
            def.id
        ));
    }

    // -- Plan the frame: env pointer slot + parameter slots + a small scratch. ----
    let mut locals: HashMap<String, NativeLocal> = HashMap::new();
    let mut next_slot: i32 = 0;
    // Env pointer (word 0 = code ptr; captures at 8*(k+1)).
    next_slot += 8;
    let env_slot = next_slot;
    // Parameters (integer-cell scalars), one word each, in order.
    for pname in &layout.param_names {
        next_slot += 8;
        locals.insert(
            pname.clone(),
            NativeLocal {
                slot: next_slot,
                ty: NativeType::I64,
            },
        );
    }
    // A small scratch region for any inline builtin that spills, plus the guard
    // word the scratch cursor reserves. The body has no aggregate/`match` scratch
    // needs (scalar-only), so a few words are ample.
    let scratch_base = next_slot;
    next_slot += (4 + 1) * 8;
    // 32 bytes of shadow space so `rsp` stays 16-byte aligned across any inline
    // builtin's internal stack use; the body issues no real `call`, but reserving
    // the shadow is harmless and keeps alignment robust.
    let raw = next_slot + 32;
    let frame_size = ((raw + 15) / 16) * 16;

    // Env capture offsets: capture `k` at byte offset `8*(k+1)` in the env block.
    let mut captures = HashMap::new();
    for (index, (cap_name, _)) in layout.captures.iter().enumerate() {
        captures.insert(cap_name.clone(), ((index + 1) * 8) as i32);
    }

    let mut ctx = NativeCtx::for_closure_body(
        locals,
        frame_size,
        scratch_base,
        ClosureEnv { env_slot, captures },
        callable,
        extern_sigs,
        structs,
        enums,
        strings,
        signatures,
        closure_layouts,
    );

    let mut code = Vec::new();
    // Prologue: push rbp; mov rbp, rsp; sub rsp, frame.
    code.extend_from_slice(&[0x55, 0x48, 0x89, 0xE5]);
    emit_sub_rsp(&mut code, frame_size);
    // Seat the env pointer (rcx) into its slot: `mov [rbp - env_slot], rcx`.
    code.extend_from_slice(&[0x48, 0x89, 0x8D]);
    code.extend_from_slice(&(-env_slot).to_le_bytes());
    // Seat parameters from rdx / r8 / r9 (Win64 argument registers 1..3).
    const PARAM_STORE: [&[u8]; 3] = [
        &[0x48, 0x89, 0x95], // mov [rbp+disp32], rdx (param 0)
        &[0x4C, 0x89, 0x85], // mov [rbp+disp32], r8  (param 1)
        &[0x4C, 0x89, 0x8D], // mov [rbp+disp32], r9  (param 2)
    ];
    for (index, pname) in layout.param_names.iter().enumerate() {
        let slot = ctx.local_slot(pname)?;
        code.extend_from_slice(PARAM_STORE[index]);
        code.extend_from_slice(&(-slot).to_le_bytes());
    }

    // Body: a single integer-cell scalar expression, evaluated into `rax`.
    lower_native_expr(&mut ctx, &def.body, &mut code)?;

    // Epilogue: add rsp, frame; pop rbp; ret.
    emit_native_epilogue(&mut code, frame_size, &[]);

    Ok(LoweredNativeFunction::new_closure(
        closure_symbol(def.id),
        code,
        ctx.take_relocations(),
    ))
}
