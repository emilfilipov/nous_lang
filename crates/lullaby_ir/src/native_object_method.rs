//! Native inherent-method dispatch: expand receiver-dispatched method calls into
//! ordinary direct calls to monomorphized method-instance functions.
//!
//! # Representation the native backend consumes
//!
//! A source method call `recv.method(args)` is desugared **at parse time** (UFCS)
//! into an ordinary `Call { name: "method", args: [recv, args...] }` node — there
//! is no distinct method-call IR node. The receiver is simply `args[0]`. The
//! method bodies themselves live in [`BytecodeModule::impls`] (keyed by
//! `(base_type_name, method_name)`), **not** in [`BytecodeModule::functions`], and
//! the set of names that are receiver-dispatched (trait methods plus inherent
//! `impl Type<T>` methods) is [`BytecodeModule::trait_methods`]. The interpreters
//! dispatch such a call by the receiver's *runtime* type; the native backend
//! resolves it statically by the receiver's *static* type.
//!
//! # What this pass does
//!
//! [`expand_method_instances`] returns a NEW module (used only for native
//! emission — the interpreters keep the original) in which:
//!
//! - every method-call site whose receiver resolves to a concrete struct/enum type
//!   (including a monomorphized generic instantiation like `Box<i64>`) that has a
//!   matching `impl` method is rewritten from the bare method name to a unique,
//!   collision-free **mangled** instance name (`$mth$Box<i64>$peek` sanitized); and
//! - for each such instance, a synthesized [`BytecodeFunction`] under that mangled
//!   name is appended to `functions`, with the method body monomorphized — every
//!   `TypeRef` (params, return, and every embedded expression/`let` type) has the
//!   receiver's type arguments substituted (`{T: i64}`), reusing the same
//!   `substitute_type` the generic-type layout path uses.
//!
//! The synthesized function's `self` parameter is an ordinary aggregate parameter
//! (`self: Box<i64>`), so it flows through the **existing** aggregate-argument ABI
//! (by hidden pointer / copy-in value semantics) unchanged — no new ABI. Because a
//! method is thus just a function whose first argument is a copied aggregate, the
//! interpreters' by-value `self` (each call clones the receiver `Value`) is matched
//! bit-for-bit: mutating `self` inside a method cannot affect the caller.
//!
//! # Default-deny
//!
//! A call is rewritten to a direct instance call ONLY when the receiver type is a
//! concrete user struct/enum with a resolvable impl. Anything else — a bare type
//! parameter `T`, a trait-object/dynamic receiver, a builtin whose name happens to
//! collide, or a receiver whose monomorphized layout is outside the native subset —
//! is left untouched. An untouched name that is not a compiled/builtin function, or
//! a synthesized instance that later fails native eligibility/lowering, demotes its
//! caller through the existing native fixpoint, so the whole program skips cleanly
//! to the interpreters (`L0339`) rather than miscompiling.

use super::*;

use std::collections::{HashMap, HashSet};

use lullaby_semantics::substitute_type;

/// A method instance discovered at a call site and queued for synthesis: the
/// unique mangled symbol name, the (borrowed) generic impl-method body to
/// monomorphize, and the `{type_param -> concrete}` substitution for this
/// receiver instantiation (empty for a non-generic receiver).
struct MethodInstance<'a> {
    mangled: String,
    impl_fn: &'a BytecodeFunction,
    subst: HashMap<String, TypeRef>,
}

struct MethodExpander<'a> {
    /// `(base_type_name, method_name)` -> the generic impl-method body.
    impls: HashMap<(String, String), &'a BytecodeFunction>,
    /// Names that are receiver-dispatched (trait + inherent method names).
    method_names: HashSet<&'a str>,
    structs: &'a [IrStructDef],
    enums: &'a [IrEnumDef],
    /// Mangled instance names already queued/synthesized, for dedup.
    seen: HashSet<String>,
    /// Instances still to synthesize.
    worklist: Vec<MethodInstance<'a>>,
}

/// Expand every native-resolvable inherent/impl method call in `module` into a
/// direct call to a synthesized, monomorphized instance function. Returns a new
/// module for native emission; the input is left untouched. When the module
/// declares no receiver-dispatched methods this is a structural clone whose
/// function bodies are byte-identical to the input (identity substitution, no name
/// rewrites), so the existing COFF snapshots are unaffected.
pub(crate) fn expand_method_instances(module: &BytecodeModule) -> BytecodeModule {
    let mut expander = MethodExpander::new(module);

    // Transform every top-level function body (identity substitution). Method
    // calls are rewritten to mangled names and their instances queued.
    let mut functions: Vec<BytecodeFunction> = Vec::with_capacity(module.functions.len());
    let empty: HashMap<String, TypeRef> = HashMap::new();
    for function in &module.functions {
        functions.push(BytecodeFunction {
            name: function.name.clone(),
            params: function.params.clone(),
            return_type: function.return_type.clone(),
            instructions: expander.transform_instructions(&function.instructions, &empty),
            span: function.span,
        });
    }

    // Drain the worklist, synthesizing each queued instance (which may queue more
    // via chained method calls). An instance whose generic body contains a closure
    // is skipped (its shared closure body cannot be monomorphized per
    // instantiation); the caller then skips through the native fixpoint.
    while let Some(instance) = expander.worklist.pop() {
        if let Some(function) = expander.synthesize(&instance) {
            functions.push(function);
        }
    }

    BytecodeModule {
        functions,
        structs: module.structs.clone(),
        enums: module.enums.clone(),
        impls: module.impls.clone(),
        trait_methods: module.trait_methods.clone(),
        async_functions: module.async_functions.clone(),
        extern_functions: module.extern_functions.clone(),
        extern_signatures: module.extern_signatures.clone(),
        export_functions: module.export_functions.clone(),
        closures: module.closures.clone(),
    }
}

impl<'a> MethodExpander<'a> {
    fn new(module: &'a BytecodeModule) -> Self {
        let mut impls = HashMap::new();
        for method in &module.impls {
            impls.insert(
                (method.type_name.clone(), method.method_name.clone()),
                &method.function,
            );
        }
        let method_names = module.trait_methods.iter().map(String::as_str).collect();
        Self {
            impls,
            method_names,
            structs: &module.structs,
            enums: &module.enums,
            seen: HashSet::new(),
            worklist: Vec::new(),
        }
    }

    /// Synthesize a monomorphized method-instance function, or `None` when the
    /// instance is out of scope for a per-instantiation body (a generic method
    /// whose body contains a closure — the closure body is shared by id across
    /// instantiations and cannot be safely monomorphized). Returning `None` leaves
    /// the caller's rewritten call dangling, so the caller skips cleanly.
    fn synthesize(&mut self, instance: &MethodInstance<'a>) -> Option<BytecodeFunction> {
        let impl_fn = instance.impl_fn;
        let subst = &instance.subst;
        if !subst.is_empty() && instructions_contain_closure(&impl_fn.instructions) {
            return None;
        }
        let params = impl_fn
            .params
            .iter()
            .map(|param| crate::IrParam {
                name: param.name.clone(),
                ty: substitute_type(&param.ty, subst),
            })
            .collect();
        let return_type = substitute_type(&impl_fn.return_type, subst);
        let instructions = self.transform_instructions(&impl_fn.instructions, subst);
        Some(BytecodeFunction {
            name: instance.mangled.clone(),
            params,
            return_type,
            instructions,
            span: impl_fn.span,
        })
    }

    /// Resolve a `Call` to a method instance when its (already type-substituted)
    /// receiver is a concrete user struct/enum with a matching impl. Returns `None`
    /// for a non-method name, a non-concrete/non-user receiver, or a receiver with
    /// no matching impl — in every such case the call is left unchanged.
    fn resolve(&self, name: &str, receiver: &TypeRef) -> Option<MethodInstance<'a>> {
        if !self.method_names.contains(name) {
            return None;
        }
        let (base, type_args) = self.split_receiver(receiver)?;
        let impl_fn = self.impls.get(&(base.clone(), name.to_string())).copied()?;
        let type_params = self.type_params_of(&base);
        // A non-generic base (`type_params` empty, no type args) needs no
        // substitution. Any arity mismatch (which a checked program never
        // produces) is treated as unresolvable so the call is left untouched.
        if type_params.len() != type_args.len() {
            return None;
        }
        let subst: HashMap<String, TypeRef> = type_params
            .iter()
            .cloned()
            .zip(type_args)
            .collect();
        Some(MethodInstance {
            mangled: mangle_method(&receiver.name, name),
            impl_fn,
            subst,
        })
    }

    /// Split a concrete receiver type into `(base_user_type, type_args)`. Returns
    /// `None` unless the head names a declared user struct/enum: a scalar,
    /// `string`, a builtin generic (`list<..>`), a bare type parameter, or a
    /// function type never resolves to a user impl and is left untouched.
    fn split_receiver(&self, ty: &TypeRef) -> Option<(String, Vec<TypeRef>)> {
        if let Some(open) = ty.name.find('<') {
            if !ty.name.ends_with('>') || ty.name.starts_with("fn(") {
                return None;
            }
            let head = ty.name[..open].to_string();
            if !self.is_user_type(&head) {
                return None;
            }
            let args = ty.generic_args(&head)?;
            Some((head, args))
        } else if self.is_user_type(&ty.name) {
            Some((ty.name.clone(), Vec::new()))
        } else {
            None
        }
    }

    fn is_user_type(&self, name: &str) -> bool {
        self.structs.iter().any(|s| s.name == name) || self.enums.iter().any(|e| e.name == name)
    }

    fn type_params_of(&self, base: &str) -> Vec<String> {
        if let Some(s) = self.structs.iter().find(|s| s.name == base) {
            return s.type_params.clone();
        }
        if let Some(e) = self.enums.iter().find(|e| e.name == base) {
            return e.type_params.clone();
        }
        Vec::new()
    }

    fn enqueue(&mut self, instance: MethodInstance<'a>) {
        if self.seen.insert(instance.mangled.clone()) {
            self.worklist.push(instance);
        }
    }

    // -- Body transformation (type substitution + method-call rewriting) ------

    fn transform_instructions(
        &mut self,
        instructions: &[BytecodeInstruction],
        subst: &HashMap<String, TypeRef>,
    ) -> Vec<BytecodeInstruction> {
        instructions
            .iter()
            .map(|instruction| self.transform_instruction(instruction, subst))
            .collect()
    }

    fn transform_instruction(
        &mut self,
        instruction: &BytecodeInstruction,
        subst: &HashMap<String, TypeRef>,
    ) -> BytecodeInstruction {
        match instruction {
            BytecodeInstruction::Let {
                name,
                ty,
                value,
                span,
            } => BytecodeInstruction::Let {
                name: name.clone(),
                ty: substitute_type(ty, subst),
                value: self.transform_expr(value, subst),
                span: *span,
            },
            BytecodeInstruction::Assign {
                name,
                path,
                op,
                value,
                span,
            } => BytecodeInstruction::Assign {
                name: name.clone(),
                path: path
                    .iter()
                    .map(|place| self.transform_place(place, subst))
                    .collect(),
                op: *op,
                value: self.transform_expr(value, subst),
                span: *span,
            },
            BytecodeInstruction::Return(value) => {
                BytecodeInstruction::Return(value.as_ref().map(|v| self.transform_expr(v, subst)))
            }
            BytecodeInstruction::Break(span) => BytecodeInstruction::Break(*span),
            BytecodeInstruction::Continue(span) => BytecodeInstruction::Continue(*span),
            BytecodeInstruction::Expr(value) => {
                BytecodeInstruction::Expr(self.transform_expr(value, subst))
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                span,
            } => BytecodeInstruction::If {
                branches: branches
                    .iter()
                    .map(|branch| BytecodeIfBranch {
                        condition: self.transform_expr(&branch.condition, subst),
                        body: self.transform_instructions(&branch.body, subst),
                    })
                    .collect(),
                else_body: self.transform_instructions(else_body, subst),
                span: *span,
            },
            BytecodeInstruction::While {
                condition,
                body,
                span,
            } => BytecodeInstruction::While {
                condition: self.transform_expr(condition, subst),
                body: self.transform_instructions(body, subst),
                span: *span,
            },
            BytecodeInstruction::For {
                name,
                start,
                end,
                step,
                body,
                span,
            } => BytecodeInstruction::For {
                name: name.clone(),
                start: self.transform_expr(start, subst),
                end: self.transform_expr(end, subst),
                step: step.as_ref().map(|s| self.transform_expr(s, subst)),
                body: self.transform_instructions(body, subst),
                span: *span,
            },
            BytecodeInstruction::Loop { body, span } => BytecodeInstruction::Loop {
                body: self.transform_instructions(body, subst),
                span: *span,
            },
            BytecodeInstruction::Asm { bytes, span } => BytecodeInstruction::Asm {
                bytes: bytes.clone(),
                span: *span,
            },
            BytecodeInstruction::Throw { value, span } => BytecodeInstruction::Throw {
                value: self.transform_expr(value, subst),
                span: *span,
            },
            BytecodeInstruction::Try {
                body,
                catch_name,
                catch_body,
                span,
            } => BytecodeInstruction::Try {
                body: self.transform_instructions(body, subst),
                catch_name: catch_name.clone(),
                catch_body: self.transform_instructions(catch_body, subst),
                span: *span,
            },
            BytecodeInstruction::Match {
                scrutinee,
                arms,
                span,
            } => BytecodeInstruction::Match {
                scrutinee: self.transform_expr(scrutinee, subst),
                arms: arms
                    .iter()
                    .map(|arm| BytecodeMatchArm {
                        pattern: arm.pattern.clone(),
                        body: self.transform_instructions(&arm.body, subst),
                    })
                    .collect(),
                span: *span,
            },
        }
    }

    fn transform_place(
        &mut self,
        place: &BytecodePlace,
        subst: &HashMap<String, TypeRef>,
    ) -> BytecodePlace {
        match place {
            BytecodePlace::Field(name) => BytecodePlace::Field(name.clone()),
            BytecodePlace::Index(index) => BytecodePlace::Index(self.transform_expr(index, subst)),
        }
    }

    fn transform_expr(
        &mut self,
        expr: &BytecodeExpr,
        subst: &HashMap<String, TypeRef>,
    ) -> BytecodeExpr {
        let ty = substitute_type(&expr.ty, subst);
        let kind = match &expr.kind {
            BytecodeExprKind::Integer(value) => BytecodeExprKind::Integer(*value),
            BytecodeExprKind::Float(value) => BytecodeExprKind::Float(*value),
            BytecodeExprKind::Bool(value) => BytecodeExprKind::Bool(*value),
            BytecodeExprKind::String(value) => BytecodeExprKind::String(value.clone()),
            BytecodeExprKind::Char(value) => BytecodeExprKind::Char(*value),
            BytecodeExprKind::Variable(name) => BytecodeExprKind::Variable(name.clone()),
            BytecodeExprKind::Closure { id } => BytecodeExprKind::Closure { id: *id },
            BytecodeExprKind::Array(items) => BytecodeExprKind::Array(
                items
                    .iter()
                    .map(|item| self.transform_expr(item, subst))
                    .collect(),
            ),
            BytecodeExprKind::Index { target, index } => BytecodeExprKind::Index {
                target: Box::new(self.transform_expr(target, subst)),
                index: Box::new(self.transform_expr(index, subst)),
            },
            BytecodeExprKind::Unary { op, expr: inner } => BytecodeExprKind::Unary {
                op: *op,
                expr: Box::new(self.transform_expr(inner, subst)),
            },
            BytecodeExprKind::Binary { left, op, right } => BytecodeExprKind::Binary {
                left: Box::new(self.transform_expr(left, subst)),
                op: *op,
                right: Box::new(self.transform_expr(right, subst)),
            },
            BytecodeExprKind::Field { target, field } => BytecodeExprKind::Field {
                target: Box::new(self.transform_expr(target, subst)),
                field: field.clone(),
            },
            BytecodeExprKind::Await { expr: inner } => BytecodeExprKind::Await {
                expr: Box::new(self.transform_expr(inner, subst)),
            },
            BytecodeExprKind::Call { name, args } => {
                let args: Vec<BytecodeExpr> = args
                    .iter()
                    .map(|arg| self.transform_expr(arg, subst))
                    .collect();
                // The receiver's type is now concrete (args were substituted
                // first), so method resolution keys on the instantiation.
                let rewritten = args
                    .first()
                    .and_then(|receiver| self.resolve(name, &receiver.ty));
                match rewritten {
                    Some(instance) => {
                        let mangled = instance.mangled.clone();
                        self.enqueue(instance);
                        BytecodeExprKind::Call {
                            name: mangled,
                            args,
                        }
                    }
                    None => BytecodeExprKind::Call {
                        name: name.clone(),
                        args,
                    },
                }
            }
        };
        BytecodeExpr {
            kind,
            ty,
            span: expr.span,
        }
    }
}

/// A collision-free, deterministic symbol name for a method instance, derived from
/// the receiver's concrete type spelling and the method name. The leading `$`
/// (never valid in a source identifier: `[A-Za-z_][A-Za-z0-9_]*`) guarantees no
/// collision with a user function name; non-identifier characters in the type
/// spelling (`<`, `>`, `,`, spaces) are sanitized to `_` so the symbol is a plain
/// name the object writers accept, while distinct instantiations stay distinct
/// (the trailing marker from `>` keeps `Box<i64>` apart from a literal `Box_i64`).
fn mangle_method(receiver_type: &str, method: &str) -> String {
    let sanitized: String = receiver_type
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("$mth${sanitized}${method}")
}

/// Whether any instruction in the body constructs a closure literal. A generic
/// method whose body does is skipped by [`MethodExpander::synthesize`] because the
/// closure body is keyed by id in [`BytecodeModule::closures`] and shared across
/// all instantiations, so it cannot carry a per-instantiation monomorphization.
fn instructions_contain_closure(instructions: &[BytecodeInstruction]) -> bool {
    instructions.iter().any(instruction_contains_closure)
}

fn instruction_contains_closure(instruction: &BytecodeInstruction) -> bool {
    match instruction {
        BytecodeInstruction::Let { value, .. }
        | BytecodeInstruction::Expr(value)
        | BytecodeInstruction::Throw { value, .. } => expr_contains_closure(value),
        BytecodeInstruction::Assign { path, value, .. } => {
            expr_contains_closure(value)
                || path.iter().any(|place| match place {
                    BytecodePlace::Field(_) => false,
                    BytecodePlace::Index(index) => expr_contains_closure(index),
                })
        }
        BytecodeInstruction::Return(value) => value.as_ref().is_some_and(expr_contains_closure),
        BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_)
        | BytecodeInstruction::Asm { .. } => false,
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().any(|branch| {
                expr_contains_closure(&branch.condition)
                    || instructions_contain_closure(&branch.body)
            }) || instructions_contain_closure(else_body)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => expr_contains_closure(condition) || instructions_contain_closure(body),
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_contains_closure(start)
                || expr_contains_closure(end)
                || step.as_ref().is_some_and(expr_contains_closure)
                || instructions_contain_closure(body)
        }
        BytecodeInstruction::Loop { body, .. } => instructions_contain_closure(body),
        BytecodeInstruction::Try {
            body, catch_body, ..
        } => instructions_contain_closure(body) || instructions_contain_closure(catch_body),
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => {
            expr_contains_closure(scrutinee)
                || arms
                    .iter()
                    .any(|arm| instructions_contain_closure(&arm.body))
        }
    }
}

fn expr_contains_closure(expr: &BytecodeExpr) -> bool {
    match &expr.kind {
        BytecodeExprKind::Closure { .. } => true,
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Variable(_) => false,
        BytecodeExprKind::Array(items) => items.iter().any(expr_contains_closure),
        BytecodeExprKind::Index { target, index } => {
            expr_contains_closure(target) || expr_contains_closure(index)
        }
        BytecodeExprKind::Unary { expr: inner, .. } | BytecodeExprKind::Await { expr: inner } => {
            expr_contains_closure(inner)
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            expr_contains_closure(left) || expr_contains_closure(right)
        }
        BytecodeExprKind::Field { target, .. } => expr_contains_closure(target),
        BytecodeExprKind::Call { args, .. } => args.iter().any(expr_contains_closure),
    }
}

#[cfg(test)]
mod tests {
    use super::super::emit_native_program;
    use crate::{BytecodeModule, lower, lower_to_bytecode};
    use lullaby_lexer::lex;
    use lullaby_parser::parse;
    use lullaby_semantics::validate_executable;

    /// Compile source through the full frontend into a `BytecodeModule`.
    fn module_for(source: &str) -> BytecodeModule {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let checked = validate_executable(&program).expect("semantic");
        let ir = lower(&checked).expect("lower");
        lower_to_bytecode(&ir)
    }

    /// Assert that `main` compiles natively and every listed instance symbol is a
    /// compiled `.text` function, with nothing skipped.
    fn assert_compiles(source: &str, instances: &[&str]) {
        let program = emit_native_program(&module_for(source)).expect("program compiles native");
        assert!(
            program.compiled.contains(&"main".to_string()),
            "main must compile: skipped={:?}",
            program.skipped
        );
        for symbol in instances {
            assert!(
                program.compiled.contains(&symbol.to_string()),
                "method instance `{symbol}` must compile; compiled={:?}",
                program.compiled
            );
        }
        assert!(
            program.skipped.is_empty(),
            "nothing must skip: {:?}",
            program.skipped
        );
    }

    /// Assert that `main` does NOT compile natively (it skips to the interpreters),
    /// either via the `L0339` no-eligible gate or by `main` being listed skipped.
    fn assert_main_skips(source: &str) {
        match emit_native_program(&module_for(source)) {
            Err(error) => assert_eq!(error.code, "L0339", "skip must carry L0339: {source}"),
            Ok(program) => assert!(
                !program.compiled.contains(&"main".to_string()),
                "main must NOT compile: {source}\ncompiled={:?}",
                program.compiled
            ),
        }
    }

    /// A non-generic inherent method compiles to a direct call. Value semantics:
    /// `self` is a copied aggregate, so `c.bump(3)` cannot mutate the caller's `c`.
    #[test]
    fn non_generic_method_compiles() {
        assert_compiles(
            concat!(
                "struct Counter\n",
                "    n i64\n",
                "impl Counter\n",
                "    fn bump self amount i64 -> Counter\n",
                "        Counter(self.n + amount)\n",
                "    fn value self -> i64\n",
                "        self.n\n",
                "fn main -> i64\n",
                "    let c Counter = Counter(10)\n",
                "    let d Counter = c.bump(3)\n",
                "    c.value() + d.value() + c.bump(5).value()\n",
            ),
            &["$mth$Counter$bump", "$mth$Counter$value"],
        );
    }

    /// A generic struct method monomorphizes per instantiation: `Box<i64>` and
    /// `Box<bool>` each get their own `peek` instance, plus a `Box<i64>` `rewrap`
    /// that returns a fresh aggregate.
    #[test]
    fn generic_struct_method_monomorphizes() {
        assert_compiles(
            concat!(
                "struct Box<T>\n",
                "    value T\n",
                "impl Box<T>\n",
                "    fn peek self -> T\n",
                "        self.value\n",
                "    fn rewrap self v T -> Box<T>\n",
                "        Box(v)\n",
                "fn main -> i64\n",
                "    let a Box<i64> = Box(5)\n",
                "    let flag Box<bool> = Box(true)\n",
                "    let bumped Box<i64> = a.rewrap(9)\n",
                "    let extra i64 = 100 if flag.peek() else 0\n",
                "    a.peek() + bumped.peek() + extra\n",
            ),
            &[
                "$mth$Box_i64_$peek",
                "$mth$Box_bool_$peek",
                "$mth$Box_i64_$rewrap",
            ],
        );
    }

    /// A generic enum method compiles: `Opt<i64>.unwrap_or` matches `self`, binding
    /// the payload as the concrete `i64`.
    #[test]
    fn generic_enum_method_compiles() {
        assert_compiles(
            concat!(
                "enum Opt<T>\n",
                "    present T\n",
                "    absent\n",
                "impl Opt<T>\n",
                "    fn unwrap_or self fallback T -> T\n",
                "        match self\n",
                "            present(x) -> x\n",
                "            absent -> fallback\n",
                "fn main -> i64\n",
                "    let o Opt<i64> = present(30)\n",
                "    let missing Opt<i64> = absent\n",
                "    o.unwrap_or(0) + missing.unwrap_or(7)\n",
            ),
            &["$mth$Opt_i64_$unwrap_or"],
        );
    }

    /// A one-level heap-field receiver (`Box<string>`) method reads the immutable
    /// string field through the shared pointer word.
    #[test]
    fn heap_field_receiver_method_compiles() {
        assert_compiles(
            concat!(
                "struct Box<T>\n",
                "    value T\n",
                "impl Box<T>\n",
                "    fn peek self -> T\n",
                "        self.value\n",
                "fn main -> i64\n",
                "    let b Box<string> = Box(\"hello\")\n",
                "    len(b.peek())\n",
            ),
            &["$mth$Box_string_$peek"],
        );
    }

    /// A method whose receiver monomorphizes to a deeper-than-one-level heap layout
    /// (a struct with a `list<i64>` field) is out of the native subset, so the
    /// method instance is ineligible and `main` skips cleanly.
    #[test]
    fn deeper_heap_receiver_method_skips() {
        assert_main_skips(concat!(
            "struct Stack\n",
            "    items list<i64>\n",
            "impl Stack\n",
            "    fn size self -> i64\n",
            "        len(self.items)\n",
            "fn main -> i64\n",
            "    let xs list<i64> = list_new()\n",
            "    xs = push(xs, 10)\n",
            "    let s Stack = Stack(xs)\n",
            "    s.size()\n",
        ));
    }

    /// A method with an out-of-subset parameter (a `map<string, i64>`, whose string
    /// keys are deferred) is ineligible, so `main` skips cleanly rather than
    /// miscompiling.
    #[test]
    fn out_of_subset_param_method_skips() {
        assert_main_skips(concat!(
            "struct Wrap\n",
            "    n i64\n",
            "impl Wrap\n",
            "    fn lookup self table map<string, i64> -> i64\n",
            "        self.n\n",
            "fn main -> i64\n",
            "    let t map<string, i64> = map_new()\n",
            "    t = map_set(t, \"x\", 5)\n",
            "    let w Wrap = Wrap(9)\n",
            "    w.lookup(t)\n",
        ));
    }

    /// A trait method called through a bounded generic free function (`describe<T:
    /// Show>` calling `v.show()` on a bare `T`) is dynamic dispatch on a
    /// non-concrete receiver: it is left untouched and the generic function (and
    /// thus `main`) skips cleanly.
    #[test]
    fn trait_dispatch_through_generic_bound_skips() {
        assert_main_skips(concat!(
            "trait Show\n",
            "    fn show self -> string\n",
            "struct Point\n",
            "    x i64\n",
            "    y i64\n",
            "impl Show for Point\n",
            "    fn show self -> string\n",
            "        to_string(self.x)\n",
            "fn describe<T: Show> v T -> string\n",
            "    v.show()\n",
            "fn main -> i64\n",
            "    let p Point = Point(3, 4)\n",
            "    len(describe(p))\n",
        ));
    }
}
