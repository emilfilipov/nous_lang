use std::collections::{HashMap, HashSet};

use lullaby_diagnostics::Span;
use lullaby_parser::{
    AssignOp, BinaryOp, EnumDecl, EnumVariant, Expr, ExprKind, Function, INFERRED_RETURN, IfBranch,
    MatchArm, MatchPattern, MethodSig, Param, Place, Program, RegionDecl, Stmt, StructDecl,
    StructField, TypeRef, UnaryOp, function_type, generic_type,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub function: Option<String>,
    pub span: Option<Span>,
}

impl SemanticDiagnostic {
    fn new(code: &'static str, message: impl Into<String>, function: Option<String>) -> Self {
        Self {
            code,
            message: message.into(),
            function,
            span: None,
        }
    }

    fn at(
        code: &'static str,
        message: impl Into<String>,
        function: Option<String>,
        span: Span,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            function,
            span: Some(span),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckedProgram {
    pub program: Program,
    pub info: SemanticInfo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticInfo {
    pub signatures: HashMap<String, Signature>,
    pub expression_types: Vec<ExpressionType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionType {
    pub function: String,
    pub span: Span,
    pub ty: TypeRef,
}

pub fn validate(program: &Program) -> Result<CheckedProgram, Vec<SemanticDiagnostic>> {
    // Resolve type aliases to their canonical types before any checking, so the
    // rest of the pipeline (and IR/runtime) never sees an alias. Aliases carry
    // no runtime representation, so runtime layout is unchanged.
    let (mut resolved, alias_diagnostics) = resolve_program_aliases(program);

    let mut checker = Checker::new(&resolved);
    checker.diagnostics = alias_diagnostics;
    checker.validate();
    if !checker.diagnostics.is_empty() {
        return Err(std::mem::take(&mut checker.diagnostics));
    }

    let signatures = std::mem::take(&mut checker.signatures);
    let expression_types = std::mem::take(&mut checker.expression_types);
    let resolved_returns = std::mem::take(&mut checker.resolved_returns);
    drop(checker);

    // Write inferred return types back into the program so IR lowering and every
    // downstream backend see a concrete type, never the `INFERRED_RETURN`
    // sentinel. A function with an explicit `-> T` is left untouched.
    for function in &mut resolved.functions {
        if function.return_type.name == INFERRED_RETURN {
            function.return_type = resolved_returns
                .get(&function.name)
                .cloned()
                .unwrap_or_else(|| TypeRef::new("void"));
        }
    }

    Ok(CheckedProgram {
        program: resolved,
        info: SemanticInfo {
            signatures,
            expression_types,
        },
    })
}

/// Resolve all type aliases in a program to canonical types, returning the
/// rewritten program plus any alias-definition diagnostics (duplicate `L0360`,
/// cyclic `L0361`).
fn resolve_program_aliases(program: &Program) -> (Program, Vec<SemanticDiagnostic>) {
    let mut diagnostics = Vec::new();
    let mut map: HashMap<String, TypeRef> = HashMap::new();
    for alias in &program.aliases {
        if map.contains_key(&alias.name) {
            diagnostics.push(SemanticDiagnostic::at(
                "L0360",
                format!("duplicate type alias `{}`", alias.name),
                None,
                alias.span,
            ));
            continue;
        }
        map.insert(alias.name.clone(), alias.target.clone());
    }

    // Detect cyclic alias chains (e.g. `alias A = B` / `alias B = A`).
    for alias in &program.aliases {
        if chain_is_cyclic(&alias.name, &map) {
            diagnostics.push(SemanticDiagnostic::at(
                "L0361",
                format!("type alias `{}` is defined in terms of itself", alias.name),
                None,
                alias.span,
            ));
        }
    }

    let functions = program
        .functions
        .iter()
        .map(|function| Function {
            name: function.name.clone(),
            type_params: function.type_params.clone(),
            params: function
                .params
                .iter()
                .map(|param| Param {
                    name: param.name.clone(),
                    ty: resolve_alias_type(&param.ty, &map),
                })
                .collect(),
            return_type: resolve_alias_type(&function.return_type, &map),
            body: function
                .body
                .iter()
                .map(|stmt| rewrite_stmt_types(stmt, &map))
                .collect(),
            span: function.span,
            is_public: function.is_public,
            is_async: function.is_async,
            is_extern: function.is_extern,
            is_export: function.is_export,
        })
        .collect();

    let structs = program
        .structs
        .iter()
        .map(|declaration| StructDecl {
            name: declaration.name.clone(),
            fields: declaration
                .fields
                .iter()
                .map(|field| StructField {
                    name: field.name.clone(),
                    ty: resolve_alias_type(&field.ty, &map),
                })
                .collect(),
            span: declaration.span,
            is_public: declaration.is_public,
        })
        .collect();

    let enums = program
        .enums
        .iter()
        .map(|declaration| EnumDecl {
            name: declaration.name.clone(),
            variants: declaration
                .variants
                .iter()
                .map(|variant| EnumVariant {
                    name: variant.name.clone(),
                    payload: variant
                        .payload
                        .iter()
                        .map(|ty| resolve_alias_type(ty, &map))
                        .collect(),
                })
                .collect(),
            span: declaration.span,
            is_public: declaration.is_public,
        })
        .collect();

    (
        Program {
            functions,
            aliases: program.aliases.clone(),
            structs,
            enums,
            imports: program.imports.clone(),
            // Trait/impl method types are resolved against aliases below via the
            // same `resolve_alias_type` mapping so the checker never sees an alias.
            traits: program
                .traits
                .iter()
                .map(|decl| lullaby_parser::TraitDecl {
                    name: decl.name.clone(),
                    methods: decl
                        .methods
                        .iter()
                        .map(|method| lullaby_parser::MethodSig {
                            name: method.name.clone(),
                            params: method
                                .params
                                .iter()
                                .map(|param| Param {
                                    name: param.name.clone(),
                                    ty: resolve_alias_type(&param.ty, &map),
                                })
                                .collect(),
                            return_type: resolve_alias_type(&method.return_type, &map),
                            span: method.span,
                        })
                        .collect(),
                    span: decl.span,
                    is_public: decl.is_public,
                })
                .collect(),
            impls: program
                .impls
                .iter()
                .map(|decl| lullaby_parser::ImplDecl {
                    trait_name: decl.trait_name.clone(),
                    type_name: decl.type_name.clone(),
                    methods: decl
                        .methods
                        .iter()
                        .map(|function| Function {
                            name: function.name.clone(),
                            type_params: function.type_params.clone(),
                            params: function
                                .params
                                .iter()
                                .map(|param| Param {
                                    name: param.name.clone(),
                                    ty: resolve_alias_type(&param.ty, &map),
                                })
                                .collect(),
                            return_type: resolve_alias_type(&function.return_type, &map),
                            body: function
                                .body
                                .iter()
                                .map(|stmt| rewrite_stmt_types(stmt, &map))
                                .collect(),
                            span: function.span,
                            is_public: function.is_public,
                            is_async: function.is_async,
                            is_extern: function.is_extern,
                            is_export: function.is_export,
                        })
                        .collect(),
                    span: decl.span,
                })
                .collect(),
        },
        diagnostics,
    )
}

/// True if following the alias chain from `name` revisits `name` (a cycle).
fn chain_is_cyclic(name: &str, map: &HashMap<String, TypeRef>) -> bool {
    let mut seen = HashSet::new();
    let mut current = name.to_string();
    while let Some(target) = map.get(&current) {
        if !map.contains_key(&target.name) {
            return false;
        }
        current = target.name.clone();
        if current == name {
            return true;
        }
        if !seen.insert(current.clone()) {
            return false;
        }
    }
    false
}

/// Render an assignment place path for diagnostics, e.g. `.items[0].x`.
/// True for the four variant names reserved by the built-in `option`/`result`
/// generic enums.
fn is_builtin_variant(name: &str) -> bool {
    matches!(name, "some" | "none" | "ok" | "err")
}

/// The five `MemoryOrder` enum variant names, weakest to strongest. `MemoryOrder`
/// is a compiler-provided nominal enum (like `option`/`result`); its unit
/// variants are the memory orderings passed to the ordering-taking atomic
/// builtins (`atomic_*_ordered`) and to `fence`.
const MEMORY_ORDER_VARIANTS: [&str; 5] = ["relaxed", "acquire", "release", "acq_rel", "seq_cst"];

/// The outcome of unifying a call's arguments against a generic function's
/// parameter types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenericInferenceError {
    /// A type variable received two different concrete types (`L0395`).
    Conflict {
        param: String,
        first: TypeRef,
        second: TypeRef,
    },
    /// A type variable in the return type was never pinned by an argument
    /// (`L0396`).
    Unresolved { param: String },
}

/// Split a `ctor<...>` spelling into `(ctor, args)` when it is a compound
/// generic type. Recognizes every built-in generic constructor plus function
/// types spelled `fn(...) -> R`. Returns `None` for a plain type name.
fn decompose_generic(ty: &TypeRef) -> Option<(String, Vec<TypeRef>)> {
    if let Some((params, ret)) = ty.function_signature() {
        let mut args = params;
        args.push(ret);
        return Some(("fn".to_string(), args));
    }
    for ctor in [
        "array", "list", "option", "result", "map", "ptr", "ref", "rc",
    ] {
        if let Some(args) = ty.generic_args(ctor) {
            return Some((ctor.to_string(), args));
        }
    }
    None
}

/// Structurally unify a parameter type (which may contain type variables drawn
/// from `type_params`) against a concrete argument type, extending `subst`.
///
/// - A parameter type that is exactly a type-variable name binds that variable
///   to the whole argument type, reporting `L0395` on a conflicting rebinding.
/// - A compound parameter type (`list<T>`, `option<T>`, `fn(T) -> U`, ...)
///   unifies argument-wise against a same-constructor argument type.
/// - Any non-variable, non-compound parameter type is a fixed type and is not
///   unified here (ordinary argument type checking already validates it).
pub fn unify_param(
    param: &TypeRef,
    actual: &TypeRef,
    type_params: &[String],
    subst: &mut HashMap<String, TypeRef>,
) -> Result<(), GenericInferenceError> {
    if type_params.iter().any(|name| name == &param.name) {
        // `param` is a bare type variable: bind it to the whole actual type.
        match subst.get(&param.name) {
            Some(existing) if existing != actual => {
                return Err(GenericInferenceError::Conflict {
                    param: param.name.clone(),
                    first: existing.clone(),
                    second: actual.clone(),
                });
            }
            Some(_) => {}
            None => {
                subst.insert(param.name.clone(), actual.clone());
            }
        }
        return Ok(());
    }

    // A compound parameter type unifies component-wise when the argument shares
    // its constructor and arity. If the argument does not match structurally the
    // ordinary argument type check reports the mismatch, so we simply stop.
    if let (Some((param_ctor, param_args)), Some((actual_ctor, actual_args))) =
        (decompose_generic(param), decompose_generic(actual))
        && param_ctor == actual_ctor
        && param_args.len() == actual_args.len()
    {
        for (p, a) in param_args.iter().zip(actual_args.iter()) {
            unify_param(p, a, type_params, subst)?;
        }
    }
    Ok(())
}

/// Substitute inferred type variables into a declared type. Applies to a bare
/// type variable and recursively into compound types (`option<T>`, `fn(T) -> U`,
/// ...). Type variables with no binding are left as-is; the caller detects those
/// via `unresolved_type_vars`.
pub fn substitute_type(ty: &TypeRef, subst: &HashMap<String, TypeRef>) -> TypeRef {
    if let Some(bound) = subst.get(&ty.name) {
        return bound.clone();
    }
    if let Some((ctor, args)) = decompose_generic(ty) {
        let mapped: Vec<TypeRef> = args.iter().map(|arg| substitute_type(arg, subst)).collect();
        if ctor == "fn" {
            let (params, ret) = mapped.split_at(mapped.len() - 1);
            return function_type(params, &ret[0]);
        }
        return generic_type(&ctor, &mapped);
    }
    ty.clone()
}

/// Report the first declared type parameter that still appears unbound in `ty`
/// after substitution (`L0396`). Recurses into compound types.
pub fn first_unresolved_type_var(
    ty: &TypeRef,
    type_params: &[String],
    subst: &HashMap<String, TypeRef>,
) -> Option<String> {
    if type_params.iter().any(|name| name == &ty.name) && !subst.contains_key(&ty.name) {
        return Some(ty.name.clone());
    }
    if let Some((_, args)) = decompose_generic(ty) {
        for arg in &args {
            if let Some(found) = first_unresolved_type_var(arg, type_params, subst) {
                return Some(found);
            }
        }
    }
    None
}

/// True when `ty` mentions the type variable `var` anywhere (as the whole type
/// or nested inside a compound type).
fn type_contains_var(ty: &TypeRef, var: &str) -> bool {
    if ty.name == var {
        return true;
    }
    if let Some((_, args)) = decompose_generic(ty) {
        return args.iter().any(|arg| type_contains_var(arg, var));
    }
    false
}

/// Run full call-site inference for a generic function: unify every argument
/// against the corresponding parameter, then substitute into the return type.
/// Returns the concrete return type, or the first inference error.
pub fn infer_generic_return(
    signature: &Signature,
    arg_types: &[TypeRef],
) -> Result<TypeRef, GenericInferenceError> {
    let mut subst: HashMap<String, TypeRef> = HashMap::new();
    for (param, actual) in signature.params.iter().zip(arg_types.iter()) {
        unify_param(param, actual, &signature.type_params, &mut subst)?;
    }
    if let Some(param) =
        first_unresolved_type_var(&signature.return_type, &signature.type_params, &subst)
    {
        return Err(GenericInferenceError::Unresolved { param });
    }
    Ok(substitute_type(&signature.return_type, &subst))
}

/// Replace the `Self` type variable with the implementing type. Recurses into
/// compound generic types (`option<Self>`, `list<Self>`, ...).
fn substitute_self(ty: &TypeRef, self_ty: &TypeRef) -> TypeRef {
    let mut subst: HashMap<String, TypeRef> = HashMap::new();
    subst.insert("Self".to_string(), self_ty.clone());
    substitute_type(ty, &subst)
}

/// Map a checked value type to the runtime/impl type name used for trait-method
/// dispatch. Structs/enums use their declared name; scalars map to their
/// primitive name. Generic/compound types are dispatched by their constructor
/// name (e.g. `list`), which is sufficient for the first increment.
fn dispatch_type_name(ty: &TypeRef) -> String {
    if let Some((ctor, _)) = decompose_generic(ty) {
        return ctor;
    }
    ty.name.clone()
}

/// Canonical `option<T>` type spelling.
fn option_type(payload: &TypeRef) -> TypeRef {
    generic_type("option", std::slice::from_ref(payload))
}

/// Canonical `result<T, E>` type spelling.
fn result_type(ok: &TypeRef, err: &TypeRef) -> TypeRef {
    generic_type("result", &[ok.clone(), err.clone()])
}

/// Canonical `list<T>` type spelling.
fn list_type(element: &TypeRef) -> TypeRef {
    generic_type("list", std::slice::from_ref(element))
}

/// The element type `T` of a `list<T>` spelling, if any.
fn list_element(ty: &TypeRef) -> Option<TypeRef> {
    ty.generic_args("list")
        .filter(|args| args.len() == 1)
        .map(|mut args| args.remove(0))
}

/// Canonical `map<K, V>` type spelling.
fn map_type(key: &TypeRef, value: &TypeRef) -> TypeRef {
    generic_type("map", &[key.clone(), value.clone()])
}

/// A `map<K, V>` key type is restricted to `i64` or `string`.
fn map_key_ok(key: &TypeRef) -> bool {
    matches!(key.name.as_str(), "i64" | "string")
}

/// The `(K, V)` type pair of a `map<K, V>` spelling, if any.
fn map_kv(ty: &TypeRef) -> Option<(TypeRef, TypeRef)> {
    ty.generic_args("map")
        .filter(|args| args.len() == 2)
        .map(|mut args| {
            let value = args.remove(1);
            let key = args.remove(0);
            (key, value)
        })
}

fn render_place_path(path: &[Place]) -> String {
    let mut out = String::new();
    for place in path {
        match place {
            Place::Field(field) => {
                out.push('.');
                out.push_str(field);
            }
            Place::Index(_) => out.push_str("[…]"),
        }
    }
    out
}

/// Expand alias names inside a type, including generic arguments, to canonical
/// form. Bounded by a depth guard so cyclic aliases cannot loop forever.
fn resolve_alias_type(ty: &TypeRef, map: &HashMap<String, TypeRef>) -> TypeRef {
    resolve_alias_type_depth(ty, map, 0)
}

fn resolve_alias_type_depth(ty: &TypeRef, map: &HashMap<String, TypeRef>, depth: usize) -> TypeRef {
    if depth > 32 {
        return ty.clone();
    }
    for ctor in ["array", "ptr", "ref", "rc"] {
        if let Some(inner) = ty.generic_arg(ctor) {
            let resolved = resolve_alias_type_depth(&inner, map, depth + 1);
            return TypeRef::new(format!("{ctor}<{}>", resolved.name));
        }
    }
    if let Some(target) = map.get(&ty.name) {
        return resolve_alias_type_depth(target, map, depth + 1);
    }
    ty.clone()
}

/// Rewrite alias types in a statement's type annotations, recursing into blocks.
fn rewrite_stmt_types(stmt: &Stmt, map: &HashMap<String, TypeRef>) -> Stmt {
    match stmt {
        Stmt::Let {
            name,
            ty,
            value,
            span,
        } => Stmt::Let {
            name: name.clone(),
            ty: ty.as_ref().map(|ty| resolve_alias_type(ty, map)),
            value: value.clone(),
            span: *span,
        },
        Stmt::If {
            branches,
            else_body,
            span,
        } => Stmt::If {
            branches: branches
                .iter()
                .map(|branch| IfBranch {
                    condition: branch.condition.clone(),
                    body: branch
                        .body
                        .iter()
                        .map(|stmt| rewrite_stmt_types(stmt, map))
                        .collect(),
                })
                .collect(),
            else_body: else_body
                .iter()
                .map(|stmt| rewrite_stmt_types(stmt, map))
                .collect(),
            span: *span,
        },
        Stmt::While {
            condition,
            body,
            span,
        } => Stmt::While {
            condition: condition.clone(),
            body: body.iter().map(|s| rewrite_stmt_types(s, map)).collect(),
            span: *span,
        },
        Stmt::For {
            name,
            start,
            end,
            step,
            body,
            span,
        } => Stmt::For {
            name: name.clone(),
            start: start.clone(),
            end: end.clone(),
            step: step.clone(),
            body: body.iter().map(|s| rewrite_stmt_types(s, map)).collect(),
            span: *span,
        },
        Stmt::ForEach {
            name,
            iterable,
            body,
            span,
        } => Stmt::ForEach {
            name: name.clone(),
            iterable: iterable.clone(),
            body: body.iter().map(|s| rewrite_stmt_types(s, map)).collect(),
            span: *span,
        },
        Stmt::Loop { body, span } => Stmt::Loop {
            body: body.iter().map(|s| rewrite_stmt_types(s, map)).collect(),
            span: *span,
        },
        Stmt::Unsafe { body, span } => Stmt::Unsafe {
            body: body.iter().map(|s| rewrite_stmt_types(s, map)).collect(),
            span: *span,
        },
        Stmt::Try {
            body,
            catch_name,
            catch_body,
            span,
        } => Stmt::Try {
            body: body.iter().map(|s| rewrite_stmt_types(s, map)).collect(),
            catch_name: catch_name.clone(),
            catch_body: catch_body
                .iter()
                .map(|s| rewrite_stmt_types(s, map))
                .collect(),
            span: *span,
        },
        // A `match` reaches semantics wrapped in a `Stmt::Expr`; rewrite type
        // annotations inside its arm bodies so aliases in arm `let`s resolve.
        Stmt::Expr(Expr {
            kind: ExprKind::Match { scrutinee, arms },
            span,
        }) => Stmt::Expr(Expr {
            kind: ExprKind::Match {
                scrutinee: scrutinee.clone(),
                arms: arms
                    .iter()
                    .map(|arm| MatchArm {
                        pattern: arm.pattern.clone(),
                        body: arm
                            .body
                            .iter()
                            .map(|s| rewrite_stmt_types(s, map))
                            .collect(),
                    })
                    .collect(),
            },
            span: *span,
        }),
        other => other.clone(),
    }
}

pub fn validate_executable(program: &Program) -> Result<CheckedProgram, Vec<SemanticDiagnostic>> {
    let checked = validate(program)?;
    validate_entrypoint(program)?;
    Ok(checked)
}

pub fn validate_entrypoint(program: &Program) -> Result<(), Vec<SemanticDiagnostic>> {
    let Some(main) = program
        .functions
        .iter()
        .find(|function| function.name == "main")
    else {
        return Err(vec![SemanticDiagnostic::new(
            "L0329",
            "executable source must define a zero-argument `main` function",
            None,
        )]);
    };

    if main.params.is_empty() {
        Ok(())
    } else {
        Err(vec![SemanticDiagnostic::at(
            "L0329",
            format!(
                "executable `main` must take zero arguments but declares {}",
                main.params.len()
            ),
            Some(main.name.clone()),
            main.span,
        )])
    }
}

struct Checker<'a> {
    program: &'a Program,
    signatures: HashMap<String, Signature>,
    expression_types: Vec<ExpressionType>,
    diagnostics: Vec<SemanticDiagnostic>,
    loop_depth: usize,
    unsafe_depth: usize,
    region_names: HashSet<String>,
    /// Declared struct types: name -> ordered fields.
    structs: HashMap<String, Vec<StructField>>,
    /// Declared enum types: enum name -> ordered variants.
    enums: HashMap<String, Vec<EnumVariant>>,
    /// Variant name -> (owning enum name, payload types). Variant names are
    /// globally unique across all enums, so this resolves construction directly.
    variants: HashMap<String, (String, Vec<TypeRef>)>,
    /// Trait name -> its required method signatures. A method signature stores
    /// the parameters after `self` and the return type; `Self` in a type means
    /// the implementing type.
    traits: HashMap<String, Vec<MethodSig>>,
    /// Trait method name -> owning trait name. Trait method names are disjoint
    /// from free-function names, so this resolves a call to a trait method.
    trait_methods: HashMap<String, String>,
    /// `(type_name, method_name)` -> the impl method's resolved signature
    /// `(param types after self, return type)` with `Self` substituted to the
    /// implementing type.
    impl_methods: HashMap<(String, String), (Vec<TypeRef>, TypeRef)>,
    /// Set of `(type_name, trait_name)` pairs that have an `impl`. Used for
    /// bound checking (`L0400`) and duplicate detection (`L0399`).
    impl_traits: HashSet<(String, String)>,
    /// Return types inferred for functions declared without a `-> T` clause
    /// (`INFERRED_RETURN`), keyed by function name. Populated by
    /// [`Checker::resolve_inferred_returns`] before body validation; consulted
    /// via [`Checker::effective_return_type`] and written back into the program.
    resolved_returns: HashMap<String, TypeRef>,
    /// Functions whose return type is currently being inferred, to detect
    /// (mutual) recursion through inferred-return functions (`L0439`).
    inferring: HashSet<String>,
    /// `L0439` diagnostics raised while inferring return types. Kept out of the
    /// truncated inference pass and merged into `diagnostics` afterward so they
    /// survive; the real body pass would otherwise discard them.
    deferred_diagnostics: Vec<SemanticDiagnostic>,
    /// Functions whose return-type inference failed (recursion). Their bodies are
    /// skipped in the real pass so a broken return type does not cascade into
    /// confusing secondary errors; the `L0439` already tells the user the fix.
    inference_failed: HashSet<String>,
}

impl<'a> Checker<'a> {
    fn new(program: &'a Program) -> Self {
        Self {
            program,
            signatures: HashMap::new(),
            expression_types: Vec::new(),
            diagnostics: Vec::new(),
            loop_depth: 0,
            unsafe_depth: 0,
            region_names: HashSet::new(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            variants: HashMap::new(),
            traits: HashMap::new(),
            trait_methods: HashMap::new(),
            impl_methods: HashMap::new(),
            impl_traits: HashSet::new(),
            resolved_returns: HashMap::new(),
            inferring: HashSet::new(),
            deferred_diagnostics: Vec::new(),
            inference_failed: HashSet::new(),
        }
    }

    fn validate(&mut self) {
        self.collect_structs();
        self.collect_enums();
        self.collect_traits();
        self.collect_signatures();
        self.collect_impls();
        // Resolve the return type of every function declared without `-> T`
        // before any body is validated, so call sites see concrete return types.
        self.resolve_inferred_returns();
        for function in &self.program.functions {
            // A function whose return-type inference failed (recursion) already
            // has its `L0439`; skip its body so the broken return type does not
            // cascade into confusing secondary diagnostics.
            if self.inference_failed.contains(&function.name) {
                continue;
            }
            // Extern (C-ABI) declarations are body-less: there is no body to
            // check, but their C-marshallable signature is validated so an
            // unmarshallable parameter/return type (`list`/`map`/non-`repr(C)`
            // struct/callback/`string`) is rejected up front with `L0424`
            // rather than silently demoted at native codegen.
            if function.is_extern {
                self.check_extern_signature(function);
                continue;
            }
            // An `export fn` is exposed under its plain C name as a native symbol.
            // The first increment supports only the Win64 integer convention, so
            // its parameters and return type must all be `i64`. Reject anything
            // else up front (`L0424`) rather than silently demoting it to a native
            // skip, since `export` is an explicit user request.
            if function.is_export {
                self.check_export_signature(function);
            }
            self.validate_function(function);
        }
        self.validate_impls();
        // Validate each impl method body like an ordinary function. Its `self`
        // parameter already carries the implementing type, so field access and
        // trait-method calls on `self` resolve normally.
        for decl in &self.program.impls {
            for method in &decl.methods {
                self.validate_function(method);
            }
        }
    }

    /// Check that an `export fn` has a C-callable scalar signature. Exports cross
    /// the Win64 C ABI in the delivered scalar set: `i64` (integer register) and
    /// the `f64`/`f32` floats (SSE registers `xmm0..3`, positionally routed). Any
    /// other parameter/return type — a wider marshalling case (pointers, structs,
    /// strings) not yet exported — is `L0424`. Generic exports are also rejected
    /// (a C symbol is monomorphic).
    fn check_export_signature(&mut self, function: &Function) {
        if !function.type_params.is_empty() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0424",
                format!(
                    "`export fn {}` cannot be generic; exports must be monomorphic scalar functions",
                    function.name
                ),
                Some(function.name.clone()),
                function.span,
            ));
        }
        for param in &function.params {
            if !Self::is_exportable_scalar(&param.ty.name) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0424",
                    format!(
                        "`export fn {}` parameter `{}` has type `{}`; exports support the scalar set `i64`/`f64`/`f32` (pointers, structs, and strings are not yet exportable)",
                        function.name, param.name, param.ty.name
                    ),
                    Some(function.name.clone()),
                    function.span,
                ));
            }
        }
        if !Self::is_exportable_scalar(&function.return_type.name) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0424",
                format!(
                    "`export fn {}` returns `{}`; exports support the scalar set `i64`/`f64`/`f32` (pointers, structs, and strings are not yet exportable)",
                    function.name, function.return_type.name
                ),
                Some(function.name.clone()),
                function.span,
            ));
        }
    }

    /// Whether a type name is in the delivered `export fn` C-ABI scalar set:
    /// `i64` (integer register) or an `f64`/`f32` float (SSE register). Wider
    /// scalar/aggregate marshalling for exports is deferred.
    fn is_exportable_scalar(type_name: &str) -> bool {
        matches!(type_name, "i64" | "f64" | "f32")
    }

    /// Check that a body-less `extern fn` has a C-marshallable signature. The
    /// delivered `extern`-call marshalling set is: every fixed-width scalar
    /// (`i8`…`u64`, `isize`/`usize`, `bool`, `char`, `byte`, `f32`, `f64`), a raw
    /// pointer `ptr<T>` (a machine address, passed/returned by value), and — for a
    /// **parameter only** — the FFI-only `cstr` marker (a Lullaby `string` is
    /// materialized into a NUL-terminated buffer across the boundary). A return
    /// type may additionally be `void`. Anything else — a `string`, `list`/`map`,
    /// non-`repr(C)` struct/enum, callback (`fn(...) -> R`), array, or a `cstr`
    /// return — is not yet marshallable and is rejected with `L0424` (the shared
    /// FFI-signature family) rather than silently demoted at native codegen. A
    /// generic extern is rejected (a C symbol is monomorphic).
    fn check_extern_signature(&mut self, function: &Function) {
        if !function.type_params.is_empty() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0424",
                format!(
                    "`extern fn {}` cannot be generic; an imported C symbol is monomorphic",
                    function.name
                ),
                Some(function.name.clone()),
                function.span,
            ));
        }
        for param in &function.params {
            if !Self::is_extern_param_type(&param.ty) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0424",
                    format!(
                        "`extern fn {}` parameter `{}` has type `{}`; an extern parameter must be a C scalar (`i8`…`u64`/`isize`/`usize`/`bool`/`char`/`byte`/`f32`/`f64`), a raw pointer `ptr<T>`, or `cstr` (structs by value, callbacks, `string`, `list`/`map` are not yet marshallable)",
                        function.name, param.name, param.ty.name
                    ),
                    Some(function.name.clone()),
                    function.span,
                ));
            }
        }
        if !Self::is_extern_return_type(&function.return_type) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0424",
                format!(
                    "`extern fn {}` returns `{}`; an extern return must be `void`, a C scalar, or a raw pointer `ptr<T>` (`cstr`, structs by value, `string`, `list`/`map` returns are not yet marshallable)",
                    function.name, function.return_type.name
                ),
                Some(function.name.clone()),
                function.span,
            ));
        }
    }

    /// A C scalar type marshallable across the FFI boundary in either direction:
    /// every fixed-width integer plus `bool`/`char`/`byte` and the floats.
    fn is_ffi_scalar(type_name: &str) -> bool {
        matches!(
            type_name,
            "i8" | "i16"
                | "i32"
                | "i64"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "isize"
                | "usize"
                | "bool"
                | "char"
                | "byte"
                | "f32"
                | "f64"
        )
    }

    /// Whether a type is valid as an `extern fn` **parameter**: a C scalar, a raw
    /// pointer `ptr<T>`, or the FFI-only `cstr` marker (a materialized C string).
    fn is_extern_param_type(ty: &TypeRef) -> bool {
        Self::is_ffi_scalar(&ty.name) || ty.name == "cstr" || ty.is_raw_pointer()
    }

    /// Whether a type is valid as an `extern fn` **return**: `void`, a C scalar, or
    /// a raw pointer `ptr<T>`. `cstr` is not a returnable value type (an inbound C
    /// string is received as `ptr<byte>` and copied explicitly).
    fn is_extern_return_type(ty: &TypeRef) -> bool {
        ty.is_void() || Self::is_ffi_scalar(&ty.name) || ty.is_raw_pointer()
    }

    /// Collect trait declarations into the trait table and the trait-method
    /// index. A trait method name must be globally unique and disjoint from free
    /// function names; a clash is reported when signatures are collected.
    fn collect_traits(&mut self) {
        for decl in &self.program.traits {
            if self.traits.contains_key(&decl.name) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0398",
                    format!("duplicate trait `{}`", decl.name),
                    None,
                    decl.span,
                ));
                continue;
            }
            for method in &decl.methods {
                if let Some(other) = self.trait_methods.get(&method.name) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0398",
                        format!(
                            "trait method `{}` is declared in both trait `{other}` and trait `{}`",
                            method.name, decl.name
                        ),
                        None,
                        decl.span,
                    ));
                    continue;
                }
                self.trait_methods
                    .insert(method.name.clone(), decl.name.clone());
            }
            self.traits.insert(decl.name.clone(), decl.methods.clone());
        }
    }

    /// Collect and validate `impl Trait for Type` blocks. Each impl must provide
    /// exactly the trait's methods with matching signatures (with `Self` =
    /// implementing type). Missing/extra/mismatched → `L0398`; duplicate impl of
    /// the same trait for the same type → `L0399`.
    fn collect_impls(&mut self) {
        for decl in &self.program.impls {
            let key = (decl.type_name.clone(), decl.trait_name.clone());
            if self.impl_traits.contains(&key) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0399",
                    format!(
                        "duplicate `impl {} for {}`",
                        decl.trait_name, decl.type_name
                    ),
                    None,
                    decl.span,
                ));
                continue;
            }
            if !self.traits.contains_key(&decl.trait_name) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0398",
                    format!("unknown trait `{}` in impl block", decl.trait_name),
                    None,
                    decl.span,
                ));
                continue;
            }
            self.impl_traits.insert(key);

            // Index the impl's method signatures (Self-substituted) so trait-
            // method calls can look them up by (type, method).
            let self_ty = TypeRef::new(decl.type_name.clone());
            for method in &decl.methods {
                // The remaining parameter types after `self`.
                let param_types: Vec<TypeRef> = method
                    .params
                    .iter()
                    .skip(1)
                    .map(|param| substitute_self(&param.ty, &self_ty))
                    .collect();
                let return_type = substitute_self(&method.return_type, &self_ty);
                self.impl_methods.insert(
                    (decl.type_name.clone(), method.name.clone()),
                    (param_types, return_type),
                );
            }
        }
    }

    /// After signatures/impl methods are indexed, verify each impl satisfies its
    /// trait exactly: every required method present, no extras, and each matching
    /// the declared signature with `Self` = implementing type (`L0398`).
    fn validate_impls(&mut self) {
        let mut diagnostics = Vec::new();
        // Trait method names must not clash with free function names.
        for method_name in self.trait_methods.keys() {
            if self.signatures.contains_key(method_name) {
                diagnostics.push(SemanticDiagnostic::new(
                    "L0398",
                    format!(
                        "`{method_name}` is both a free function and a trait method; the two namespaces must be disjoint"
                    ),
                    None,
                ));
            }
        }

        for decl in &self.program.impls {
            let Some(trait_methods) = self.traits.get(&decl.trait_name) else {
                continue; // already reported unknown trait
            };
            let self_ty = TypeRef::new(decl.type_name.clone());

            // Every required method must be present with a matching signature.
            for required in trait_methods {
                let Some(provided) = decl
                    .methods
                    .iter()
                    .find(|method| method.name == required.name)
                else {
                    diagnostics.push(SemanticDiagnostic::at(
                        "L0398",
                        format!(
                            "`impl {} for {}` is missing method `{}` required by the trait",
                            decl.trait_name, decl.type_name, required.name
                        ),
                        None,
                        decl.span,
                    ));
                    continue;
                };
                Self::check_impl_method_signature(
                    decl,
                    required,
                    provided,
                    &self_ty,
                    &mut diagnostics,
                );
            }

            // No extra methods beyond what the trait requires.
            for method in &decl.methods {
                if !trait_methods.iter().any(|m| m.name == method.name) {
                    diagnostics.push(SemanticDiagnostic::at(
                        "L0398",
                        format!(
                            "`impl {} for {}` declares method `{}` which is not part of the trait",
                            decl.trait_name, decl.type_name, method.name
                        ),
                        None,
                        method.span,
                    ));
                }
            }
        }
        self.diagnostics.extend(diagnostics);
    }

    /// Check that a provided impl method matches the trait's required signature
    /// with `Self` substituted to the implementing type. The receiver `self` must
    /// be the first parameter and have the implementing type.
    fn check_impl_method_signature(
        decl: &lullaby_parser::ImplDecl,
        required: &MethodSig,
        provided: &Function,
        self_ty: &TypeRef,
        diagnostics: &mut Vec<SemanticDiagnostic>,
    ) {
        if provided.params.is_empty() || provided.params[0].name != "self" {
            diagnostics.push(SemanticDiagnostic::at(
                "L0398",
                format!(
                    "method `{}` of `impl {} for {}` must take `self` as its first parameter",
                    required.name, decl.trait_name, decl.type_name
                ),
                Some(provided.name.clone()),
                provided.span,
            ));
            return;
        }
        let provided_rest = &provided.params[1..];
        if provided_rest.len() != required.params.len() {
            diagnostics.push(SemanticDiagnostic::at(
                "L0398",
                format!(
                    "method `{}` of `impl {} for {}` takes {} parameter(s) after `self` but the trait requires {}",
                    required.name,
                    decl.trait_name,
                    decl.type_name,
                    provided_rest.len(),
                    required.params.len()
                ),
                Some(provided.name.clone()),
                provided.span,
            ));
            return;
        }
        for (provided_param, required_param) in provided_rest.iter().zip(required.params.iter()) {
            let expected = substitute_self(&required_param.ty, self_ty);
            if provided_param.ty != expected {
                diagnostics.push(SemanticDiagnostic::at(
                    "L0398",
                    format!(
                        "parameter `{}` of method `{}` in `impl {} for {}` must be `{}` but is `{}`",
                        provided_param.name,
                        required.name,
                        decl.trait_name,
                        decl.type_name,
                        expected.name,
                        provided_param.ty.name
                    ),
                    Some(provided.name.clone()),
                    provided.span,
                ));
            }
        }
        let expected_return = substitute_self(&required.return_type, self_ty);
        if provided.return_type != expected_return {
            diagnostics.push(SemanticDiagnostic::at(
                "L0398",
                format!(
                    "method `{}` of `impl {} for {}` must return `{}` but returns `{}`",
                    required.name,
                    decl.trait_name,
                    decl.type_name,
                    expected_return.name,
                    provided.return_type.name
                ),
                Some(provided.name.clone()),
                provided.span,
            ));
        }
    }

    fn collect_structs(&mut self) {
        for declaration in &self.program.structs {
            if self.structs.contains_key(&declaration.name) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0370",
                    format!("duplicate struct `{}`", declaration.name),
                    None,
                    declaration.span,
                ));
                continue;
            }
            let mut seen = HashSet::new();
            for field in &declaration.fields {
                if !seen.insert(field.name.clone()) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0370",
                        format!(
                            "duplicate field `{}` in struct `{}`",
                            field.name, declaration.name
                        ),
                        None,
                        declaration.span,
                    ));
                }
            }
            self.structs
                .insert(declaration.name.clone(), declaration.fields.clone());
        }
    }

    /// Collect enum declarations. Enforces unique enum names, unique variant
    /// names within an enum, non-empty enums (`L0380`), and global uniqueness of
    /// variant names across all enums (`L0382`).
    fn collect_enums(&mut self) {
        // Register the compiler-provided `MemoryOrder` enum before user enums so
        // its five unit variants resolve for construction and `match`, and any
        // user enum that reuses the name (`L0380`) or one of its variant names
        // (`L0382`) collides against it through the existing checks below.
        self.enums.insert(
            "MemoryOrder".to_string(),
            MEMORY_ORDER_VARIANTS
                .iter()
                .map(|name| EnumVariant {
                    name: (*name).to_string(),
                    payload: Vec::new(),
                })
                .collect(),
        );
        for name in MEMORY_ORDER_VARIANTS {
            self.variants
                .insert(name.to_string(), ("MemoryOrder".to_string(), Vec::new()));
        }
        for declaration in &self.program.enums {
            // `option` and `result` are built-in generic enum names; a user enum
            // may not redeclare them.
            if matches!(declaration.name.as_str(), "option" | "result") {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0380",
                    format!(
                        "`{}` is a built-in generic enum and cannot be redeclared",
                        declaration.name
                    ),
                    None,
                    declaration.span,
                ));
                continue;
            }
            if self.enums.contains_key(&declaration.name) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0380",
                    format!("duplicate enum `{}`", declaration.name),
                    None,
                    declaration.span,
                ));
                continue;
            }
            if declaration.variants.is_empty() {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0380",
                    format!("enum `{}` declares no variants", declaration.name),
                    None,
                    declaration.span,
                ));
            }
            let mut seen = HashSet::new();
            for variant in &declaration.variants {
                if !seen.insert(variant.name.clone()) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0380",
                        format!(
                            "duplicate variant `{}` in enum `{}`",
                            variant.name, declaration.name
                        ),
                        None,
                        declaration.span,
                    ));
                    continue;
                }
                // `some`/`none`/`ok`/`err` are reserved for the built-in
                // `option`/`result` generic enums; a user variant may not shadow
                // them (reuse the `L0382` global-collision diagnostic).
                if is_builtin_variant(&variant.name) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0382",
                        format!(
                            "variant `{}` is reserved for the built-in `option`/`result` types",
                            variant.name
                        ),
                        None,
                        declaration.span,
                    ));
                    continue;
                }
                if let Some((other, _)) = self.variants.get(&variant.name) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0382",
                        format!(
                            "variant `{}` is declared in both enum `{other}` and enum `{}`",
                            variant.name, declaration.name
                        ),
                        None,
                        declaration.span,
                    ));
                    continue;
                }
                self.variants.insert(
                    variant.name.clone(),
                    (declaration.name.clone(), variant.payload.clone()),
                );
            }
            self.enums
                .insert(declaration.name.clone(), declaration.variants.clone());
        }
    }

    fn collect_signatures(&mut self) {
        for function in &self.program.functions {
            if self.signatures.contains_key(&function.name) {
                self.diagnostics.push(SemanticDiagnostic::new(
                    "L0300",
                    format!("duplicate function `{}`", function.name),
                    Some(function.name.clone()),
                ));
                continue;
            }
            self.signatures.insert(
                function.name.clone(),
                Signature {
                    params: function
                        .params
                        .iter()
                        .map(|param| param.ty.clone())
                        .collect(),
                    return_type: function.return_type.clone(),
                    type_params: function
                        .type_params
                        .iter()
                        .map(|tp| tp.name.clone())
                        .collect(),
                    type_param_bounds: function
                        .type_params
                        .iter()
                        .map(|tp| tp.bounds.clone())
                        .collect(),
                    is_async: function.is_async,
                    is_extern: function.is_extern,
                },
            );
        }
    }

    /// The concrete return type of `function`: the inferred type for a function
    /// declared without `-> T` (`INFERRED_RETURN`), otherwise the explicit type.
    fn effective_return_type(&self, function: &Function) -> TypeRef {
        if function.return_type.name == INFERRED_RETURN {
            self.resolved_returns
                .get(&function.name)
                .cloned()
                .unwrap_or_else(|| TypeRef::new("void"))
        } else {
            function.return_type.clone()
        }
    }

    /// Expected type at a `return`/tail-expression site: the inferred return type
    /// once known, `None` while it is still being inferred (so the body type is
    /// computed freely), or the explicit type.
    fn expected_return(&self, function: &Function) -> Option<TypeRef> {
        if function.return_type.name == INFERRED_RETURN {
            self.resolved_returns.get(&function.name).cloned()
        } else {
            Some(function.return_type.clone())
        }
    }

    /// Whether a function's final expression is its return value (return type is
    /// non-void). While an inferred return type is still being computed, treat
    /// the tail expression as the value so its type is captured.
    fn wants_tail_value(&self, function: &Function) -> bool {
        if function.return_type.name == INFERRED_RETURN {
            match self.resolved_returns.get(&function.name) {
                Some(ty) => !ty.is_void(),
                None => true,
            }
        } else {
            !function.return_type.is_void()
        }
    }

    /// Infer, once, the return type of every function declared without a `-> T`
    /// clause. Runs before body validation so every call site resolves to a
    /// concrete return type.
    fn resolve_inferred_returns(&mut self) {
        let names: Vec<String> = self
            .program
            .functions
            .iter()
            .filter(|f| f.return_type.name == INFERRED_RETURN)
            .map(|f| f.name.clone())
            .collect();
        for name in names {
            self.infer_return(&name);
        }
        // Surface the recursion errors that were held out of the truncated
        // inference pass.
        let deferred = std::mem::take(&mut self.deferred_diagnostics);
        self.diagnostics.extend(deferred);
    }

    /// Resolve one function's inferred return type on demand (memoized in
    /// `resolved_returns` and written back into its signature). A function that
    /// reaches itself before its type is known is (mutually) recursive and needs
    /// an explicit annotation (`L0439`); its type falls back to `void`.
    fn infer_return(&mut self, name: &str) -> TypeRef {
        if let Some(ty) = self.resolved_returns.get(name) {
            return ty.clone();
        }
        let function = self.program.functions.iter().find(|f| f.name == name);
        let Some(function) = function else {
            return TypeRef::new("void");
        };
        if function.return_type.name != INFERRED_RETURN {
            return function.return_type.clone();
        }
        if self.inferring.contains(name) {
            self.deferred_diagnostics.push(SemanticDiagnostic::at(
                "L0439",
                format!(
                    "cannot infer the return type of `{name}` because it is (mutually) recursive; add an explicit `-> T`"
                ),
                Some(name.to_string()),
                function.span,
            ));
            self.inference_failed.insert(name.to_string());
            let fallback = TypeRef::new("void");
            self.resolved_returns
                .insert(name.to_string(), fallback.clone());
            return fallback;
        }
        self.inferring.insert(name.to_string());

        // Type-check the body only to learn its value type. Diagnostics and
        // expression-type entries produced here are discarded; the real
        // validation pass re-emits them once every return type is known.
        let diag_mark = self.diagnostics.len();
        let expr_mark = self.expression_types.len();
        let mut scope = Scope::default();
        for param in &function.params {
            scope.locals.insert(param.name.clone(), param.ty.clone());
        }
        let block_type = self.check_function_body(&function.body, &mut scope, function);
        self.diagnostics.truncate(diag_mark);
        self.expression_types.truncate(expr_mark);
        self.inferring.remove(name);

        let ret = block_type.unwrap_or_else(|| TypeRef::new("void"));
        self.resolved_returns.insert(name.to_string(), ret.clone());
        if let Some(sig) = self.signatures.get_mut(name) {
            sig.return_type = ret.clone();
        }
        ret
    }

    fn validate_function(&mut self, function: &Function) {
        self.region_names.clear();
        let mut scope = Scope::default();
        for param in &function.params {
            if scope
                .locals
                .insert(param.name.clone(), param.ty.clone())
                .is_some()
            {
                self.diagnostics.push(SemanticDiagnostic::new(
                    "L0302",
                    format!("duplicate parameter `{}`", param.name),
                    Some(function.name.clone()),
                ));
            }
        }

        let block_type = self.check_function_body(&function.body, &mut scope, function);
        self.check_lifetimes(function);
        let return_type = self.effective_return_type(function);
        if return_type.is_void() {
            return;
        }

        if block_type.as_ref() != Some(&return_type) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0301",
                format!(
                    "function `{}` declares `{}` but has no final return value of that type",
                    function.name, return_type.name
                ),
                Some(function.name.clone()),
                function.span,
            ));
        }
    }

    fn check_block(
        &mut self,
        statements: &[Stmt],
        scope: &mut Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let mut last_type = None;
        for statement in statements {
            last_type = self.check_statement(statement, scope, function);
            if matches!(
                statement,
                Stmt::Return(_) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Throw { .. }
            ) {
                break;
            }
        }
        last_type
    }

    /// Like `check_block`, but when a function's body ends in a bare expression
    /// statement that expression is checked against the function's declared
    /// return type, so `some/none/ok/err` in final-expression position get their
    /// context-directed type (the generics-foundation return-type site).
    fn check_function_body(
        &mut self,
        statements: &[Stmt],
        scope: &mut Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let last_index = statements.len().checked_sub(1);
        let mut last_type = None;
        for (index, statement) in statements.iter().enumerate() {
            last_type = match statement {
                Stmt::Expr(expr)
                    if Some(index) == last_index && self.wants_tail_value(function) =>
                {
                    let expected = self.expected_return(function);
                    self.check_expr_expected(expr, expected.as_ref(), scope, function)
                }
                _ => self.check_statement(statement, scope, function),
            };
            if matches!(
                statement,
                Stmt::Return(_) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Throw { .. }
            ) {
                break;
            }
        }
        last_type
    }

    fn check_statement(
        &mut self,
        statement: &Stmt,
        scope: &mut Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        match statement {
            Stmt::Let {
                name, ty, value, ..
            } => {
                let value_type = self.check_expr_expected(value, ty.as_ref(), scope, function);
                let binding_type = match ty {
                    Some(declared) => {
                        if value_type.as_ref() != Some(declared) {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0303",
                                format!(
                                    "binding `{name}` declares `{}` but initializer has `{}`",
                                    declared.name,
                                    value_type
                                        .as_ref()
                                        .map(|ty| ty.name.as_str())
                                        .unwrap_or("<unknown>")
                                ),
                                Some(function.name.clone()),
                                value.span,
                            ));
                        }
                        declared.clone()
                    }
                    None => value_type.unwrap_or_else(|| TypeRef::new("<unknown>")),
                };
                if ty.is_none() && binding_type.is_void() {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0303",
                        format!("binding `{name}` cannot infer type from a void initializer"),
                        Some(function.name.clone()),
                        value.span,
                    ));
                }
                scope.locals.insert(name.clone(), binding_type);
                None
            }
            Stmt::Assign {
                name,
                path,
                op,
                value,
                span,
            } => {
                let root = scope.locals.get(name).cloned();
                let value_type = self.check_expr(value, scope, function);
                let Some(root) = root else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0316",
                        format!("assignment target `{name}` is not declared"),
                        Some(function.name.clone()),
                        *span,
                    ));
                    return None;
                };
                // Walk any `.field` path to the mutated field's type.
                let expected = self.resolve_field_path(&root, path, *span, scope, function)?;
                let target = if path.is_empty() {
                    format!("`{name}`")
                } else {
                    format!("`{name}{}`", render_place_path(path))
                };
                if *op == AssignOp::Replace {
                    if value_type.as_ref() != Some(&expected) {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0314",
                            format!(
                                "assignment to {target} expects `{}` but got `{}`",
                                expected.name,
                                value_type
                                    .as_ref()
                                    .map(|ty| ty.name.as_str())
                                    .unwrap_or("<unknown>")
                            ),
                            Some(function.name.clone()),
                            value.span,
                        ));
                    }
                } else if *op == AssignOp::Remainder {
                    // `%=` is integer remainder: floats are not allowed.
                    if expected.name.as_str() != "i64" || value_type.as_ref() != Some(&expected) {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0315",
                            format!("compound assignment `%=` to {target} requires matching i64 operands"),
                            Some(function.name.clone()),
                            value.span,
                        ));
                    }
                } else if *op == AssignOp::Add && expected.name.as_str() == "string" {
                    // `s += piece` is string concatenation. `piece` is a `string`,
                    // or a `char` coerced to a one-character string (like `s + c`).
                    let ok = matches!(
                        value_type.as_ref().map(|t| t.name.as_str()),
                        Some("string") | Some("char")
                    );
                    if !ok {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0315",
                            format!(
                                "compound assignment `+=` to {target} requires a string or char operand"
                            ),
                            Some(function.name.clone()),
                            value.span,
                        ));
                    }
                } else if !matches!(expected.name.as_str(), "i64" | "f64")
                    || value_type.as_ref() != Some(&expected)
                {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0315",
                        format!(
                            "compound assignment to {target} requires matching i64 or f64 operands"
                        ),
                        Some(function.name.clone()),
                        value.span,
                    ));
                }
                None
            }
            Stmt::Return(expr) => {
                let expected = self.expected_return(function);
                let actual = expr
                    .as_ref()
                    .map(|expr| self.check_expr_expected(expr, expected.as_ref(), scope, function))
                    .unwrap_or_else(|| Some(TypeRef::new("void")));
                // Skip the match check while the return type is still being
                // inferred (`expected` is None); the real validation pass runs
                // it once the type is known.
                if let Some(expected) = &expected
                    && actual.as_ref() != Some(expected)
                {
                    let span = expr.as_ref().map(|expr| expr.span).unwrap_or(function.span);
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0304",
                        format!(
                            "return type `{}` does not match function return `{}`",
                            actual
                                .as_ref()
                                .map(|ty| ty.name.as_str())
                                .unwrap_or("<unknown>"),
                            expected.name
                        ),
                        Some(function.name.clone()),
                        span,
                    ));
                }
                actual
            }
            Stmt::Break(span) => {
                if self.loop_depth == 0 {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0317",
                        "`break` can only appear inside a loop",
                        Some(function.name.clone()),
                        *span,
                    ));
                }
                None
            }
            Stmt::Continue(span) => {
                if self.loop_depth == 0 {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0318",
                        "`continue` can only appear inside a loop",
                        Some(function.name.clone()),
                        *span,
                    ));
                }
                None
            }
            Stmt::Expr(expr) => self.check_expr(expr, scope, function),
            Stmt::If {
                branches,
                else_body,
                ..
            } => {
                let mut branch_types = Vec::new();
                for branch in branches {
                    let condition_type = self.check_expr(&branch.condition, scope, function);
                    if condition_type.as_ref() != Some(&TypeRef::new("bool")) {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0305",
                            "if condition must be bool",
                            Some(function.name.clone()),
                            branch.condition.span,
                        ));
                    }
                    let mut branch_scope = scope.clone();
                    branch_types.push(self.check_block(&branch.body, &mut branch_scope, function));
                }
                let mut else_scope = scope.clone();
                let else_type = self.check_block(else_body, &mut else_scope, function);
                if else_body.is_empty() {
                    return None;
                }
                if branch_types
                    .iter()
                    .all(|branch_type| branch_type.as_ref() == else_type.as_ref())
                {
                    else_type
                } else {
                    None
                }
            }
            Stmt::While {
                condition, body, ..
            } => {
                let condition_type = self.check_expr(condition, scope, function);
                if condition_type.as_ref() != Some(&TypeRef::new("bool")) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0305",
                        "while condition must be bool",
                        Some(function.name.clone()),
                        condition.span,
                    ));
                }
                let mut loop_scope = scope.clone();
                self.loop_depth += 1;
                self.check_block(body, &mut loop_scope, function);
                self.loop_depth -= 1;
                None
            }
            Stmt::For {
                name,
                start,
                end,
                step,
                body,
                ..
            } => {
                for (label, expr) in [("start", start), ("end", end)] {
                    let expr_type = self.check_expr(expr, scope, function);
                    if expr_type.as_ref() != Some(&TypeRef::new("i64")) {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0321",
                            format!("for loop {label} expression must be i64"),
                            Some(function.name.clone()),
                            expr.span,
                        ));
                    }
                }
                if let Some(step) = step {
                    let step_type = self.check_expr(step, scope, function);
                    if step_type.as_ref() != Some(&TypeRef::new("i64")) {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0322",
                            "for loop step expression must be i64",
                            Some(function.name.clone()),
                            step.span,
                        ));
                    }
                }
                let mut loop_scope = scope.clone();
                loop_scope.locals.insert(name.clone(), TypeRef::new("i64"));
                self.loop_depth += 1;
                self.check_block(body, &mut loop_scope, function);
                self.loop_depth -= 1;
                None
            }
            Stmt::ForEach {
                name,
                iterable,
                body,
                ..
            } => {
                // The loop variable takes the element type: `array<T>`/`list<T>`
                // yield `T`, and a `string` yields `char`.
                let iter_type = self.check_expr(iterable, scope, function);
                let element = match iter_type.as_ref() {
                    Some(ty) if ty.name == "string" => Some(TypeRef::new("char")),
                    Some(ty) => ty.array_element().or_else(|| ty.list_element()),
                    None => None,
                };
                let element = element.unwrap_or_else(|| {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0434",
                        "`for … in` requires an array, list, or string",
                        Some(function.name.clone()),
                        iterable.span,
                    ));
                    TypeRef::new("<unknown>")
                });
                let mut loop_scope = scope.clone();
                loop_scope.locals.insert(name.clone(), element);
                self.loop_depth += 1;
                self.check_block(body, &mut loop_scope, function);
                self.loop_depth -= 1;
                None
            }
            Stmt::Loop { body, .. } => {
                let mut loop_scope = scope.clone();
                self.loop_depth += 1;
                self.check_block(body, &mut loop_scope, function);
                self.loop_depth -= 1;
                None
            }
            Stmt::Unsafe { body, .. } => {
                // `unsafe` is a transparent compile-time gate: its body runs in
                // the enclosing scope, but raw-pointer operations inside it are
                // permitted. Locals declared here remain visible afterward, to
                // match IR lowering, which inlines the body.
                self.unsafe_depth += 1;
                let block_type = self.check_block(body, scope, function);
                self.unsafe_depth -= 1;
                block_type
            }
            Stmt::Asm { bytes, span } => {
                self.check_asm(bytes, *span, function);
                // Inline assembly is trusted to leave the return value in `rax`
                // (the native epilogue returns `rax`), so — like `throw` — a
                // trailing `asm` satisfies the function's final-value requirement.
                Some(function.return_type.clone())
            }
            Stmt::Region(decl) => {
                self.check_region(decl, function);
                None
            }
            Stmt::Throw { value, .. } => {
                self.expect_arg_type("throw", 1, value, "string", scope, function);
                // `throw` diverges, so it is compatible with any return type.
                Some(function.return_type.clone())
            }
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                ..
            } => {
                let mut try_scope = scope.clone();
                let try_type = self.check_block(body, &mut try_scope, function);
                let mut catch_scope = scope.clone();
                // The caught error is exposed to the handler as a string message.
                catch_scope
                    .locals
                    .insert(catch_name.clone(), TypeRef::new("string"));
                let catch_type = self.check_block(catch_body, &mut catch_scope, function);
                // Like `if`/`else`, a `try`/`catch` yields a value only when both
                // arms produce the same type; otherwise it is a void statement.
                if try_type.as_ref() == catch_type.as_ref() {
                    try_type
                } else {
                    None
                }
            }
        }
    }

    fn check_region(&mut self, decl: &RegionDecl, function: &Function) {
        if decl.size <= 0 {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0340",
                format!("region `{}` size must be positive", decl.name),
                Some(function.name.clone()),
                decl.span,
            ));
        }
        if let Some(align) = decl.align
            && (align <= 0 || (align & (align - 1)) != 0)
        {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0340",
                format!(
                    "region `{}` alignment must be a positive power of two",
                    decl.name
                ),
                Some(function.name.clone()),
                decl.span,
            ));
        }
        if !matches!(decl.kind.as_str(), "static" | "dynamic") {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0340",
                format!(
                    "region `{}` kind `{}` must be `static` or `dynamic`",
                    decl.name, decl.kind
                ),
                Some(function.name.clone()),
                decl.span,
            ));
        }
        if !self.region_names.insert(decl.name.clone()) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0341",
                format!("duplicate region `{}`", decl.name),
                Some(function.name.clone()),
                decl.span,
            ));
        }
    }

    /// Conservative compile-time lifetime analysis.
    ///
    /// - A borrowed `ref<T>` may not be returned from a function, because the
    ///   borrow cannot outlive the owner it points into (`L0351`).
    /// - Straight-line use-after-free / double-free of a resource freed by
    ///   `dealloc`/`rc_release` is reported (`L0350`). The per-block cleanup
    ///   ordering itself is the deterministic plan produced by
    ///   `lullaby_ir::frame_layout`.
    fn check_lifetimes(&mut self, function: &Function) {
        if function.return_type.reference_target().is_some() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0351",
                format!(
                    "function `{}` returns borrowed `{}`, which cannot escape its owner's scope",
                    function.name, function.return_type.name
                ),
                Some(function.name.clone()),
                function.span,
            ));
        }
        let mut freed: HashSet<String> = HashSet::new();
        self.walk_lifetimes(&function.body, &mut freed, function);
    }

    fn walk_lifetimes(&mut self, body: &[Stmt], freed: &mut HashSet<String>, function: &Function) {
        for statement in body {
            match statement {
                Stmt::Let { name, value, .. } => {
                    self.check_freed_uses(value, freed, function);
                    // Re-binding revives a name.
                    freed.remove(name);
                }
                Stmt::Assign { name, value, .. } => {
                    self.check_freed_uses(value, freed, function);
                    freed.remove(name);
                }
                Stmt::Return(Some(expr)) | Stmt::Expr(expr) => {
                    if let Some(target) = free_call_target(expr) {
                        // The freeing call may double-free an already-dead resource.
                        if freed.contains(target) {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0350",
                                format!("`{target}` is used after it was already freed"),
                                Some(function.name.clone()),
                                expr.span,
                            ));
                        }
                        freed.insert(target.to_string());
                    } else {
                        self.check_freed_uses(expr, freed, function);
                    }
                }
                Stmt::If {
                    branches,
                    else_body,
                    ..
                } => {
                    for branch in branches {
                        self.check_freed_uses(&branch.condition, freed, function);
                        self.walk_lifetimes(&branch.body, &mut freed.clone(), function);
                    }
                    self.walk_lifetimes(else_body, &mut freed.clone(), function);
                }
                Stmt::While {
                    condition, body, ..
                } => {
                    self.check_freed_uses(condition, freed, function);
                    self.walk_lifetimes(body, &mut freed.clone(), function);
                }
                Stmt::For {
                    start,
                    end,
                    step,
                    body,
                    ..
                } => {
                    self.check_freed_uses(start, freed, function);
                    self.check_freed_uses(end, freed, function);
                    if let Some(step) = step {
                        self.check_freed_uses(step, freed, function);
                    }
                    self.walk_lifetimes(body, &mut freed.clone(), function);
                }
                Stmt::ForEach { iterable, body, .. } => {
                    self.check_freed_uses(iterable, freed, function);
                    self.walk_lifetimes(body, &mut freed.clone(), function);
                }
                Stmt::Loop { body, .. } | Stmt::Unsafe { body, .. } => {
                    self.walk_lifetimes(body, &mut freed.clone(), function);
                }
                Stmt::Throw { value, .. } => {
                    self.check_freed_uses(value, freed, function);
                }
                Stmt::Try {
                    body, catch_body, ..
                } => {
                    self.walk_lifetimes(body, &mut freed.clone(), function);
                    self.walk_lifetimes(catch_body, &mut freed.clone(), function);
                }
                Stmt::Return(None)
                | Stmt::Break(_)
                | Stmt::Continue(_)
                | Stmt::Region(_)
                | Stmt::Asm { .. } => {}
            }
        }
    }

    /// Flag any use of a freed binding inside an expression.
    fn check_freed_uses(&mut self, expr: &Expr, freed: &HashSet<String>, function: &Function) {
        match &expr.kind {
            ExprKind::Variable(name) => {
                if freed.contains(name) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0350",
                        format!("`{name}` is used after it was freed"),
                        Some(function.name.clone()),
                        expr.span,
                    ));
                }
            }
            ExprKind::Array(values) => {
                for value in values {
                    self.check_freed_uses(value, freed, function);
                }
            }
            ExprKind::Index { target, index } => {
                self.check_freed_uses(target, freed, function);
                self.check_freed_uses(index, freed, function);
            }
            ExprKind::Field { target, .. } => self.check_freed_uses(target, freed, function),
            ExprKind::Await { expr } => self.check_freed_uses(expr, freed, function),
            ExprKind::Try(inner) => self.check_freed_uses(inner, freed, function),
            ExprKind::Unary { expr, .. } => self.check_freed_uses(expr, freed, function),
            ExprKind::Binary { left, right, .. } => {
                self.check_freed_uses(left, freed, function);
                self.check_freed_uses(right, freed, function);
            }
            ExprKind::Call { args, .. } => {
                for arg in args {
                    self.check_freed_uses(arg, freed, function);
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                for (_, value) in fields {
                    self.check_freed_uses(value, freed, function);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.check_freed_uses(scrutinee, freed, function);
                for arm in arms {
                    self.walk_lifetimes(&arm.body, &mut freed.clone(), function);
                }
            }
            // A closure body may reference a freed binding through capture, so
            // recurse into it under the same freed set.
            ExprKind::Closure { body, .. } => self.check_freed_uses(body, freed, function),
            ExprKind::Conditional {
                cond,
                then_branch,
                else_branch,
            } => {
                self.check_freed_uses(cond, freed, function);
                self.check_freed_uses(then_branch, freed, function);
                self.check_freed_uses(else_branch, freed, function);
            }
            ExprKind::In { value, collection } => {
                self.check_freed_uses(value, freed, function);
                self.check_freed_uses(collection, freed, function);
            }
            ExprKind::Slice { target, start, end } => {
                self.check_freed_uses(target, freed, function);
                if let Some(start) = start {
                    self.check_freed_uses(start, freed, function);
                }
                if let Some(end) = end {
                    self.check_freed_uses(end, freed, function);
                }
            }
            ExprKind::Integer(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::String(_)
            | ExprKind::Char(_) => {}
        }
    }

    /// Type-check an expression with no contextual expected type. This is the
    /// common path; every construct behaves exactly as before.
    fn check_expr(&mut self, expr: &Expr, scope: &Scope, function: &Function) -> Option<TypeRef> {
        self.check_expr_expected(expr, None, scope, function)
    }

    /// Type-check an expression, optionally against an expected type supplied by
    /// context (a `let` annotation, a `return`, or a function's final
    /// expression). Only the built-in `option`/`result` constructors consult
    /// `expected`; all other expressions ignore it, so existing code is
    /// unaffected.
    fn check_expr_expected(
        &mut self,
        expr: &Expr,
        expected: Option<&TypeRef>,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        // Built-in `option`/`result` construction is context-directed. Handle it
        // before the generic expression rules so `none`/`ok`/`err` can consult
        // the expected type. `some(v)` synthesizes without an expected type.
        if let Some(ty) = self.try_check_builtin_construction(expr, expected, scope, function) {
            if let Some(ty) = &ty {
                self.expression_types.push(ExpressionType {
                    function: function.name.clone(),
                    span: expr.span,
                    ty: ty.clone(),
                });
            }
            return ty;
        }
        let inferred = match &expr.kind {
            ExprKind::Integer(_) => Some(TypeRef::new("i64")),
            ExprKind::Float(_) => Some(TypeRef::new("f64")),
            ExprKind::Bool(_) => Some(TypeRef::new("bool")),
            ExprKind::String(_) => Some(TypeRef::new("string")),
            ExprKind::Char(_) => Some(TypeRef::new("char")),
            ExprKind::Array(values) => self.check_array_literal(values, scope, function),
            ExprKind::Variable(name) => match scope.locals.get(name) {
                Some(ty) => Some(ty.clone()),
                None => {
                    // A bare name that is not a local but is a known unit variant
                    // constructs that variant.
                    if let Some((enum_name, payload)) = self.variants.get(name).cloned() {
                        if payload.is_empty() {
                            Some(TypeRef::new(enum_name))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0381",
                                format!(
                                    "variant `{name}` of enum `{enum_name}` expects {} payload value(s) but was used as a unit variant",
                                    payload.len()
                                ),
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    } else if let Some(signature) = self.signatures.get(name) {
                        // A bare name that is not a local but is a declared
                        // top-level function evaluates to a function value of
                        // type `fn(params) -> ret`.
                        Some(function_type(&signature.params, &signature.return_type))
                    } else {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0306",
                            format!("unknown variable `{name}`"),
                            Some(function.name.clone()),
                            expr.span,
                        ));
                        None
                    }
                }
            },
            ExprKind::Index { target, index } => {
                let target_type = self.check_expr(target, scope, function);
                let index_type = self.check_expr(index, scope, function);
                if index_type.as_ref() != Some(&TypeRef::new("i64")) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0326",
                        "array index expression must be i64",
                        Some(function.name.clone()),
                        index.span,
                    ));
                }

                // Indexing a `string` reads the char at that position (`s[i] ->
                // char`, bounds-checked at runtime); indexing an array yields its
                // element type.
                if target_type.as_ref().map(|ty| ty.name.as_str()) == Some("string") {
                    Some(TypeRef::new("char"))
                } else {
                    match target_type.and_then(|ty| ty.array_element()) {
                        Some(element_type) => Some(element_type),
                        None => {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0325",
                                "index target must be an array or string",
                                Some(function.name.clone()),
                                target.span,
                            ));
                            None
                        }
                    }
                }
            }
            ExprKind::Unary { op, expr } => {
                let expr_type = self.check_expr(expr, scope, function);
                match op {
                    UnaryOp::Not => {
                        if expr_type.as_ref() == Some(&TypeRef::new("bool")) {
                            Some(TypeRef::new("bool"))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0319",
                                "`not` operand must be bool",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                    UnaryOp::BitNot => {
                        // Bitwise NOT applies to any integer type (`i64` or a
                        // fixed-width kind) and preserves it. `f32`/`f64` are not
                        // integers, so they are rejected.
                        match &expr_type {
                            Some(ty) if ty.name == "i64" || is_fixed_width_int_name(&ty.name) => {
                                Some(ty.clone())
                            }
                            _ => {
                                self.diagnostics.push(SemanticDiagnostic::at(
                                    "L0307",
                                    "operand of `~` must be an integer",
                                    Some(function.name.clone()),
                                    expr.span,
                                ));
                                None
                            }
                        }
                    }
                    UnaryOp::Negate => {
                        // Arithmetic negation applies to any numeric type (`i64`,
                        // `f64`, `f32`, or a fixed-width integer) and preserves it.
                        match &expr_type {
                            Some(ty)
                                if ty.name == "i64"
                                    || ty.name == "f64"
                                    || ty.name == "f32"
                                    || is_fixed_width_int_name(&ty.name) =>
                            {
                                Some(ty.clone())
                            }
                            _ => {
                                self.diagnostics.push(SemanticDiagnostic::at(
                                    "L0307",
                                    "operand of unary `-` must be numeric",
                                    Some(function.name.clone()),
                                    expr.span,
                                ));
                                None
                            }
                        }
                    }
                }
            }
            ExprKind::Binary { left, op, right } => {
                let left_type = self.check_expr(left, scope, function);
                let right_type = self.check_expr(right, scope, function);
                let same_numeric = same_numeric_type(&left_type, &right_type);
                match op {
                    BinaryOp::Add => {
                        let string_type = TypeRef::new("string");
                        let char_type = TypeRef::new("char");
                        let is_str = |t: &Option<TypeRef>| t.as_ref() == Some(&string_type);
                        let is_chr = |t: &Option<TypeRef>| t.as_ref() == Some(&char_type);
                        if let Some(numeric) = same_numeric {
                            Some(numeric)
                        } else if is_str(&left_type) && is_str(&right_type) {
                            // `+` concatenates two strings.
                            Some(string_type)
                        } else if (is_str(&left_type) && is_chr(&right_type))
                            || (is_chr(&left_type) && is_str(&right_type))
                        {
                            // `string + char` (either order) concatenates the
                            // char as a one-character string; lowering coerces the
                            // char via `to_string`.
                            Some(string_type)
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0307",
                                "operands of `+` must both be the same numeric type, both be string, or be a string and a char",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                    BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide => {
                        if let Some(numeric) = same_numeric {
                            Some(numeric)
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0307",
                                "arithmetic operands must both be the same numeric type",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                    BinaryOp::Remainder => {
                        // `%` is integer remainder only (like `& | ^ << >>`):
                        // both operands must be the same integer type and the
                        // result is that type. Floats are rejected (use integer
                        // arithmetic, or a future `fmod` builtin).
                        let is_integer = |ty: &Option<TypeRef>| {
                            ty.as_ref().is_some_and(|t| {
                                t.name == "i64" || is_fixed_width_int_name(&t.name)
                            })
                        };
                        if left_type == right_type
                            && is_integer(&left_type)
                            && is_integer(&right_type)
                        {
                            left_type
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0307",
                                "operands of `%` must both be the same integer type",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                    BinaryOp::Equal | BinaryOp::NotEqual => {
                        if left_type.is_some() && left_type == right_type {
                            Some(TypeRef::new("bool"))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0308",
                                "comparison operands must have the same type",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                    BinaryOp::Less
                    | BinaryOp::LessEqual
                    | BinaryOp::Greater
                    | BinaryOp::GreaterEqual => {
                        // Ordering compares two values of the same numeric type
                        // (i64, f64, i32, u32), two chars (by code point), two
                        // bytes (numerically), or two strings (lexicographically
                        // by Unicode code point).
                        let both_string = left_type.as_ref().map(|t| t.name.as_str())
                            == Some("string")
                            && right_type.as_ref().map(|t| t.name.as_str()) == Some("string");
                        if same_numeric.is_some()
                            || same_orderable_scalar(&left_type, &right_type)
                            || both_string
                        {
                            Some(TypeRef::new("bool"))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0327",
                                "ordering comparison operands must both be the same numeric type, both be char, both be byte, or both be string",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                    BinaryOp::And | BinaryOp::Or => {
                        if left_type.as_ref() == Some(&TypeRef::new("bool"))
                            && right_type.as_ref() == Some(&TypeRef::new("bool"))
                        {
                            Some(TypeRef::new("bool"))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0320",
                                "logical operands must both be bool",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                    BinaryOp::BitAnd
                    | BinaryOp::BitOr
                    | BinaryOp::BitXor
                    | BinaryOp::Shl
                    | BinaryOp::Shr => {
                        // Bitwise ops require two operands of the same integer type
                        // (`i64` or a fixed-width kind) and produce that type. The
                        // shift amount shares the operand's type (no width mixing);
                        // per-width masking and logical-vs-arithmetic right shift
                        // happen at runtime.
                        let is_integer = |ty: &Option<TypeRef>| {
                            ty.as_ref().is_some_and(|t| {
                                t.name == "i64" || is_fixed_width_int_name(&t.name)
                            })
                        };
                        if left_type == right_type
                            && is_integer(&left_type)
                            && is_integer(&right_type)
                        {
                            left_type
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0307",
                                "bitwise operands (`& | ^ << >>`) must both be the same integer type",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                }
            }
            ExprKind::Call { name, args } => {
                // A call whose name is a local variable dispatches through the
                // held value: a function-typed local calls the referenced
                // function; any other local is not callable (`L0390`).
                if let Some(local_type) = scope.locals.get(name).cloned() {
                    self.check_call_through_local(
                        name,
                        &local_type,
                        args,
                        expr.span,
                        scope,
                        function,
                    )
                } else if self.variants.contains_key(name) {
                    self.check_enum_construction(name, args, expr.span, scope, function)
                } else if self.structs.contains_key(name) {
                    self.check_struct_construction(name, args, expr.span, scope, function)
                } else {
                    self.check_call(name, args, expr.span, expected, scope, function)
                }
            }
            ExprKind::StructLiteral { name, fields } => {
                self.check_struct_literal(name, fields, expr.span, scope, function)
            }
            ExprKind::Field { target, field } => {
                let target_type = self.check_expr(target, scope, function)?;
                match self.structs.get(&target_type.name) {
                    Some(fields) => match fields.iter().find(|f| &f.name == field) {
                        Some(matched) => Some(matched.ty.clone()),
                        None => {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0371",
                                format!("struct `{}` has no field `{field}`", target_type.name),
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    },
                    None => {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0371",
                            format!(
                                "cannot access field `{field}` on non-struct type `{}`",
                                target_type.name
                            ),
                            Some(function.name.clone()),
                            expr.span,
                        ));
                        None
                    }
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.check_match(scrutinee, arms, expr.span, scope, function)
            }
            ExprKind::Await { expr: inner } => {
                // `await e` requires `e: Future<T>` and produces `T`. Awaiting a
                // non-future (an ordinary value or synchronous call) is `L0344`.
                let inner_type = self.check_expr(inner, scope, function)?;
                match future_inner(&inner_type) {
                    Some(result_type) => Some(result_type),
                    None => {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0344",
                            format!(
                                "`await` expects a `Future<T>` but got `{}`; only the result of calling an `async fn` can be awaited",
                                inner_type.name
                            ),
                            Some(function.name.clone()),
                            expr.span,
                        ));
                        None
                    }
                }
            }
            ExprKind::Try(inner) => self.check_try(inner, expr.span, scope, function),
            // A closure literal `fn PARAMS -> BODY` type-checks in a child scope
            // that layers its parameters over the enclosing locals, so the body
            // can read captured locals (and parameters shadow them). Its value
            // type is `fn(param types) -> typeof(BODY)` in the canonical spelling,
            // so it unifies structurally with any expected `fn(...) -> ...` type
            // (a function-typed `let`, an `apply`/`parallel_map` argument, a
            // returned function value).
            ExprKind::Closure { params, body, .. } => {
                let mut body_scope = scope.clone();
                for param in params {
                    body_scope
                        .locals
                        .insert(param.name.clone(), param.ty.clone());
                }
                let body_type = self.check_expr(body, &body_scope, function)?;
                let param_types: Vec<TypeRef> =
                    params.iter().map(|param| param.ty.clone()).collect();
                Some(function_type(&param_types, &body_type))
            }
            // Inline conditional `THEN if COND else ELSE`: the condition must be
            // `bool` (`L0305`, shared with `if`/`while`), and both branches must
            // have the same type (`L0435`), which becomes the expression's type.
            // The contextual `expected` type flows into both branches so
            // `none`/`ok`/`err` branches resolve to the right `option`/`result`.
            ExprKind::Conditional {
                cond,
                then_branch,
                else_branch,
            } => {
                let cond_type = self.check_expr(cond, scope, function);
                if cond_type.as_ref() != Some(&TypeRef::new("bool")) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0305",
                        "the condition of an inline conditional must be bool",
                        Some(function.name.clone()),
                        cond.span,
                    ));
                }
                let then_type = self.check_expr_expected(then_branch, expected, scope, function);
                let else_type = self.check_expr_expected(else_branch, expected, scope, function);
                match (then_type, else_type) {
                    (Some(then_ty), Some(else_ty)) => {
                        if then_ty != else_ty {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0435",
                                format!(
                                    "the branches of an inline conditional must have the same type, but the `then` branch is `{}` and the `else` branch is `{}`",
                                    then_ty.name, else_ty.name
                                ),
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        } else if !is_inline_conditional_type(&then_ty) {
                            // The IR desugar hoists a zero-initialized temporary,
                            // which only has a well-typed zero for scalars and
                            // `string`. Reject aggregate/heap results with a clear
                            // message; an `if` statement selects those instead.
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0436",
                                format!(
                                    "an inline conditional currently supports scalar and `string` result types, but these branches are `{}`; use an `if` statement to select a `{}` value",
                                    then_ty.name, then_ty.name
                                ),
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        } else {
                            Some(then_ty)
                        }
                    }
                    _ => None,
                }
            }
            // Membership `VALUE in COLLECTION` yields `bool`. The collection is a
            // `string` (value is a `char`/`string`) or a `list<T>` (value is a
            // `T`); anything else is `L0437`.
            ExprKind::In { value, collection } => {
                let coll_type = self.check_expr(collection, scope, function);
                match coll_type.as_ref() {
                    Some(ct) if ct.name == "string" => {
                        let value_type = self.check_expr(value, scope, function);
                        let ok = matches!(
                            value_type.as_ref().map(|t| t.name.as_str()),
                            Some("string") | Some("char")
                        );
                        if !ok {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0437",
                                format!(
                                    "the left operand of `in` on a string must be a `char` or `string`, but got `{}`",
                                    value_type.as_ref().map_or("?", |t| t.name.as_str())
                                ),
                                Some(function.name.clone()),
                                value.span,
                            ));
                        }
                        Some(TypeRef::new("bool"))
                    }
                    Some(ct) if ct.generic_arg("list").is_some() => {
                        let element = ct.generic_arg("list").expect("list element");
                        let value_type =
                            self.check_expr_expected(value, Some(&element), scope, function);
                        if value_type.as_ref() != Some(&element) {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0437",
                                format!(
                                    "the left operand of `in` on a `{}` must be `{}`, but got `{}`",
                                    ct.name,
                                    element.name,
                                    value_type.as_ref().map_or("?", |t| t.name.as_str())
                                ),
                                Some(function.name.clone()),
                                value.span,
                            ));
                        }
                        Some(TypeRef::new("bool"))
                    }
                    other => {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0437",
                            format!(
                                "`in` needs a `string` or `list<T>` collection, but got `{}`",
                                other.map_or("?", |t| t.name.as_str())
                            ),
                            Some(function.name.clone()),
                            collection.span,
                        ));
                        None
                    }
                }
            }
            // String slice `target[start:end]` yields a `string`. The target
            // must be a `string` and each present bound must be `i64` (`L0438`).
            ExprKind::Slice { target, start, end } => {
                let target_type = self.check_expr(target, scope, function);
                if target_type.as_ref().map(|t| t.name.as_str()) != Some("string") {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0438",
                        format!(
                            "a slice `target[start:end]` requires a `string` target, but got `{}`",
                            target_type.as_ref().map_or("?", |t| t.name.as_str())
                        ),
                        Some(function.name.clone()),
                        target.span,
                    ));
                }
                for bound in [start, end].into_iter().flatten() {
                    let bound_type = self.check_expr(bound, scope, function);
                    if bound_type.as_ref().map(|t| t.name.as_str()) != Some("i64") {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0438",
                            format!(
                                "a slice bound must be `i64`, but got `{}`",
                                bound_type.as_ref().map_or("?", |t| t.name.as_str())
                            ),
                            Some(function.name.clone()),
                            bound.span,
                        ));
                    }
                }
                Some(TypeRef::new("string"))
            }
        };

        if let Some(ty) = &inferred {
            self.expression_types.push(ExpressionType {
                function: function.name.clone(),
                span: expr.span,
                ty: ty.clone(),
            });
        }

        inferred
    }

    /// If `expr` constructs a built-in `option`/`result` value, type-check it
    /// against the contextual `expected` type and return `Some(result)`.
    /// Otherwise return `None` so the caller falls through to the generic rules.
    ///
    /// - `some(v)` → `option<typeof v>`; if `expected = option<U>`, require
    ///   `typeof v == U`.
    /// - `none` → requires `expected = option<U>` (else `L0386`).
    /// - `ok(v)`/`err(v)` → require `expected = result<T, E>` and pin `v` to
    ///   `T`/`E` respectively (else `L0386`).
    ///
    /// A payload whose type disagrees with the expected type is `L0303`.
    fn try_check_builtin_construction(
        &mut self,
        expr: &Expr,
        expected: Option<&TypeRef>,
        scope: &Scope,
        function: &Function,
    ) -> Option<Option<TypeRef>> {
        match &expr.kind {
            // `none` is a bare name. A local of the same name would shadow it,
            // matching how user unit-variant construction defers to locals.
            ExprKind::Variable(name) if name == "none" && !scope.locals.contains_key(name) => {
                Some(self.check_option_none(expected, expr.span, function))
            }
            ExprKind::Call { name, args } if name == "some" => {
                Some(self.check_option_some(args, expected, expr.span, scope, function))
            }
            ExprKind::Call { name, args } if name == "ok" || name == "err" => Some(
                self.check_result_construction(name, args, expected, expr.span, scope, function),
            ),
            // `list_new()` has no argument to infer `T` from, so its element type
            // comes from the contextual expected `list<...>` type, exactly like
            // `none`/`ok`/`err` take theirs.
            ExprKind::Call { name, args } if name == "list_new" => {
                Some(self.check_list_new(args, expected, expr.span, function))
            }
            // `map_new()` has no argument to infer `K`/`V` from, so its key and
            // value types come from the contextual expected `map<...>` type.
            ExprKind::Call { name, args } if name == "map_new" => {
                Some(self.check_map_new(args, expected, expr.span, function))
            }
            _ => None,
        }
    }

    /// Type-check a postfix `EXPR?` error-propagation operator.
    ///
    /// The operand must be an `option<T>` or a `result<T, E>` (else `L0428`).
    /// The enclosing function's return type must be a *compatible* propagation
    /// target: for a `result<T, E>` operand the function must return
    /// `result<U, E>` — the SAME error type `E` (a wrong `E` is `L0429`); for an
    /// `option<T>` operand it must return `option<U>`. A return type that is not
    /// a matching `result`/`option` at all is `L0427`. The expression's type is
    /// the payload `T`.
    fn check_try(
        &mut self,
        inner: &Expr,
        span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let operand_type = self.check_expr(inner, scope, function)?;
        let return_type = &function.return_type;
        if let Some((ok_ty, err_ty)) = operand_type.result_args() {
            // A `result<T, E>` operand requires a `result<U, E>` return type.
            match return_type.result_args() {
                Some((_, return_err)) if return_err == err_ty => Some(ok_ty),
                Some((_, return_err)) => {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0429",
                        format!(
                            "`?` on a `{}` requires the enclosing function to return a `result` with the same error type `{}`, but `{}` returns error type `{}`",
                            operand_type.name, err_ty.name, function.name, return_err.name
                        ),
                        Some(function.name.clone()),
                        span,
                    ));
                    None
                }
                None => {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0427",
                        format!(
                            "`?` on a `{}` requires the enclosing function `{}` to return a `result<..., {}>`, but it returns `{}`",
                            operand_type.name, function.name, err_ty.name, return_type.name
                        ),
                        Some(function.name.clone()),
                        span,
                    ));
                    None
                }
            }
        } else if let Some(payload) = operand_type.option_element() {
            // An `option<T>` operand requires an `option<U>` return type.
            if return_type.option_element().is_some() {
                Some(payload)
            } else {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0427",
                    format!(
                        "`?` on a `{}` requires the enclosing function `{}` to return an `option<...>`, but it returns `{}`",
                        operand_type.name, function.name, return_type.name
                    ),
                    Some(function.name.clone()),
                    span,
                ));
                None
            }
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0428",
                format!(
                    "`?` can only be applied to an `option` or `result` value, but the operand has type `{}`",
                    operand_type.name
                ),
                Some(function.name.clone()),
                span,
            ));
            None
        }
    }

    fn check_list_new(
        &mut self,
        args: &[Expr],
        expected: Option<&TypeRef>,
        span: Span,
        function: &Function,
    ) -> Option<TypeRef> {
        if !args.is_empty() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0387",
                format!("`list_new` expects 0 arguments but got {}", args.len()),
                Some(function.name.clone()),
                span,
            ));
            return None;
        }
        match expected {
            Some(ty) if list_element(ty).is_some() => Some(ty.clone()),
            Some(ty) => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0387",
                    format!("`list_new` has `list` type but `{}` was expected", ty.name),
                    Some(function.name.clone()),
                    span,
                ));
                None
            }
            None => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0387",
                    "cannot infer the element type of `list_new`; add a `list<...>` annotation or return type",
                    Some(function.name.clone()),
                    span,
                ));
                None
            }
        }
    }

    fn check_map_new(
        &mut self,
        args: &[Expr],
        expected: Option<&TypeRef>,
        span: Span,
        function: &Function,
    ) -> Option<TypeRef> {
        if !args.is_empty() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0388",
                format!("`map_new` expects 0 arguments but got {}", args.len()),
                Some(function.name.clone()),
                span,
            ));
            return None;
        }
        match expected {
            Some(ty) => match map_kv(ty) {
                Some((key, _value)) if map_key_ok(&key) => Some(ty.clone()),
                Some((key, _value)) => {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0388",
                        format!(
                            "`map` keys must be `i64` or `string` but got `{}`",
                            key.name
                        ),
                        Some(function.name.clone()),
                        span,
                    ));
                    None
                }
                None => {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0388",
                        format!("`map_new` has `map` type but `{}` was expected", ty.name),
                        Some(function.name.clone()),
                        span,
                    ));
                    None
                }
            },
            None => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0388",
                    "cannot infer the key/value types of `map_new`; add a `map<...>` annotation or return type",
                    Some(function.name.clone()),
                    span,
                ));
                None
            }
        }
    }

    fn check_option_some(
        &mut self,
        args: &[Expr],
        expected: Option<&TypeRef>,
        span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        if args.len() != 1 {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0381",
                format!("`some` expects 1 payload value but got {}", args.len()),
                Some(function.name.clone()),
                span,
            ));
            for arg in args {
                self.check_expr(arg, scope, function);
            }
            return None;
        }
        // The expected `option<U>` payload type, if any, guides nested inference.
        let expected_payload = expected.and_then(|ty| ty.option_element());
        let value_type =
            self.check_expr_expected(&args[0], expected_payload.as_ref(), scope, function)?;
        if let Some(expected_ty) = expected {
            match expected_ty.option_element() {
                Some(payload) => {
                    if payload != value_type {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0303",
                            format!(
                                "`some` payload has `{}` but `{}` expects `{}`",
                                value_type.name, expected_ty.name, payload.name
                            ),
                            Some(function.name.clone()),
                            args[0].span,
                        ));
                    }
                    return Some(expected_ty.clone());
                }
                None => {
                    // Expected a non-option type; report the mismatch and still
                    // synthesize the natural `option<typeof v>`.
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0303",
                        format!(
                            "`some(...)` has `option` type but `{}` was expected",
                            expected_ty.name
                        ),
                        Some(function.name.clone()),
                        span,
                    ));
                }
            }
        }
        Some(option_type(&value_type))
    }

    fn check_option_none(
        &mut self,
        expected: Option<&TypeRef>,
        span: Span,
        function: &Function,
    ) -> Option<TypeRef> {
        match expected {
            Some(ty) if ty.option_element().is_some() => Some(ty.clone()),
            Some(ty) => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0303",
                    format!("`none` has `option` type but `{}` was expected", ty.name),
                    Some(function.name.clone()),
                    span,
                ));
                None
            }
            None => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0386",
                    "cannot infer the type of `none`; add an `option<...>` annotation or return type",
                    Some(function.name.clone()),
                    span,
                ));
                None
            }
        }
    }

    fn check_result_construction(
        &mut self,
        name: &str,
        args: &[Expr],
        expected: Option<&TypeRef>,
        span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        if args.len() != 1 {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0381",
                format!("`{name}` expects 1 payload value but got {}", args.len()),
                Some(function.name.clone()),
                span,
            ));
            for arg in args {
                self.check_expr(arg, scope, function);
            }
            return None;
        }
        let Some((ok_ty, err_ty)) = expected.and_then(|ty| ty.result_args()) else {
            // Without an expected `result<...>` we cannot pin the sibling type.
            self.check_expr(&args[0], scope, function);
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0386",
                format!(
                    "cannot infer the type of `{name}`; add a `result<...>` annotation or return type"
                ),
                Some(function.name.clone()),
                span,
            ));
            return None;
        };
        let pinned = if name == "ok" { &ok_ty } else { &err_ty };
        let value_type = self.check_expr_expected(&args[0], Some(pinned), scope, function);
        if value_type.as_ref() != Some(pinned) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0303",
                format!(
                    "`{name}` payload expects `{}` but got `{}`",
                    pinned.name,
                    value_type
                        .as_ref()
                        .map(|ty| ty.name.as_str())
                        .unwrap_or("<unknown>")
                ),
                Some(function.name.clone()),
                args[0].span,
            ));
        }
        Some(result_type(&ok_ty, &err_ty))
    }

    fn check_array_literal(
        &mut self,
        values: &[Expr],
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let Some((first, rest)) = values.split_first() else {
            self.diagnostics.push(SemanticDiagnostic::new(
                "L0323",
                "array literals must contain at least one value",
                Some(function.name.clone()),
            ));
            return None;
        };

        let element_type = self.check_expr(first, scope, function)?;
        for value in rest {
            let value_type = self.check_expr(value, scope, function);
            if value_type.as_ref() != Some(&element_type) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0324",
                    "array literal values must all have the same type",
                    Some(function.name.clone()),
                    value.span,
                ));
            }
        }

        Some(TypeRef::new(format!("array<{}>", element_type.name)))
    }

    /// Check a call whose name resolves to a local variable. A function-typed
    /// local (`fn(A...) -> R`) is invoked: argument arity and types are checked
    /// against the function-type parameters and the call yields `R`. A local
    /// that is not of function type is not callable and reports `L0390`.
    fn check_call_through_local(
        &mut self,
        name: &str,
        local_type: &TypeRef,
        args: &[Expr],
        call_span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let Some((params, return_type)) = local_type.function_signature() else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0390",
                format!(
                    "local `{name}` has type `{}`, which is not a function and cannot be called",
                    local_type.name
                ),
                Some(function.name.clone()),
                call_span,
            ));
            return None;
        };

        if params.len() != args.len() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0312",
                format!(
                    "function value `{name}` expects {} arguments but got {}",
                    params.len(),
                    args.len()
                ),
                Some(function.name.clone()),
                call_span,
            ));
            return None;
        }

        for (index, (arg, expected)) in args.iter().zip(params.iter()).enumerate() {
            // Propagate the function value's declared parameter type so a nested
            // context-directed constructor in argument position infers from it.
            let actual = self.check_expr_expected(arg, Some(expected), scope, function);
            if actual.as_ref() != Some(expected) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0313",
                    format!(
                        "argument {} for `{name}` must be `{}` but got `{}`",
                        index + 1,
                        expected.name,
                        actual
                            .as_ref()
                            .map(|ty| ty.name.as_str())
                            .unwrap_or("<unknown>")
                    ),
                    Some(function.name.clone()),
                    arg.span,
                ));
            }
        }

        Some(return_type)
    }

    /// Type-check a call. `expected` is the contextual expected type of the whole
    /// call expression (from a `let` annotation, a `return`, or an enclosing
    /// call's parameter type). It is used for argument-position inference: a
    /// collection-growing builtin (`push`/`set`/`pop`/`map_set`/`map_del`) whose
    /// result type equals the container type propagates `expected` into its
    /// container argument, so a nested `list_new()`/`map_new()` there infers its
    /// element/key/value types; the resolved element/key/value type is then
    /// propagated into the value arguments so a nested `none`/`ok`/`err` infers
    /// too. User-function calls propagate each concrete parameter type similarly.
    fn check_call(
        &mut self,
        name: &str,
        args: &[Expr],
        call_span: Span,
        expected: Option<&TypeRef>,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        // A trait-method call (`recv.method(...)` desugared to `method(recv,...)`)
        // takes priority over the free-function/builtin paths: trait-method and
        // free-function namespaces are disjoint.
        if let Some(trait_name) = self.trait_methods.get(name).cloned() {
            return self.check_trait_method_call(
                name,
                &trait_name,
                args,
                call_span,
                scope,
                function,
            );
        }
        match name {
            "alloc" => {
                self.expect_arg_count(name, args, 1, function)?;
                let value_type = self.check_expr(&args[0], scope, function)?;
                Some(TypeRef::new(format!("ptr_{}", value_type.name)))
            }
            "load" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ptr_type = self.check_expr(&args[0], scope, function)?;
                ptr_type
                    .name
                    .strip_prefix("ptr_")
                    .map(TypeRef::new)
                    .or_else(|| {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0310",
                            "load expects a pointer argument",
                            Some(function.name.clone()),
                            args[0].span,
                        ));
                        None
                    })
            }
            "store" => {
                self.expect_arg_count(name, args, 2, function)?;
                let ptr_type = self.check_expr(&args[0], scope, function)?;
                let value_type = self.check_expr(&args[1], scope, function)?;
                let Some(expected) = ptr_type.name.strip_prefix("ptr_").map(TypeRef::new) else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0310",
                        "store expects a pointer as its first argument",
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    return None;
                };
                if value_type != expected {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0328",
                        format!(
                            "store expects value `{}` for pointer `{}` but got `{}`",
                            expected.name, ptr_type.name, value_type.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                Some(TypeRef::new("void"))
            }
            "dealloc" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ptr_type = self.check_expr(&args[0], scope, function)?;
                if ptr_type.name.starts_with("ptr_") {
                    Some(TypeRef::new("void"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0311",
                        "dealloc expects a pointer argument",
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "read_file" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_arg_type(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("string"))
            }
            "write_file" | "append_file" => {
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_arg_type(name, 1, &args[0], "string", scope, function)?;
                self.expect_arg_type(name, 2, &args[1], "string", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "file_exists" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_arg_type(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("bool"))
            }
            "read_lines" | "list_dir" => {
                self.expect_fs_arg_count(name, args, 1, function)?;
                self.expect_fs_arg_type(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("list<string>"))
            }
            "read_bytes" => {
                self.expect_fs_arg_count(name, args, 1, function)?;
                self.expect_fs_arg_type(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("list<byte>"))
            }
            "write_bytes" => {
                self.expect_fs_arg_count(name, args, 2, function)?;
                self.expect_fs_arg_type(name, 1, &args[0], "string", scope, function)?;
                self.expect_fs_arg_type(name, 2, &args[1], "list<byte>", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "file_size" => {
                self.expect_fs_arg_count(name, args, 1, function)?;
                self.expect_fs_arg_type(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            "is_file" | "is_dir" => {
                self.expect_fs_arg_count(name, args, 1, function)?;
                self.expect_fs_arg_type(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("bool"))
            }
            "make_dir" | "remove_file" | "remove_dir" => {
                self.expect_fs_arg_count(name, args, 1, function)?;
                self.expect_fs_arg_type(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "sys_status" | "sys_output" => {
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_arg_type(name, 1, &args[0], "string", scope, function)?;
                self.expect_arg_type(name, 2, &args[1], "array<string>", scope, function)?;
                Some(TypeRef::new(if name == "sys_status" {
                    "i64"
                } else {
                    "string"
                }))
            }
            "print" | "println" | "warn" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_arg_type(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "wasm_log" => {
                // `wasm_log(x i64) -> void`: a host log call. On the interpreters
                // it prints the value as a stdout line so cross-backend parity
                // holds; on the WASM backend it lowers to a `call` of the imported
                // host function `env.log_i64`.
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_arg_type(name, 1, &args[0], "i64", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "console_log" => {
                // `console_log(s string) -> void`: a JS/DOM host call. On the
                // interpreters it prints the string as a stdout line so
                // cross-backend parity holds; on the WASM backend it lowers to a
                // `call` of the imported host function `env.console_log(ptr, len)`.
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_arg_type(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "dom_set_text" => {
                // `dom_set_text(id string, text string) -> void`: the DOM-write
                // primitive. On the interpreters it prints a deterministic
                // `id=text` line so cross-backend parity holds; on the WASM backend
                // it lowers to a `call` of the imported host function
                // `env.dom_set_text(id_ptr, id_len, text_ptr, text_len)`.
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_arg_type(name, 1, &args[0], "string", scope, function)?;
                self.expect_arg_type(name, 2, &args[1], "string", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "flush" => {
                self.expect_arg_count(name, args, 0, function)?;
                Some(TypeRef::new("void"))
            }
            "mono_now" => {
                // `mono_now() -> i64`: a monotonic clock in nanoseconds since a
                // fixed per-process baseline. Non-decreasing within a run.
                self.expect_arg_count(name, args, 0, function)?;
                Some(TypeRef::new("i64"))
            }
            "wall_now" => {
                // `wall_now() -> i64`: wall-clock time as milliseconds since the
                // Unix epoch.
                self.expect_arg_count(name, args, 0, function)?;
                Some(TypeRef::new("i64"))
            }
            "sleep_millis" => {
                // `sleep_millis(ms i64) -> void`: sleep the current thread for
                // `ms` milliseconds; a negative `ms` sleeps for zero.
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_arg_type(name, 1, &args[0], "i64", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "assert" => {
                self.expect_arg_count(name, args, 1, function)?;
                let arg_type = self.check_expr(&args[0], scope, function)?;
                if arg_type.name == "bool" {
                    Some(TypeRef::new("void"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0342",
                        format!("assert expects a bool argument but got `{}`", arg_type.name),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "to_string" => {
                self.expect_arg_count(name, args, 1, function)?;
                let arg_type = self.check_expr(&args[0], scope, function)?;
                // Every scalar renders: the full numeric lattice plus bool,
                // string, char, and byte.
                if is_numeric_type_name(&arg_type.name)
                    || matches!(arg_type.name.as_str(), "bool" | "string" | "char" | "byte")
                {
                    Some(TypeRef::new("string"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0313",
                        format!(
                            "to_string expects a scalar value (a numeric type, bool, string, char, or byte) but got `{}`",
                            arg_type.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "len" => {
                self.expect_arg_count(name, args, 1, function)?;
                let arg_type = self.check_expr(&args[0], scope, function)?;
                if arg_type.name == "string"
                    || arg_type.array_element().is_some()
                    || list_element(&arg_type).is_some()
                {
                    Some(TypeRef::new("i64"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0373",
                        format!(
                            "len expects a string, array, or list value but got `{}`",
                            arg_type.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "array_fill" => {
                // `array_fill(n i64, value T) -> array<T>`: a runtime-sized array
                // with every element `value`. The element type is inferred from
                // the value argument.
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_arg_type(name, 1, &args[0], "i64", scope, function)?;
                let element = self.check_expr(&args[1], scope, function)?;
                Some(TypeRef::new(format!("array<{}>", element.name)))
            }
            "push" => {
                self.expect_arg_count(name, args, 2, function)?;
                // `push` returns `list<T>`, so the outer expected `list<T>` flows
                // into the list argument (inferring a nested `list_new()`), and
                // the resolved element type flows into the value argument.
                let list_ty = self.check_expr_expected(&args[0], expected, scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                let value_type =
                    self.check_expr_expected(&args[1], Some(&element), scope, function)?;
                if value_type != element {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0387",
                        format!(
                            "`push` element must be `{}` but got `{}`",
                            element.name, value_type.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                Some(list_type(&element))
            }
            "get" => {
                self.expect_arg_count(name, args, 2, function)?;
                let list_ty = self.check_expr(&args[0], scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                self.expect_arg_type(name, 2, &args[1], "i64", scope, function)?;
                Some(element)
            }
            "list_index_of" | "list_contains" => {
                self.expect_arg_count(name, args, 2, function)?;
                let list_ty = self.check_expr(&args[0], scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                let value_type =
                    self.check_expr_expected(&args[1], Some(&element), scope, function)?;
                if value_type != element {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0387",
                        format!(
                            "`{name}` search value must be `{}` but got `{}`",
                            element.name, value_type.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                Some(TypeRef::new(if name == "list_index_of" {
                    "i64"
                } else {
                    "bool"
                }))
            }
            "set" => {
                self.expect_arg_count(name, args, 3, function)?;
                let list_ty = self.check_expr_expected(&args[0], expected, scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                self.expect_arg_type(name, 2, &args[1], "i64", scope, function)?;
                let value_type =
                    self.check_expr_expected(&args[2], Some(&element), scope, function)?;
                if value_type != element {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0387",
                        format!(
                            "`set` element must be `{}` but got `{}`",
                            element.name, value_type.name
                        ),
                        Some(function.name.clone()),
                        args[2].span,
                    ));
                    return None;
                }
                Some(list_type(&element))
            }
            "pop" => {
                self.expect_arg_count(name, args, 1, function)?;
                let list_ty = self.check_expr_expected(&args[0], expected, scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                Some(list_type(&element))
            }
            "reverse" => {
                self.expect_arg_count(name, args, 1, function)?;
                let list_ty = self.check_expr_expected(&args[0], expected, scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                Some(list_type(&element))
            }
            "sort" => {
                // `sort(l list<T>) -> list<T>`: ascending sort over a scalar list.
                // Accepts `i64`, `f64` (total order via `total_cmp`), and `string`
                // (lexicographic); other element types are rejected with `L0387`.
                self.expect_arg_count(name, args, 1, function)?;
                let list_ty = self.check_expr_expected(&args[0], expected, scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                if !matches!(element.name.as_str(), "i64" | "f64" | "string") {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0387",
                        format!(
                            "`sort` expects a `list<i64>`, `list<f64>`, or `list<string>` but got `list<{}>`",
                            element.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    return None;
                }
                Some(list_type(&element))
            }
            "sort_by" => {
                // `sort_by(l list<T>, cmp fn(T, T) -> i64) -> list<T>`: a stable
                // sort ordered by the comparator (`cmp(a, b)` negative if `a`
                // precedes `b`, zero if equal, positive if after). `T` is the
                // element type and the comparator must be `fn(T, T) -> i64`.
                self.expect_arg_count(name, args, 2, function)?;
                let list_ty = self.check_expr(&args[0], scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                self.expect_fn_arg(
                    name,
                    2,
                    &args[1],
                    (
                        &[element.clone(), element.clone()],
                        Some(&TypeRef::new("i64")),
                    ),
                    scope,
                    function,
                )?;
                Some(list_type(&element))
            }
            "concat" => {
                self.expect_arg_count(name, args, 2, function)?;
                // `concat` returns `list<T>`, so the outer expected `list<T>`
                // flows into the first list argument (inferring a nested
                // `list_new()`); the resolved element type then flows into `b`.
                let a_ty = self.check_expr_expected(&args[0], expected, scope, function)?;
                let element = self.expect_list_arg(name, &a_ty, args[0].span, function)?;
                let b_ty = self.check_expr_expected(
                    &args[1],
                    Some(&list_type(&element)),
                    scope,
                    function,
                )?;
                let b_element = self.expect_list_arg(name, &b_ty, args[1].span, function)?;
                if b_element != element {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0387",
                        format!(
                            "`concat` requires both lists to have the same element type, but got `{}` and `{}`",
                            element.name, b_element.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                Some(list_type(&element))
            }
            "slice" => {
                self.expect_arg_count(name, args, 3, function)?;
                let list_ty = self.check_expr_expected(&args[0], expected, scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                self.expect_arg_type(name, 2, &args[1], "i64", scope, function)?;
                self.expect_arg_type(name, 3, &args[2], "i64", scope, function)?;
                Some(list_type(&element))
            }
            "list_map" => {
                // `list_map(l list<T>, f fn(T) -> U) -> list<U>`: apply `f` to
                // each element in order, returning the mapped `list<U>`. `U` is
                // read from the function argument's declared return type.
                self.expect_arg_count(name, args, 2, function)?;
                let list_ty = self.check_expr(&args[0], scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                let (_, ret) = self.expect_fn_arg(
                    name,
                    2,
                    &args[1],
                    (std::slice::from_ref(&element), None),
                    scope,
                    function,
                )?;
                Some(list_type(&ret))
            }
            "list_filter" => {
                // `list_filter(l list<T>, pred fn(T) -> bool) -> list<T>`: keep
                // the elements for which `pred` returns `true`, order preserved.
                self.expect_arg_count(name, args, 2, function)?;
                let list_ty = self.check_expr(&args[0], scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                self.expect_fn_arg(
                    name,
                    2,
                    &args[1],
                    (std::slice::from_ref(&element), Some(&TypeRef::new("bool"))),
                    scope,
                    function,
                )?;
                Some(list_type(&element))
            }
            "list_reduce" => {
                // `list_reduce(l list<T>, init U, f fn(U, T) -> U) -> U`: a left
                // fold. `U` is fixed by `init`; the folding function must be
                // `fn(U, T) -> U`.
                self.expect_arg_count(name, args, 3, function)?;
                let list_ty = self.check_expr(&args[0], scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                let acc = self.check_expr(&args[1], scope, function)?;
                self.expect_fn_arg(
                    name,
                    3,
                    &args[2],
                    (&[acc.clone(), element], Some(&acc)),
                    scope,
                    function,
                )?;
                Some(acc)
            }
            "map_set" => {
                self.expect_arg_count(name, args, 3, function)?;
                // `map_set` returns `map<K, V>`, so the outer expected `map<K, V>`
                // flows into the map argument (inferring a nested `map_new()`),
                // and the resolved key/value types flow into the key/value args.
                let map_ty = self.check_expr_expected(&args[0], expected, scope, function)?;
                let (key, value) = self.expect_map_arg(name, &map_ty, args[0].span, function)?;
                let key_type = self.check_expr_expected(&args[1], Some(&key), scope, function)?;
                if key_type != key {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0388",
                        format!(
                            "`map_set` key must be `{}` but got `{}`",
                            key.name, key_type.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                let value_type =
                    self.check_expr_expected(&args[2], Some(&value), scope, function)?;
                if value_type != value {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0388",
                        format!(
                            "`map_set` value must be `{}` but got `{}`",
                            value.name, value_type.name
                        ),
                        Some(function.name.clone()),
                        args[2].span,
                    ));
                    return None;
                }
                Some(map_type(&key, &value))
            }
            "map_get" => {
                self.expect_arg_count(name, args, 2, function)?;
                let map_ty = self.check_expr(&args[0], scope, function)?;
                let (key, value) = self.expect_map_arg(name, &map_ty, args[0].span, function)?;
                let key_type = self.check_expr(&args[1], scope, function)?;
                if key_type != key {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0388",
                        format!(
                            "`map_get` key must be `{}` but got `{}`",
                            key.name, key_type.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                Some(option_type(&value))
            }
            "map_has" => {
                self.expect_arg_count(name, args, 2, function)?;
                let map_ty = self.check_expr(&args[0], scope, function)?;
                let (key, _value) = self.expect_map_arg(name, &map_ty, args[0].span, function)?;
                let key_type = self.check_expr(&args[1], scope, function)?;
                if key_type != key {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0388",
                        format!(
                            "`map_has` key must be `{}` but got `{}`",
                            key.name, key_type.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                Some(TypeRef::new("bool"))
            }
            "map_len" => {
                self.expect_arg_count(name, args, 1, function)?;
                let map_ty = self.check_expr(&args[0], scope, function)?;
                self.expect_map_arg(name, &map_ty, args[0].span, function)?;
                Some(TypeRef::new("i64"))
            }
            "map_keys" => {
                self.expect_arg_count(name, args, 1, function)?;
                let map_ty = self.check_expr(&args[0], scope, function)?;
                let (key, _value) = self.expect_map_arg(name, &map_ty, args[0].span, function)?;
                Some(list_type(&key))
            }
            "map_values" => {
                self.expect_arg_count(name, args, 1, function)?;
                let map_ty = self.check_expr(&args[0], scope, function)?;
                let (_key, value) = self.expect_map_arg(name, &map_ty, args[0].span, function)?;
                Some(list_type(&value))
            }
            "map_del" => {
                self.expect_arg_count(name, args, 2, function)?;
                let map_ty = self.check_expr_expected(&args[0], expected, scope, function)?;
                let (key, value) = self.expect_map_arg(name, &map_ty, args[0].span, function)?;
                let key_type = self.check_expr(&args[1], scope, function)?;
                if key_type != key {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0388",
                        format!(
                            "`map_del` key must be `{}` but got `{}`",
                            key.name, key_type.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                Some(map_type(&key, &value))
            }
            "substring" => {
                self.expect_arg_count(name, args, 3, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                self.expect_string_builtin_arg(name, 2, &args[1], "i64", scope, function)?;
                self.expect_string_builtin_arg(name, 3, &args[2], "i64", scope, function)?;
                Some(TypeRef::new("string"))
            }
            "find" => {
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                self.expect_string_builtin_arg(name, 2, &args[1], "string", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            "contains" => {
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                self.expect_string_builtin_arg(name, 2, &args[1], "string", scope, function)?;
                Some(TypeRef::new("bool"))
            }
            "starts_with" | "ends_with" => {
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                self.expect_string_builtin_arg(name, 2, &args[1], "string", scope, function)?;
                Some(TypeRef::new("bool"))
            }
            "repeat" => {
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                self.expect_string_builtin_arg(name, 2, &args[1], "i64", scope, function)?;
                Some(TypeRef::new("string"))
            }
            "split" => {
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                self.expect_string_builtin_arg(name, 2, &args[1], "string", scope, function)?;
                Some(TypeRef::new("array<string>"))
            }
            // `words`/`count` are common identifiers, so a user-defined function of
            // that name shadows the builtin (the guard yields to the `_ =>` user-call
            // path). Adding these stdlib names must never break existing user code.
            "words" if !self.signatures.contains_key("words") => {
                // `words(s string) -> array<string>`: split on runs of whitespace,
                // dropping empty fields (like Python's zero-argument `str.split()`).
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("array<string>"))
            }
            "count" if !self.signatures.contains_key("count") => {
                // `count(s string, sub string) -> i64`: non-overlapping occurrences
                // of `sub` in `s` (an empty `sub` yields `0`).
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                self.expect_string_builtin_arg(name, 2, &args[1], "string", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            "join" => {
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_string_builtin_arg(
                    name,
                    1,
                    &args[0],
                    "array<string>",
                    scope,
                    function,
                )?;
                self.expect_string_builtin_arg(name, 2, &args[1], "string", scope, function)?;
                Some(TypeRef::new("string"))
            }
            "trim" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("string"))
            }
            "replace" => {
                self.expect_arg_count(name, args, 3, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                self.expect_string_builtin_arg(name, 2, &args[1], "string", scope, function)?;
                self.expect_string_builtin_arg(name, 3, &args[2], "string", scope, function)?;
                Some(TypeRef::new("string"))
            }
            "upper" | "lower" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("string"))
            }
            "chars" => {
                // `chars(s string) -> list<char>`: the characters of `s` in order.
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                Some(list_type(&TypeRef::new("char")))
            }
            "string_from_chars" => {
                // `string_from_chars(cs list<char>) -> string`: the inverse of `chars`.
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "list<char>", scope, function)?;
                Some(TypeRef::new("string"))
            }
            "to_bytes" => {
                // `to_bytes(s string) -> list<byte>`: the UTF-8 encoding of `s`.
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                Some(list_type(&TypeRef::new("byte")))
            }
            "from_bytes" => {
                // `from_bytes(b list<byte>) -> result<string, string>`: decode the
                // bytes as UTF-8, yielding `err(message)` on invalid input.
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "list<byte>", scope, function)?;
                Some(result_type(
                    &TypeRef::new("string"),
                    &TypeRef::new("string"),
                ))
            }
            "byte_len" => {
                // `byte_len(s string) -> i64`: the UTF-8 byte length of `s`
                // (distinct from `len`, which counts characters for a string).
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            "parse_i64" => {
                // `parse_i64(s string) -> result<i64, string>`: parse `s` as a
                // base-10 signed 64-bit integer, yielding `err(message)` on any
                // failure (empty, non-numeric, or out of range). Whitespace is
                // not trimmed, so a padded string is an `err`.
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                Some(result_type(&TypeRef::new("i64"), &TypeRef::new("string")))
            }
            "parse_f64" => {
                // `parse_f64(s string) -> result<f64, string>`: parse `s` as an
                // `f64`, yielding `err(message)` on failure.
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_string_builtin_arg(name, 1, &args[0], "string", scope, function)?;
                Some(result_type(&TypeRef::new("f64"), &TypeRef::new("string")))
            }
            "abs" => {
                self.expect_arg_count(name, args, 1, function)?;
                let arg_type = self.check_expr(&args[0], scope, function)?;
                if matches!(arg_type.name.as_str(), "i64" | "f64") {
                    Some(TypeRef::new(arg_type.name))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0374",
                        format!(
                            "abs expects an i64 or f64 value but got `{}`",
                            arg_type.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "min" | "max" => {
                self.expect_arg_count(name, args, 2, function)?;
                let left = self.check_expr(&args[0], scope, function)?;
                let right = self.check_expr(&args[1], scope, function)?;
                if left == right && matches!(left.name.as_str(), "i64" | "f64") {
                    Some(TypeRef::new(left.name))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0374",
                        format!(
                            "{name} expects two matching i64 or f64 values but got `{}` and `{}`",
                            left.name, right.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "pow" => {
                self.expect_arg_count(name, args, 2, function)?;
                let base = self.check_expr(&args[0], scope, function)?;
                let exp = self.check_expr(&args[1], scope, function)?;
                if base == exp && matches!(base.name.as_str(), "i64" | "f64") {
                    Some(TypeRef::new(base.name))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0374",
                        format!(
                            "pow expects two matching i64 or f64 values but got `{}` and `{}`",
                            base.name, exp.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "clamp" => {
                // `clamp(x, lo, hi) -> T`: all three operands share the same
                // numeric type (`i64` or `f64`); the result is that type.
                self.expect_arg_count(name, args, 3, function)?;
                let x = self.check_expr(&args[0], scope, function)?;
                let lo = self.check_expr(&args[1], scope, function)?;
                let hi = self.check_expr(&args[2], scope, function)?;
                if x == lo && lo == hi && matches!(x.name.as_str(), "i64" | "f64") {
                    Some(TypeRef::new(x.name))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0374",
                        format!(
                            "clamp expects three matching i64 or f64 values but got `{}`, `{}`, and `{}`",
                            x.name, lo.name, hi.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "sign" => {
                // `sign(x) -> i64`: x is `i64` or `f64`; always returns `i64`.
                self.expect_arg_count(name, args, 1, function)?;
                let arg_type = self.check_expr(&args[0], scope, function)?;
                if matches!(arg_type.name.as_str(), "i64" | "f64") {
                    Some(TypeRef::new("i64"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0374",
                        format!(
                            "sign expects an i64 or f64 value but got `{}`",
                            arg_type.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "gcd" => {
                // `gcd(a, b) -> i64`: both operands are `i64`; the result is the
                // non-negative greatest common divisor.
                self.expect_arg_count(name, args, 2, function)?;
                let a = self.check_expr(&args[0], scope, function)?;
                let b = self.check_expr(&args[1], scope, function)?;
                if a.name == "i64" && b.name == "i64" {
                    Some(TypeRef::new("i64"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0374",
                        format!(
                            "gcd expects two i64 values but got `{}` and `{}`",
                            a.name, b.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "list_sum" => {
                // `list_sum(l) -> T`: sum of a `list<i64>` (wrapping) or
                // `list<f64>`; the result type is the element type. Only numeric
                // element types are accepted.
                self.expect_arg_count(name, args, 1, function)?;
                let list_ty = self.check_expr(&args[0], scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                if matches!(element.name.as_str(), "i64" | "f64") {
                    Some(TypeRef::new(element.name))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0387",
                        format!(
                            "`list_sum` expects a `list<i64>` or `list<f64>` but got `list<{}>`",
                            element.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "list_min" | "list_max" => {
                // `list_min(l)` / `list_max(l)` -> `option<T>` over a numeric
                // list; `none` on empty, else `some(extreme element)`.
                self.expect_arg_count(name, args, 1, function)?;
                let list_ty = self.check_expr(&args[0], scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                if matches!(element.name.as_str(), "i64" | "f64") {
                    Some(option_type(&element))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0387",
                        format!(
                            "`{name}` expects a `list<i64>` or `list<f64>` but got `list<{}>`",
                            element.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "sqrt" | "floor" | "ceil" | "round" | "sin" | "cos" | "tan" | "atan" | "exp" | "ln"
            | "log10" => {
                self.expect_arg_count(name, args, 1, function)?;
                let arg_type = self.check_expr(&args[0], scope, function)?;
                if arg_type.name == "f64" {
                    Some(TypeRef::new("f64"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0374",
                        format!("{name} expects an f64 value but got `{}`", arg_type.name),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "atan2" => {
                // `atan2(y, x)` takes two f64 values and returns the f64 angle.
                self.expect_arg_count(name, args, 2, function)?;
                let y = self.check_expr(&args[0], scope, function)?;
                let x = self.check_expr(&args[1], scope, function)?;
                if y.name == "f64" && x.name == "f64" {
                    Some(TypeRef::new("f64"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0374",
                        format!(
                            "atan2 expects two f64 values but got `{}` and `{}`",
                            y.name, x.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "rotate_left" | "rotate_right" => {
                // Bit rotation: `rotate_left(x, n)` / `rotate_right(x, n)` rotate
                // the 64 bits of `x` by `(n & 63)` positions; both args are i64
                // and the result is i64.
                self.expect_arg_count(name, args, 2, function)?;
                let x = self.check_expr(&args[0], scope, function)?;
                let n = self.check_expr(&args[1], scope, function)?;
                if x.name == "i64" && n.name == "i64" {
                    Some(TypeRef::new("i64"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0374",
                        format!(
                            "{name} expects two i64 values but got `{}` and `{}`",
                            x.name, n.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "count_ones" | "leading_zeros" | "trailing_zeros" | "reverse_bytes" => {
                // Unary bit intrinsics on i64: population count, leading/trailing
                // zero count, and byte swap. Each takes and returns i64.
                self.expect_arg_count(name, args, 1, function)?;
                let arg_type = self.check_expr(&args[0], scope, function)?;
                if arg_type.name == "i64" {
                    Some(TypeRef::new("i64"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0374",
                        format!("{name} expects an i64 value but got `{}`", arg_type.name),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "rc_new" => {
                self.expect_arg_count(name, args, 1, function)?;
                let value_type = self.check_expr(&args[0], scope, function)?;
                Some(TypeRef::new(format!("rc<{}>", value_type.name)))
            }
            "rc_clone" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                self.expect_reference("rc_clone", "rc", &ty, args[0].span, function)?;
                Some(ty)
            }
            "rc_release" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                self.expect_reference("rc_release", "rc", &ty, args[0].span, function)?;
                Some(TypeRef::new("void"))
            }
            "rc_get" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                self.expect_reference("rc_get", "rc", &ty, args[0].span, function)
            }
            "rc_borrow" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                let inner =
                    self.expect_reference("rc_borrow", "rc", &ty, args[0].span, function)?;
                Some(TypeRef::new(format!("ref<{}>", inner.name)))
            }
            "ref_get" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                self.expect_reference("ref_get", "ref", &ty, args[0].span, function)
            }
            "ptr_read" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                let inner = self.expect_raw_pointer("ptr_read", &ty, args[0].span, function)?;
                self.require_unsafe("ptr_read", call_span, function)?;
                Some(inner)
            }
            "ptr_write" => {
                self.expect_arg_count(name, args, 2, function)?;
                let ptr_type = self.check_expr(&args[0], scope, function)?;
                let value_type = self.check_expr(&args[1], scope, function)?;
                let inner =
                    self.expect_raw_pointer("ptr_write", &ptr_type, args[0].span, function)?;
                self.require_unsafe("ptr_write", call_span, function)?;
                if value_type != inner {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0331",
                        format!(
                            "ptr_write expects value `{}` for pointer `{}` but got `{}`",
                            inner.name, ptr_type.name, value_type.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                Some(TypeRef::new("void"))
            }
            // Raw-memory layout queries. `size_of`/`align_of` accept any type
            // with a defined C-natural layout (scalar, pointer/reference handle,
            // struct, or fixed `array<T>`) and fold to an `i64` constant. They
            // are safe (compile-time) queries, so they need no `unsafe` block.
            "size_of" | "align_of" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                if !self.type_has_layout(&ty) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0431",
                        format!(
                            "`{name}` requires a type with a defined memory layout but got `{}`",
                            ty.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    return None;
                }
                Some(TypeRef::new("i64"))
            }
            // `offset_of(x, "field")`: `x` must be a struct value and `field` a
            // string literal naming one of its fields. Folds to an `i64`
            // constant.
            "offset_of" => {
                self.expect_arg_count(name, args, 2, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                let ExprKind::String(field) = &args[1].kind else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0431",
                        "offset_of expects a string-literal field name as its second argument"
                            .to_string(),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                };
                let Some(fields) = self.structs.get(&ty.name) else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0431",
                        format!("offset_of expects a struct value but got `{}`", ty.name),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    return None;
                };
                if !fields.iter().any(|declared| &declared.name == field) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0431",
                        format!("struct `{}` has no field `{field}`", ty.name),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                // Reject a struct whose layout is undefined (a non-sized field),
                // so the runtime never fails to fold the constant.
                if !self.type_has_layout(&ty) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0431",
                        format!(
                            "offset_of requires a struct with a fully sized layout but `{}` has a field with no defined layout",
                            ty.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    return None;
                }
                Some(TypeRef::new("i64"))
            }
            // `ptr_to_int(p) -> i64`: the integer handle/address of a raw
            // pointer. Reinterpreting a pointer as an integer is `unsafe`.
            "ptr_to_int" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                self.expect_raw_pointer("ptr_to_int", &ty, args[0].span, function)?;
                self.require_unsafe("ptr_to_int", call_span, function)?;
                Some(TypeRef::new("i64"))
            }
            // `int_to_ptr(n) -> ptr<T>`: reconstruct a raw pointer from an
            // integer handle. Fabricating a pointer from an integer is `unsafe`.
            // The concrete pointee comes from the caller's expected annotation
            // when it is a raw pointer; otherwise it defaults to `ptr<i64>`.
            "int_to_ptr" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_arg_type(name, 1, &args[0], "i64", scope, function)?;
                self.require_unsafe("int_to_ptr", call_span, function)?;
                let result = expected
                    .filter(|ty| ty.is_raw_pointer())
                    .cloned()
                    .unwrap_or_else(|| TypeRef::new("ptr<i64>"));
                Some(result)
            }
            // `volatile_load(p) -> T` / `volatile_store(p, v)`: raw pointer
            // element read/write with volatile semantics (no elision or
            // reordering). Type-check exactly like `ptr_read`/`ptr_write`; the
            // volatility guarantee is realized by native codegen.
            "volatile_load" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                let inner =
                    self.expect_raw_pointer("volatile_load", &ty, args[0].span, function)?;
                self.require_unsafe("volatile_load", call_span, function)?;
                Some(inner)
            }
            "volatile_store" => {
                self.expect_arg_count(name, args, 2, function)?;
                let ptr_type = self.check_expr(&args[0], scope, function)?;
                let value_type = self.check_expr(&args[1], scope, function)?;
                let inner =
                    self.expect_raw_pointer("volatile_store", &ptr_type, args[0].span, function)?;
                self.require_unsafe("volatile_store", call_span, function)?;
                if value_type != inner {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0331",
                        format!(
                            "volatile_store expects value `{}` for pointer `{}` but got `{}`",
                            inner.name, ptr_type.name, value_type.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                Some(TypeRef::new("void"))
            }
            "char_code" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_scalar_builtin_arg(name, 1, &args[0], "char", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            "char_from" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_scalar_builtin_arg(name, 1, &args[0], "i64", scope, function)?;
                Some(TypeRef::new("char"))
            }
            "is_digit" | "is_alpha" | "is_alnum" | "is_whitespace" | "is_upper" | "is_lower" => {
                // Deterministic `char -> bool` classification predicates backed by
                // the corresponding Rust `char` methods. Each takes exactly one
                // `char` argument and yields a `bool`.
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_scalar_builtin_arg(name, 1, &args[0], "char", scope, function)?;
                Some(TypeRef::new("bool"))
            }
            "byte" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_scalar_builtin_arg(name, 1, &args[0], "i64", scope, function)?;
                Some(TypeRef::new("byte"))
            }
            "byte_val" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_scalar_builtin_arg(name, 1, &args[0], "byte", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            // Fixed-width integer conversions. Each `to_<T>` reinterprets an
            // `i64` into width `T` (wrapping); `to_i64` widens a fixed-width
            // integer back to `i64`. No implicit coercion exists, so these
            // explicit conversions are the only bridge between widths.
            "to_i8" | "to_i16" | "to_i32" | "to_u8" | "to_u16" | "to_u32" | "to_u64"
            | "to_isize" | "to_usize" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_scalar_builtin_arg(name, 1, &args[0], "i64", scope, function)?;
                // The target width is the builtin name with the `to_` prefix removed.
                Some(TypeRef::new(&name[3..]))
            }
            "to_i64" => {
                self.expect_arg_count(name, args, 1, function)?;
                let arg_type = self.check_expr(&args[0], scope, function)?;
                if !is_fixed_width_int_name(&arg_type.name) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0307",
                        format!(
                            "to_i64 expects a fixed-width integer argument but got `{}`",
                            arg_type.name
                        ),
                        Some(function.name.clone()),
                        call_span,
                    ));
                    return None;
                }
                Some(TypeRef::new("i64"))
            }
            // Float conversions: `to_f32` rounds an `f64` to `f32`; `to_f64`
            // widens an `f32` back to `f64`. No implicit float coercion exists.
            "to_f32" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_scalar_builtin_arg(name, 1, &args[0], "f64", scope, function)?;
                Some(TypeRef::new("f32"))
            }
            "to_f64" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_scalar_builtin_arg(name, 1, &args[0], "f32", scope, function)?;
                Some(TypeRef::new("f64"))
            }
            // Overflow-aware arithmetic on a fixed-width integer `T`: both operands
            // must be the same fixed-width type. `checked_*` yields `option<T>`
            // (`none` on overflow); `saturating_*`/`wrapping_*` yield `T`. `i64`
            // is excluded — its default arithmetic already traps on overflow.
            "checked_add" | "checked_sub" | "checked_mul" | "saturating_add" | "saturating_sub"
            | "saturating_mul" | "wrapping_add" | "wrapping_sub" | "wrapping_mul" => {
                self.expect_arg_count(name, args, 2, function)?;
                let left = self.check_expr(&args[0], scope, function)?;
                let right = self.check_expr(&args[1], scope, function)?;
                if !is_fixed_width_int_name(&left.name) || left != right {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0307",
                        format!("{name} operands must both be the same fixed-width integer type"),
                        Some(function.name.clone()),
                        call_span,
                    ));
                    return None;
                }
                if name.starts_with("checked_") {
                    Some(option_type(&left))
                } else {
                    Some(left)
                }
            }
            "env" => {
                self.expect_process_arg_count(name, args, 1, call_span, function)?;
                self.expect_process_arg(name, 1, &args[0], "string", scope, function)?;
                Some(option_type(&TypeRef::new("string")))
            }
            "args" => {
                self.expect_process_arg_count(name, args, 0, call_span, function)?;
                Some(list_type(&TypeRef::new("string")))
            }
            "os_random" => {
                // `os_random(len i64) -> result<list<byte>, string>`: `len`
                // cryptographically-secure random bytes from the OS RNG as
                // `ok(list<byte>)`, or `err(message)` on RNG failure. `len < 0`
                // yields `err` at runtime (not a compile error).
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_arg_type(name, 1, &args[0], "i64", scope, function)?;
                Some(result_type(
                    &list_type(&TypeRef::new("byte")),
                    &TypeRef::new("string"),
                ))
            }
            "parallel_map" => {
                // `parallel_map(f fn(i64) -> i64, args list<i64>) -> list<i64>`:
                // apply `f` to each element on a separate OS thread, returning
                // the mapped values in input order.
                if args.len() != 2 {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0334",
                        format!("parallel_map expects 2 arguments but got {}", args.len()),
                        Some(function.name.clone()),
                        call_span,
                    ));
                    return None;
                }
                let expected_fn = function_type(&[TypeRef::new("i64")], &TypeRef::new("i64"));
                let func_type = self.check_expr(&args[0], scope, function)?;
                if func_type != expected_fn {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0334",
                        format!(
                            "parallel_map expects a `{}` as its first argument but got `{}`",
                            expected_fn.name, func_type.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    return None;
                }
                let expected_list = list_type(&TypeRef::new("i64"));
                let list_arg_type = self.check_expr(&args[1], scope, function)?;
                if list_arg_type != expected_list {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0334",
                        format!(
                            "parallel_map expects a `{}` as its second argument but got `{}`",
                            expected_list.name, list_arg_type.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                Some(list_type(&TypeRef::new("i64")))
            }
            "chan_new" => {
                // `chan_new() -> Chan`.
                self.expect_concurrency_arity(name, args, 0, call_span, function)?;
                Some(TypeRef::new("Chan"))
            }
            "send" => {
                // `send(ch Chan, v i64) -> void`.
                self.expect_concurrency_arity(name, args, 2, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "Chan", scope, function)?;
                self.expect_concurrency_arg(name, 2, &args[1], "i64", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "recv" => {
                // `recv(ch Chan) -> i64`.
                self.expect_concurrency_arity(name, args, 1, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "Chan", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            "try_recv" => {
                // `try_recv(ch Chan) -> option<i64>`.
                self.expect_concurrency_arity(name, args, 1, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "Chan", scope, function)?;
                Some(option_type(&TypeRef::new("i64")))
            }
            "spawn" => {
                // `spawn(f fn(Chan, i64) -> void, ch Chan, v i64) -> Task`.
                self.expect_concurrency_arity(name, args, 3, call_span, function)?;
                let expected_fn = function_type(
                    &[TypeRef::new("Chan"), TypeRef::new("i64")],
                    &TypeRef::new("void"),
                );
                let func_type = self.check_expr(&args[0], scope, function)?;
                if func_type != expected_fn {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0337",
                        format!(
                            "spawn expects a `{}` as its first argument but got `{}`",
                            expected_fn.name, func_type.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    return None;
                }
                self.expect_concurrency_arg(name, 2, &args[1], "Chan", scope, function)?;
                self.expect_concurrency_arg(name, 3, &args[2], "i64", scope, function)?;
                Some(TypeRef::new("Task"))
            }
            "task_join" => {
                // `task_join(t Task) -> void` (named `task_join` because `join`
                // is the string-list joiner builtin).
                self.expect_concurrency_arity(name, args, 1, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "Task", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "mutex_new" => {
                // `mutex_new(v i64) -> Mutex`.
                self.expect_concurrency_arity(name, args, 1, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "i64", scope, function)?;
                Some(TypeRef::new("Mutex"))
            }
            "mutex_get" => {
                // `mutex_get(m Mutex) -> i64`.
                self.expect_concurrency_arity(name, args, 1, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "Mutex", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            "mutex_set" => {
                // `mutex_set(m Mutex, v i64) -> void`.
                self.expect_concurrency_arity(name, args, 2, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "Mutex", scope, function)?;
                self.expect_concurrency_arg(name, 2, &args[1], "i64", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "mutex_add" => {
                // `mutex_add(m Mutex, delta i64) -> i64`.
                self.expect_concurrency_arity(name, args, 2, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "Mutex", scope, function)?;
                self.expect_concurrency_arg(name, 2, &args[1], "i64", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            "atomic_new" => {
                // `atomic_new(v i64) -> atomic_i64`: a shared atomic cell.
                self.expect_concurrency_arity(name, args, 1, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "i64", scope, function)?;
                Some(TypeRef::new("atomic_i64"))
            }
            "atomic_load" => {
                // `atomic_load(a atomic_i64) -> i64`.
                self.expect_concurrency_arity(name, args, 1, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "atomic_i64", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            "atomic_store" => {
                // `atomic_store(a atomic_i64, v i64) -> void`.
                self.expect_concurrency_arity(name, args, 2, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "atomic_i64", scope, function)?;
                self.expect_concurrency_arg(name, 2, &args[1], "i64", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "atomic_swap" => {
                // `atomic_swap(a atomic_i64, v i64) -> i64` (returns previous).
                self.expect_concurrency_arity(name, args, 2, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "atomic_i64", scope, function)?;
                self.expect_concurrency_arg(name, 2, &args[1], "i64", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            "atomic_cas" => {
                // `atomic_cas(a atomic_i64, expected i64, new i64) -> i64`
                // (strong CAS; returns the observed value).
                self.expect_concurrency_arity(name, args, 3, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "atomic_i64", scope, function)?;
                self.expect_concurrency_arg(name, 2, &args[1], "i64", scope, function)?;
                self.expect_concurrency_arg(name, 3, &args[2], "i64", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            "atomic_add" | "atomic_sub" | "atomic_and" | "atomic_or" | "atomic_xor" => {
                // Fetch-and-op: `atomic_<op>(a atomic_i64, v i64) -> i64`
                // (returns the previous value).
                self.expect_concurrency_arity(name, args, 2, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "atomic_i64", scope, function)?;
                self.expect_concurrency_arg(name, 2, &args[1], "i64", scope, function)?;
                Some(TypeRef::new("i64"))
            }
            "atomic_load_ordered" => {
                // `atomic_load_ordered(a atomic_i64, order MemoryOrder) -> i64`.
                // A load may use `relaxed`/`acquire`/`seq_cst`, never
                // `release`/`acq_rel`.
                self.expect_concurrency_arity(name, args, 2, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "atomic_i64", scope, function)?;
                self.check_ordering_arg(
                    name,
                    2,
                    &args[1],
                    &["relaxed", "acquire", "seq_cst"],
                    scope,
                    function,
                )?;
                Some(TypeRef::new("i64"))
            }
            "atomic_store_ordered" => {
                // `atomic_store_ordered(a atomic_i64, v i64, order MemoryOrder)
                // -> void`. A store may use `relaxed`/`release`/`seq_cst`, never
                // `acquire`/`acq_rel`.
                self.expect_concurrency_arity(name, args, 3, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "atomic_i64", scope, function)?;
                self.expect_concurrency_arg(name, 2, &args[1], "i64", scope, function)?;
                self.check_ordering_arg(
                    name,
                    3,
                    &args[2],
                    &["relaxed", "release", "seq_cst"],
                    scope,
                    function,
                )?;
                Some(TypeRef::new("void"))
            }
            "atomic_swap_ordered"
            | "atomic_add_ordered"
            | "atomic_sub_ordered"
            | "atomic_and_ordered"
            | "atomic_or_ordered"
            | "atomic_xor_ordered" => {
                // Ordered read-modify-write: `(a atomic_i64, v i64, order
                // MemoryOrder) -> i64`. Every ordering is valid for an RMW.
                self.expect_concurrency_arity(name, args, 3, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "atomic_i64", scope, function)?;
                self.expect_concurrency_arg(name, 2, &args[1], "i64", scope, function)?;
                self.check_ordering_arg(
                    name,
                    3,
                    &args[2],
                    &MEMORY_ORDER_VARIANTS,
                    scope,
                    function,
                )?;
                Some(TypeRef::new("i64"))
            }
            "atomic_cas_ordered" => {
                // `atomic_cas_ordered(a atomic_i64, expected i64, new i64,
                // success MemoryOrder, failure MemoryOrder) -> i64`. `success`
                // takes any ordering; `failure` is a load and cannot be
                // `release`/`acq_rel`.
                self.expect_concurrency_arity(name, args, 5, call_span, function)?;
                self.expect_concurrency_arg(name, 1, &args[0], "atomic_i64", scope, function)?;
                self.expect_concurrency_arg(name, 2, &args[1], "i64", scope, function)?;
                self.expect_concurrency_arg(name, 3, &args[2], "i64", scope, function)?;
                self.check_ordering_arg(
                    name,
                    4,
                    &args[3],
                    &MEMORY_ORDER_VARIANTS,
                    scope,
                    function,
                )?;
                self.check_ordering_arg(
                    name,
                    5,
                    &args[4],
                    &["relaxed", "acquire", "seq_cst"],
                    scope,
                    function,
                )?;
                Some(TypeRef::new("i64"))
            }
            "fence" => {
                // `fence(order MemoryOrder) -> void`: a standalone memory fence.
                // A fence is meaningless under `relaxed`, so only
                // `acquire`/`release`/`acq_rel`/`seq_cst` are accepted.
                self.expect_concurrency_arity(name, args, 1, call_span, function)?;
                self.check_ordering_arg(
                    name,
                    1,
                    &args[0],
                    &["acquire", "release", "acq_rel", "seq_cst"],
                    scope,
                    function,
                )?;
                Some(TypeRef::new("void"))
            }
            "tcp_connect" | "tcp_listen" | "udp_bind" => {
                // `(host string, port i64) -> result<Socket, string>`.
                self.expect_socket_arg_count(name, args, 2, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "string", scope, function)?;
                self.expect_socket_arg_type(name, 2, &args[1], "i64", scope, function)?;
                Some(result_type(
                    &TypeRef::new("Socket"),
                    &TypeRef::new("string"),
                ))
            }
            "tcp_accept" => {
                // `(listener Socket) -> result<Socket, string>`.
                self.expect_socket_arg_count(name, args, 1, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "Socket", scope, function)?;
                Some(result_type(
                    &TypeRef::new("Socket"),
                    &TypeRef::new("string"),
                ))
            }
            "tcp_read" => {
                // `(conn Socket) -> result<string, string>`.
                self.expect_socket_arg_count(name, args, 1, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "Socket", scope, function)?;
                Some(result_type(
                    &TypeRef::new("string"),
                    &TypeRef::new("string"),
                ))
            }
            "tcp_write" => {
                // `(conn Socket, data string) -> result<i64, string>`.
                self.expect_socket_arg_count(name, args, 2, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "Socket", scope, function)?;
                self.expect_socket_arg_type(name, 2, &args[1], "string", scope, function)?;
                Some(result_type(&TypeRef::new("i64"), &TypeRef::new("string")))
            }
            "tcp_close" => {
                // `(conn Socket) -> void`.
                self.expect_socket_arg_count(name, args, 1, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "Socket", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "tcp_shutdown" => {
                // `(conn Socket) -> void`: gracefully shut down the write half so
                // buffered bytes are delivered (EOF) before the socket is dropped.
                self.expect_socket_arg_count(name, args, 1, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "Socket", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "udp_send_to" => {
                // `(sock Socket, data string, host string, port i64)
                // -> result<i64, string>`.
                self.expect_socket_arg_count(name, args, 4, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "Socket", scope, function)?;
                self.expect_socket_arg_type(name, 2, &args[1], "string", scope, function)?;
                self.expect_socket_arg_type(name, 3, &args[2], "string", scope, function)?;
                self.expect_socket_arg_type(name, 4, &args[3], "i64", scope, function)?;
                Some(result_type(&TypeRef::new("i64"), &TypeRef::new("string")))
            }
            "udp_recv" => {
                // `(sock Socket) -> result<string, string>`.
                self.expect_socket_arg_count(name, args, 1, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "Socket", scope, function)?;
                Some(result_type(
                    &TypeRef::new("string"),
                    &TypeRef::new("string"),
                ))
            }
            "set_nonblocking" => {
                // `(sock Socket, enabled bool) -> result<i64, string>`: toggle a
                // socket's non-blocking mode so the `*_nb` builtins can surface a
                // would-block condition as `ok(none)` instead of blocking.
                self.expect_socket_arg_count(name, args, 2, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "Socket", scope, function)?;
                self.expect_socket_arg_type(name, 2, &args[1], "bool", scope, function)?;
                Some(result_type(&TypeRef::new("i64"), &TypeRef::new("string")))
            }
            "tcp_accept_nb" => {
                // `(listener Socket) -> result<option<Socket>, string>`:
                // non-blocking accept; `ok(none)` means would-block.
                self.expect_socket_arg_count(name, args, 1, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "Socket", scope, function)?;
                Some(result_type(
                    &option_type(&TypeRef::new("Socket")),
                    &TypeRef::new("string"),
                ))
            }
            "tcp_read_nb" => {
                // `(conn Socket, max i64) -> result<option<string>, string>`:
                // non-blocking read; `ok(none)` means would-block, `ok(some(""))`
                // means a clean EOF.
                self.expect_socket_arg_count(name, args, 2, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "Socket", scope, function)?;
                self.expect_socket_arg_type(name, 2, &args[1], "i64", scope, function)?;
                Some(result_type(
                    &option_type(&TypeRef::new("string")),
                    &TypeRef::new("string"),
                ))
            }
            "udp_recv_nb" => {
                // `(sock Socket) -> result<option<string>, string>`:
                // non-blocking receive; `ok(none)` means would-block.
                self.expect_socket_arg_count(name, args, 1, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "Socket", scope, function)?;
                Some(result_type(
                    &option_type(&TypeRef::new("string")),
                    &TypeRef::new("string"),
                ))
            }
            "http_get" => {
                // `(url string) -> result<string, string>`.
                self.expect_http_arg_count(name, args, 1, function)?;
                self.expect_http_arg_type(name, 1, &args[0], scope, function)?;
                Some(result_type(
                    &TypeRef::new("string"),
                    &TypeRef::new("string"),
                ))
            }
            "http_post" => {
                // `(url string, body string) -> result<string, string>`.
                self.expect_http_arg_count(name, args, 2, function)?;
                self.expect_http_arg_type(name, 1, &args[0], scope, function)?;
                self.expect_http_arg_type(name, 2, &args[1], scope, function)?;
                Some(result_type(
                    &TypeRef::new("string"),
                    &TypeRef::new("string"),
                ))
            }
            "proc_spawn" => {
                // `(cmd string, args array<string>) -> result<process, string>`.
                // Spawns a live child process capturing stdout/stderr; extends the
                // one-shot `sys_status`/`sys_output`. Reuses the socket/network
                // handle diagnostic family (`L0335`).
                self.expect_socket_arg_count(name, args, 2, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "string", scope, function)?;
                self.expect_socket_arg_type(name, 2, &args[1], "array<string>", scope, function)?;
                Some(result_type(
                    &TypeRef::new("process"),
                    &TypeRef::new("string"),
                ))
            }
            "proc_wait" | "proc_kill" => {
                // `(p process) -> result<i64, string>`: block for exit / kill the
                // child, returning an exit code (`proc_wait`) or `0` (`proc_kill`).
                self.expect_socket_arg_count(name, args, 1, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "process", scope, function)?;
                Some(result_type(&TypeRef::new("i64"), &TypeRef::new("string")))
            }
            "proc_stdout" | "proc_stderr" => {
                // `(p process) -> result<string, string>`: the child's captured
                // stdout / stderr, read to end.
                self.expect_socket_arg_count(name, args, 1, function)?;
                self.expect_socket_arg_type(name, 1, &args[0], "process", scope, function)?;
                Some(result_type(
                    &TypeRef::new("string"),
                    &TypeRef::new("string"),
                ))
            }
            _ => {
                // If the callee's return type is still an unresolved inference
                // sentinel, resolve it now so this call site sees a concrete
                // type. This drives on-demand inference during the pre-pass (a
                // function inferring its own return type reaches a callee whose
                // return type is not yet known); it is a no-op afterwards.
                if self
                    .signatures
                    .get(name)
                    .is_some_and(|s| s.return_type.name == INFERRED_RETURN)
                {
                    self.infer_return(name);
                }
                let Some(signature) = self.signatures.get(name).cloned() else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0309",
                        format!("unknown function `{name}`"),
                        Some(function.name.clone()),
                        call_span,
                    ));
                    return None;
                };

                if signature.params.len() != args.len() {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0312",
                        format!(
                            "function `{name}` expects {} arguments but got {}",
                            signature.params.len(),
                            args.len()
                        ),
                        Some(function.name.clone()),
                        call_span,
                    ));
                    return None;
                }

                if signature.type_params.is_empty() {
                    // Non-generic call: every argument must match its declared
                    // parameter type exactly. The declared parameter type is
                    // propagated as the expected type so a nested context-directed
                    // constructor (`none`/`ok`/`err`/`list_new`/`map_new`) in
                    // argument position infers from it.
                    for (index, (arg, expected)) in
                        args.iter().zip(signature.params.iter()).enumerate()
                    {
                        let actual = self.check_expr_expected(arg, Some(expected), scope, function);
                        // An extern's `cstr` parameter is not a Lullaby value type:
                        // the caller supplies a `string`, which the FFI boundary
                        // materializes into a NUL-terminated buffer. Accept exactly a
                        // `string` there; any other argument type falls through to
                        // the mismatch report below.
                        if signature.is_extern
                            && expected.name == "cstr"
                            && actual.as_ref().map(|ty| ty.name.as_str()) == Some("string")
                        {
                            continue;
                        }
                        if actual.as_ref() != Some(expected) {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0313",
                                format!(
                                    "argument {} for `{name}` must be `{}` but got `{}`",
                                    index + 1,
                                    expected.name,
                                    actual
                                        .as_ref()
                                        .map(|ty| ty.name.as_str())
                                        .unwrap_or("<unknown>")
                                ),
                                Some(function.name.clone()),
                                arg.span,
                            ));
                        }
                    }
                    // Calling an `async fn` runs its body on a spawned thread and
                    // yields a `Future<return_type>`; `await` later resolves the
                    // `T`. A synchronous call yields the return type directly.
                    if signature.is_async {
                        return Some(future_type(&signature.return_type));
                    }
                    return Some(signature.return_type);
                }

                self.check_generic_call(name, args, &signature, call_span, scope, function)
            }
        }
    }

    /// Check a trait-method call `method(recv, extra_args...)`. The receiver's
    /// type selects the impl:
    ///
    /// - When the receiver's type is a bounded generic type variable `T` whose
    ///   bounds include this trait, the call resolves against the trait
    ///   signature with `Self` = `T` (dispatch is deferred to run time).
    /// - Otherwise the receiver must be a concrete type that implements the
    ///   trait (`L0400` if not); the impl's resolved signature is used.
    ///
    /// The remaining arguments are checked against the method's parameter types.
    fn check_trait_method_call(
        &mut self,
        method: &str,
        trait_name: &str,
        args: &[Expr],
        call_span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        if args.is_empty() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0398",
                format!("trait method `{method}` requires a receiver argument"),
                Some(function.name.clone()),
                call_span,
            ));
            return None;
        }
        let receiver_ty = self.check_expr(&args[0], scope, function)?;

        // Resolve the method's `(param types after self, return type)` in terms
        // of the receiver type, either via a bound on a generic type variable or
        // via a concrete impl.
        let (param_types, return_type) = if self.type_param_has_bound(
            function,
            &receiver_ty.name,
            trait_name,
        ) {
            // Bounded generic receiver: resolve against the trait signature with
            // `Self` = the type variable itself.
            // invariant: `trait_name` is `self.trait_methods[method]`, the reverse
            // index built from the same trait declarations as `self.traits`, so the
            // named trait is present and declares this method.
            let sig = self
                .traits
                .get(trait_name)
                .and_then(|methods| methods.iter().find(|m| m.name == method))
                .expect("trait method exists");
            let param_types = sig
                .params
                .iter()
                .map(|param| substitute_self(&param.ty, &receiver_ty))
                .collect::<Vec<_>>();
            let return_type = substitute_self(&sig.return_type, &receiver_ty);
            (param_types, return_type)
        } else {
            let dispatch = dispatch_type_name(&receiver_ty);
            match self
                .impl_methods
                .get(&(dispatch, method.to_string()))
                .cloned()
            {
                Some(resolved) => resolved,
                None => {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0400",
                        format!(
                            "type `{}` does not implement trait `{trait_name}` (required to call `{method}`)",
                            receiver_ty.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    return None;
                }
            }
        };

        let extra = &args[1..];
        if extra.len() != param_types.len() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0312",
                format!(
                    "trait method `{method}` expects {} argument(s) after the receiver but got {}",
                    param_types.len(),
                    extra.len()
                ),
                Some(function.name.clone()),
                call_span,
            ));
            return None;
        }
        for (index, (arg, expected)) in extra.iter().zip(param_types.iter()).enumerate() {
            let actual = self.check_expr(arg, scope, function);
            if actual.as_ref() != Some(expected) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0313",
                    format!(
                        "argument {} for trait method `{method}` must be `{}` but got `{}`",
                        index + 2,
                        expected.name,
                        actual
                            .as_ref()
                            .map(|ty| ty.name.as_str())
                            .unwrap_or("<unknown>")
                    ),
                    Some(function.name.clone()),
                    arg.span,
                ));
            }
        }
        Some(return_type)
    }

    /// True when `type_name` is a generic type parameter of `function` whose
    /// declared bounds include `trait_name`.
    fn type_param_has_bound(&self, function: &Function, type_name: &str, trait_name: &str) -> bool {
        function
            .type_params
            .iter()
            .any(|tp| tp.name == type_name && tp.bounds.iter().any(|b| b == trait_name))
    }

    /// Check a call to a user-defined generic function. Each argument is checked
    /// for its own type, then unified against the (possibly type-variable
    /// containing) parameter type to build a substitution; the substitution is
    /// applied to the declared return type to yield the call's result type.
    ///
    /// - A type variable bound to two different concrete types is `L0395`.
    /// - A type variable that appears only in the return type and is never
    ///   pinned by an argument is `L0396`.
    ///
    /// Concrete (non-variable) parts of a parameter type are still validated by
    /// the same structural unifier: a mismatch there leaves the variable unbound
    /// or produces a fixed-vs-fixed disagreement, which surfaces as an ordinary
    /// argument-type error via the fixed-part check below.
    fn check_generic_call(
        &mut self,
        name: &str,
        args: &[Expr],
        signature: &Signature,
        call_span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let mut subst: HashMap<String, TypeRef> = HashMap::new();
        let mut arg_types: Vec<Option<TypeRef>> = Vec::with_capacity(args.len());
        for (arg, param) in args.iter().zip(signature.params.iter()) {
            // Pass the parameter type as the expected type so context-directed
            // builtins (`none`/`ok`/`err`/`list_new`) that flow into a generic
            // slot still infer, when the parameter is a concrete generic type.
            let expected = if signature
                .type_params
                .iter()
                .any(|tp| type_contains_var(param, tp))
            {
                None
            } else {
                Some(param.clone())
            };
            let actual = self.check_expr_expected(arg, expected.as_ref(), scope, function);
            if let Some(actual_ty) = &actual {
                match unify_param(param, actual_ty, &signature.type_params, &mut subst) {
                    Ok(()) => {}
                    Err(GenericInferenceError::Conflict {
                        param: tp,
                        first,
                        second,
                    }) => {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0395",
                            format!(
                                "type parameter `{tp}` of `{name}` is inferred as both `{}` and `{}`",
                                first.name, second.name
                            ),
                            Some(function.name.clone()),
                            arg.span,
                        ));
                    }
                    Err(GenericInferenceError::Unresolved { .. }) => {}
                }
            }
            arg_types.push(actual);
        }

        // Validate the fixed (non-type-variable) portions of each parameter type
        // against the argument: after substitution the parameter must equal the
        // argument type. This catches a `list<i64>` argument passed where
        // `option<T>` is expected, and a concrete parameter mismatch.
        for (index, (arg_ty, param)) in arg_types.iter().zip(signature.params.iter()).enumerate() {
            let Some(arg_ty) = arg_ty else { continue };
            let expected = substitute_type(param, &subst);
            // Skip when the expected type still holds an unbound variable; that
            // is reported below as `L0396` (or was a conflict already).
            if first_unresolved_type_var(&expected, &signature.type_params, &subst).is_some() {
                continue;
            }
            if &expected != arg_ty {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0313",
                    format!(
                        "argument {} for `{name}` must be `{}` but got `{}`",
                        index + 1,
                        expected.name,
                        arg_ty.name
                    ),
                    Some(function.name.clone()),
                    args[index].span,
                ));
            }
        }

        if let Some(tp) =
            first_unresolved_type_var(&signature.return_type, &signature.type_params, &subst)
        {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0396",
                format!(
                    "type parameter `{tp}` of `{name}` cannot be inferred from the arguments; explicit type arguments are not yet supported"
                ),
                Some(function.name.clone()),
                call_span,
            ));
            return None;
        }

        // Trait-bound check: each type parameter's inferred concrete type must
        // implement every trait named in its bounds (`L0400`).
        for (param_name, bounds) in signature
            .type_params
            .iter()
            .zip(signature.type_param_bounds.iter())
        {
            if bounds.is_empty() {
                continue;
            }
            let Some(concrete) = subst.get(param_name) else {
                continue; // unresolved variables were already reported above
            };
            let dispatch = dispatch_type_name(concrete);
            for bound in bounds {
                if !self
                    .impl_traits
                    .contains(&(dispatch.clone(), bound.clone()))
                {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0400",
                        format!(
                            "type `{}` inferred for type parameter `{param_name}` of `{name}` does not implement bound trait `{bound}`",
                            concrete.name
                        ),
                        Some(function.name.clone()),
                        call_span,
                    ));
                }
            }
        }

        Some(substitute_type(&signature.return_type, &subst))
    }

    /// Walk a struct field path from `root`, returning the type of the final
    /// field. Empty path returns `root`. Emits L0371 on a bad step.
    fn resolve_field_path(
        &mut self,
        root: &TypeRef,
        path: &[Place],
        span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let mut current = root.clone();
        for place in path {
            match place {
                Place::Field(field) => {
                    let Some(fields) = self.structs.get(&current.name) else {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0371",
                            format!(
                                "cannot access field `{field}` on non-struct type `{}`",
                                current.name
                            ),
                            Some(function.name.clone()),
                            span,
                        ));
                        return None;
                    };
                    match fields.iter().find(|f| &f.name == field) {
                        Some(matched) => current = matched.ty.clone(),
                        None => {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0371",
                                format!("struct `{}` has no field `{field}`", current.name),
                                Some(function.name.clone()),
                                span,
                            ));
                            return None;
                        }
                    }
                }
                Place::Index(index) => {
                    let index_type = self.check_expr(index, scope, function);
                    if index_type.as_ref().map(|ty| ty.name.as_str()) != Some("i64") {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0326",
                            "array index expression must be i64",
                            Some(function.name.clone()),
                            index.span,
                        ));
                    }
                    match current.array_element() {
                        Some(element) => current = element,
                        None => {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0325",
                                "index target must be an array",
                                Some(function.name.clone()),
                                span,
                            ));
                            return None;
                        }
                    }
                }
            }
        }
        Some(current)
    }

    fn check_struct_construction(
        &mut self,
        name: &str,
        args: &[Expr],
        span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let fields = self.structs.get(name).cloned()?;
        if args.len() != fields.len() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0372",
                format!(
                    "struct `{name}` expects {} fields but got {}",
                    fields.len(),
                    args.len()
                ),
                Some(function.name.clone()),
                span,
            ));
            return None;
        }
        for (field, arg) in fields.iter().zip(args) {
            let arg_type = self.check_expr(arg, scope, function);
            if arg_type.as_ref() != Some(&field.ty) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0372",
                    format!(
                        "field `{}` of struct `{name}` expects `{}` but got `{}`",
                        field.name,
                        field.ty.name,
                        arg_type
                            .as_ref()
                            .map(|ty| ty.name.as_str())
                            .unwrap_or("<unknown>")
                    ),
                    Some(function.name.clone()),
                    arg.span,
                ));
            }
        }
        Some(TypeRef::new(name))
    }

    /// Validate enum construction `Variant(args...)`: the payload arity and each
    /// per-payload type must match the variant's declaration. Returns the owning
    /// enum's nominal type.
    fn check_enum_construction(
        &mut self,
        name: &str,
        args: &[Expr],
        span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let (enum_name, payload) = self.variants.get(name).cloned()?;
        if args.len() != payload.len() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0381",
                format!(
                    "variant `{name}` of enum `{enum_name}` expects {} payload value(s) but got {}",
                    payload.len(),
                    args.len()
                ),
                Some(function.name.clone()),
                span,
            ));
            // Still type-check the arguments to surface nested errors.
            for arg in args {
                self.check_expr(arg, scope, function);
            }
            return None;
        }
        for (expected, arg) in payload.iter().zip(args) {
            let arg_type = self.check_expr(arg, scope, function);
            if arg_type.as_ref() != Some(expected) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0381",
                    format!(
                        "payload of variant `{name}` expects `{}` but got `{}`",
                        expected.name,
                        arg_type
                            .as_ref()
                            .map(|ty| ty.name.as_str())
                            .unwrap_or("<unknown>")
                    ),
                    Some(function.name.clone()),
                    arg.span,
                ));
            }
        }
        Some(TypeRef::new(enum_name))
    }

    /// Validate a `match` over an enum. The scrutinee must be an enum type
    /// (`L0383`). Each arm's variant must belong to that enum with the correct
    /// binding arity (`L0385`), duplicate variant arms are rejected (`L0385`),
    /// and the match must be exhaustive — every variant covered or a `_`
    /// wildcard present (`L0384`). The result type is the arms' common body type
    /// when they all agree, mirroring `if`/`try`; otherwise it is void.
    /// The `(display name, ordered variants)` a `match` dispatches over for a
    /// scrutinee type. Handles user enums plus the built-in `option<U>`
    /// (`some(U)` + `none`) and `result<T, E>` (`ok(T)` + `err(E)`) generics,
    /// whose variant payloads are instantiated from the scrutinee's type args.
    fn match_variants(&self, ty: &TypeRef) -> Option<(String, Vec<EnumVariant>)> {
        if let Some(variants) = self.enums.get(&ty.name) {
            return Some((ty.name.clone(), variants.clone()));
        }
        if let Some(payload) = ty.option_element() {
            return Some((
                ty.name.clone(),
                vec![
                    EnumVariant {
                        name: "some".to_string(),
                        payload: vec![payload],
                    },
                    EnumVariant {
                        name: "none".to_string(),
                        payload: Vec::new(),
                    },
                ],
            ));
        }
        if let Some((ok_ty, err_ty)) = ty.result_args() {
            return Some((
                ty.name.clone(),
                vec![
                    EnumVariant {
                        name: "ok".to_string(),
                        payload: vec![ok_ty],
                    },
                    EnumVariant {
                        name: "err".to_string(),
                        payload: vec![err_ty],
                    },
                ],
            ));
        }
        None
    }

    fn check_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let scrutinee_type = self.check_expr(scrutinee, scope, function);
        let (enum_name, declared_variants) = match scrutinee_type
            .as_ref()
            .and_then(|ty| self.match_variants(ty))
        {
            Some(pair) => pair,
            None => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0383",
                    format!(
                        "match scrutinee must be an enum type but got `{}`",
                        scrutinee_type
                            .as_ref()
                            .map(|ty| ty.name.as_str())
                            .unwrap_or("<unknown>")
                    ),
                    Some(function.name.clone()),
                    scrutinee.span,
                ));
                // Still check arm bodies to surface nested errors.
                for arm in arms {
                    let mut arm_scope = scope.clone();
                    self.check_block(&arm.body, &mut arm_scope, function);
                }
                return None;
            }
        };
        let mut covered: HashSet<String> = HashSet::new();
        let mut has_wildcard = false;
        let mut arm_types: Vec<Option<TypeRef>> = Vec::new();

        for arm in arms {
            let mut arm_scope = scope.clone();
            match &arm.pattern {
                MatchPattern::Wildcard => {
                    has_wildcard = true;
                }
                MatchPattern::Variant { name, bindings } => {
                    match declared_variants.iter().find(|v| &v.name == name) {
                        Some(variant) => {
                            if !covered.insert(name.clone()) {
                                self.diagnostics.push(SemanticDiagnostic::at(
                                    "L0385",
                                    format!("duplicate match arm for variant `{name}`"),
                                    Some(function.name.clone()),
                                    span,
                                ));
                            }
                            if bindings.len() != variant.payload.len() {
                                self.diagnostics.push(SemanticDiagnostic::at(
                                    "L0385",
                                    format!(
                                        "variant `{name}` binds {} value(s) but declares {} payload type(s)",
                                        bindings.len(),
                                        variant.payload.len()
                                    ),
                                    Some(function.name.clone()),
                                    span,
                                ));
                            }
                            // Bind each payload to an arm-scoped local typed by
                            // the variant's declared payload type. When arities
                            // differ, bind the overlap so nested checks proceed.
                            for (binding, ty) in bindings.iter().zip(variant.payload.iter()) {
                                arm_scope.locals.insert(binding.clone(), ty.clone());
                            }
                        }
                        None => {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0385",
                                format!("variant `{name}` does not belong to enum `{enum_name}`"),
                                Some(function.name.clone()),
                                span,
                            ));
                        }
                    }
                }
            }
            arm_types.push(self.check_block(&arm.body, &mut arm_scope, function));
        }

        // Exhaustiveness: every variant covered, or a `_` wildcard present.
        if !has_wildcard {
            let missing: Vec<String> = declared_variants
                .iter()
                .filter(|v| !covered.contains(&v.name))
                .map(|v| v.name.clone())
                .collect();
            if !missing.is_empty() {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0384",
                    format!(
                        "match over enum `{enum_name}` is not exhaustive; missing variant(s): {}",
                        missing.join(", ")
                    ),
                    Some(function.name.clone()),
                    span,
                ));
            }
        }

        // Result type: the common arm body type when every arm agrees.
        match arm_types.split_first() {
            Some((first, rest)) if rest.iter().all(|ty| ty.as_ref() == first.as_ref()) => {
                first.clone()
            }
            _ => None,
        }
    }

    /// Validate named-field construction `Name(field: expr, ...)`: every
    /// declared field must appear exactly once with a matching type, in any
    /// order. Reuses the positional construction diagnostic code `L0372`.
    fn check_struct_literal(
        &mut self,
        name: &str,
        fields: &[(String, Expr)],
        span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        if !self.structs.contains_key(name) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0372",
                format!("`{name}` is not a struct type"),
                Some(function.name.clone()),
                span,
            ));
            // Still type-check the field expressions to surface nested errors.
            for (_, expr) in fields {
                self.check_expr(expr, scope, function);
            }
            return None;
        }
        let declared = self.structs.get(name).cloned()?;
        // Type-check each provided field value against its declared type.
        for (field_name, expr) in fields {
            let value_type = self.check_expr(expr, scope, function);
            match declared.iter().find(|f| &f.name == field_name) {
                Some(field) => {
                    if value_type.as_ref() != Some(&field.ty) {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0372",
                            format!(
                                "field `{field_name}` of struct `{name}` expects `{}` but got `{}`",
                                field.ty.name,
                                value_type
                                    .as_ref()
                                    .map(|ty| ty.name.as_str())
                                    .unwrap_or("<unknown>")
                            ),
                            Some(function.name.clone()),
                            expr.span,
                        ));
                    }
                }
                None => {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0372",
                        format!("struct `{name}` has no field `{field_name}`"),
                        Some(function.name.clone()),
                        expr.span,
                    ));
                }
            }
        }
        // Every declared field must be provided exactly once.
        for field in &declared {
            let count = fields.iter().filter(|(n, _)| n == &field.name).count();
            if count == 0 {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0372",
                    format!(
                        "named construction of `{name}` is missing field `{}`",
                        field.name
                    ),
                    Some(function.name.clone()),
                    span,
                ));
            } else if count > 1 {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0372",
                    format!(
                        "field `{}` of struct `{name}` is set more than once",
                        field.name
                    ),
                    Some(function.name.clone()),
                    span,
                ));
            }
        }
        Some(TypeRef::new(name))
    }

    /// Verify `ty` is a `<ctor><T>` reference (`rc` or `ref`) and return its
    /// inner type `T`.
    fn expect_reference(
        &mut self,
        name: &str,
        ctor: &str,
        ty: &TypeRef,
        span: Span,
        function: &Function,
    ) -> Option<TypeRef> {
        match ty.generic_arg(ctor) {
            Some(inner) => Some(inner),
            None => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0331",
                    format!("{name} expects a `{ctor}<T>` value but got `{}`", ty.name),
                    Some(function.name.clone()),
                    span,
                ));
                None
            }
        }
    }

    /// Verify `ty` is a `list<T>` and return its element type `T`.
    fn expect_list_arg(
        &mut self,
        name: &str,
        ty: &TypeRef,
        span: Span,
        function: &Function,
    ) -> Option<TypeRef> {
        match list_element(ty) {
            Some(element) => Some(element),
            None => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0387",
                    format!("`{name}` expects a `list<T>` value but got `{}`", ty.name),
                    Some(function.name.clone()),
                    span,
                ));
                None
            }
        }
    }

    /// Check a function-valued argument at position `index` (1-based) of a
    /// higher-order list builtin. The argument must be a `fn(...)` value whose
    /// parameter types equal `expected_params`; when `expected_ret` is `Some`,
    /// the return type must match it too. On success the function's
    /// `(param types, return type)` is returned. Failures reuse the general
    /// `list<T>` builtin diagnostic `L0387` (a mismatched or non-function
    /// argument to a list builtin).
    fn expect_fn_arg(
        &mut self,
        name: &str,
        index: usize,
        arg: &Expr,
        expected: (&[TypeRef], Option<&TypeRef>),
        scope: &Scope,
        function: &Function,
    ) -> Option<(Vec<TypeRef>, TypeRef)> {
        let (expected_params, expected_ret) = expected;
        let arg_ty = self.check_expr(arg, scope, function)?;
        let Some((params, ret)) = arg_ty.function_signature() else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0387",
                format!(
                    "`{name}` argument {index} must be a function value but got `{}`",
                    arg_ty.name
                ),
                Some(function.name.clone()),
                arg.span,
            ));
            return None;
        };
        if params != expected_params || expected_ret.is_some_and(|expected| &ret != expected) {
            let expected_ret_name = expected_ret.map(|ty| ty.name.as_str()).unwrap_or("U");
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0387",
                format!(
                    "`{name}` argument {index} must be a `{}` but got `{}`",
                    function_type(expected_params, &TypeRef::new(expected_ret_name)).name,
                    arg_ty.name
                ),
                Some(function.name.clone()),
                arg.span,
            ));
            return None;
        }
        Some((params, ret))
    }

    /// Verify `ty` is a `map<K, V>` and return its `(K, V)` pair.
    fn expect_map_arg(
        &mut self,
        name: &str,
        ty: &TypeRef,
        span: Span,
        function: &Function,
    ) -> Option<(TypeRef, TypeRef)> {
        match map_kv(ty) {
            Some(pair) => Some(pair),
            None => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0388",
                    format!("`{name}` expects a `map<K, V>` value but got `{}`", ty.name),
                    Some(function.name.clone()),
                    span,
                ));
                None
            }
        }
    }

    /// Verify `ty` is a raw pointer and return its pointee type.
    /// Whether `ty` has a defined C-natural raw-memory layout: a scalar, a
    /// pointer/reference handle (`ptr<T>`/`rc<T>`/`ref<T>`, all 8 bytes), a
    /// fixed `array<T>` whose element is itself sized, or a declared struct
    /// whose every field is sized (recursively, rejecting a by-value cycle).
    /// Drives `size_of`/`align_of`/`offset_of`. See
    /// `documents/lullaby_memory_management.md`.
    fn type_has_layout(&self, ty: &TypeRef) -> bool {
        self.type_layout_ok(ty, &mut Vec::new())
    }

    fn type_layout_ok(&self, ty: &TypeRef, stack: &mut Vec<String>) -> bool {
        const SCALARS: &[&str] = &[
            "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "isize", "usize", "f32", "f64",
            "bool", "byte", "char",
        ];
        if SCALARS.contains(&ty.name.as_str()) {
            return true;
        }
        // Pointer and reference handles are opaque 8-byte cells; their pointee
        // layout is irrelevant, so they never recurse.
        if ty.is_raw_pointer() || ty.is_safe_reference() {
            return true;
        }
        if let Some(element) = ty.array_element() {
            return self.type_layout_ok(&element, stack);
        }
        if let Some(fields) = self.structs.get(&ty.name) {
            // A struct that (transitively) contains itself by value has no finite
            // size, so its layout is undefined.
            if stack.iter().any(|name| name == &ty.name) {
                return false;
            }
            stack.push(ty.name.clone());
            let ok = fields
                .iter()
                .all(|field| self.type_layout_ok(&field.ty, stack));
            stack.pop();
            return ok;
        }
        false
    }

    fn expect_raw_pointer(
        &mut self,
        name: &str,
        ty: &TypeRef,
        span: Span,
        function: &Function,
    ) -> Option<TypeRef> {
        match ty.pointer_target() {
            Some(inner) => Some(inner),
            None => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0331",
                    format!("{name} expects a raw pointer value but got `{}`", ty.name),
                    Some(function.name.clone()),
                    span,
                ));
                None
            }
        }
    }

    /// Validate an `asm` inline-assembly statement: it must sit inside an
    /// `unsafe` block (inline machine code is inherently unsafe) and every byte
    /// literal must be in `0..=255`. The statement is native-only, so this is the
    /// only place its shape is checked; the interpreters reject it at runtime with
    /// `L0425`, and the native backend emits the bytes verbatim.
    fn check_asm(&mut self, bytes: &[i64], span: Span, function: &Function) {
        if self.unsafe_depth == 0 {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0330",
                "`asm` inline assembly requires an `unsafe` block".to_string(),
                Some(function.name.clone()),
                span,
            ));
        }
        if bytes.is_empty() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0425",
                "`asm` statement must emit at least one byte".to_string(),
                Some(function.name.clone()),
                span,
            ));
        }
        for byte in bytes {
            if !(0..=255).contains(byte) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0425",
                    format!("`asm` byte value {byte} is out of range; each byte must be 0..=255"),
                    Some(function.name.clone()),
                    span,
                ));
            }
        }
    }

    /// Require the current context to be inside an `unsafe` block.
    fn require_unsafe(&mut self, name: &str, span: Span, function: &Function) -> Option<()> {
        if self.unsafe_depth > 0 {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0330",
                format!("raw pointer operation `{name}` requires an `unsafe` block"),
                Some(function.name.clone()),
                span,
            ));
            None
        }
    }

    fn expect_arg_count(
        &mut self,
        name: &str,
        args: &[Expr],
        expected: usize,
        function: &Function,
    ) -> Option<()> {
        if args.len() == expected {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0312",
                format!(
                    "function `{name}` expects {expected} arguments but got {}",
                    args.len()
                ),
                Some(function.name.clone()),
                args.first().map(|arg| arg.span).unwrap_or(function.span),
            ));
            None
        }
    }

    fn expect_arg_type(
        &mut self,
        name: &str,
        index: usize,
        arg: &Expr,
        expected: &str,
        scope: &Scope,
        function: &Function,
    ) -> Option<()> {
        let expected = TypeRef::new(expected);
        let actual = self.check_expr(arg, scope, function);
        if actual.as_ref() == Some(&expected) {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0313",
                format!(
                    "argument {index} for `{name}` must be `{}` but got `{}`",
                    expected.name,
                    actual
                        .as_ref()
                        .map(|ty| ty.name.as_str())
                        .unwrap_or("<unknown>")
                ),
                Some(function.name.clone()),
                arg.span,
            ));
            None
        }
    }

    /// Validate a file-system builtin argument count, reporting `L0333` on a
    /// mismatch.
    fn expect_fs_arg_count(
        &mut self,
        name: &str,
        args: &[Expr],
        expected: usize,
        function: &Function,
    ) -> Option<()> {
        if args.len() == expected {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0333",
                format!(
                    "file-system builtin `{name}` expects {expected} arguments but got {}",
                    args.len()
                ),
                Some(function.name.clone()),
                args.first().map(|arg| arg.span).unwrap_or(function.span),
            ));
            None
        }
    }

    /// Validate a file-system builtin argument type, reporting `L0333` on a
    /// mismatch.
    fn expect_fs_arg_type(
        &mut self,
        name: &str,
        index: usize,
        arg: &Expr,
        expected: &str,
        scope: &Scope,
        function: &Function,
    ) -> Option<()> {
        let expected = TypeRef::new(expected);
        let actual = self.check_expr(arg, scope, function);
        if actual.as_ref() == Some(&expected) {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0333",
                format!(
                    "argument {index} for `{name}` must be `{}` but got `{}`",
                    expected.name,
                    actual
                        .as_ref()
                        .map(|ty| ty.name.as_str())
                        .unwrap_or("<unknown>")
                ),
                Some(function.name.clone()),
                arg.span,
            ));
            None
        }
    }

    /// Validate a socket/network builtin argument count, reporting `L0335` on a
    /// mismatch.
    fn expect_socket_arg_count(
        &mut self,
        name: &str,
        args: &[Expr],
        expected: usize,
        function: &Function,
    ) -> Option<()> {
        if args.len() == expected {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0335",
                format!(
                    "socket builtin `{name}` expects {expected} arguments but got {}",
                    args.len()
                ),
                Some(function.name.clone()),
                args.first().map(|arg| arg.span).unwrap_or(function.span),
            ));
            None
        }
    }

    /// Validate a concurrency builtin argument count, reporting `L0337` on a
    /// mismatch.
    fn expect_concurrency_arity(
        &mut self,
        name: &str,
        args: &[Expr],
        expected: usize,
        call_span: Span,
        function: &Function,
    ) -> Option<()> {
        if args.len() == expected {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0337",
                format!(
                    "concurrency builtin `{name}` expects {expected} arguments but got {}",
                    args.len()
                ),
                Some(function.name.clone()),
                args.first().map(|arg| arg.span).unwrap_or(call_span),
            ));
            None
        }
    }

    /// Validate a concurrency builtin argument type, reporting `L0337` on a
    /// mismatch.
    fn expect_concurrency_arg(
        &mut self,
        name: &str,
        index: usize,
        arg: &Expr,
        expected: &str,
        scope: &Scope,
        function: &Function,
    ) -> Option<()> {
        let expected = TypeRef::new(expected);
        let actual = self.check_expr(arg, scope, function);
        if actual.as_ref() == Some(&expected) {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0337",
                format!(
                    "argument {index} for `{name}` must be `{}` but got `{}`",
                    expected.name,
                    actual
                        .as_ref()
                        .map(|ty| ty.name.as_str())
                        .unwrap_or("<unknown>")
                ),
                Some(function.name.clone()),
                arg.span,
            ));
            None
        }
    }

    /// Validate a `MemoryOrder` ordering argument for an ordering-taking atomic
    /// builtin or `fence`. The argument must first type-check as `MemoryOrder`
    /// (`L0337`). When it is a literal ordering variant (a bare `acquire`/…, not
    /// a local of `MemoryOrder` type), the ordering is additionally checked
    /// against `allowed` for this operation and an invalid combination — a
    /// `release` load, an `acquire` store, a `relaxed` fence, and so on — is
    /// rejected statically with `L0432`. A dynamically chosen `MemoryOrder`
    /// (passed through a variable) type-checks here and is guarded at runtime.
    fn check_ordering_arg(
        &mut self,
        name: &str,
        index: usize,
        arg: &Expr,
        allowed: &[&str],
        scope: &Scope,
        function: &Function,
    ) -> Option<()> {
        self.expect_concurrency_arg(name, index, arg, "MemoryOrder", scope, function)?;
        if let ExprKind::Variable(variant) = &arg.kind
            && !scope.locals.contains_key(variant)
            && MEMORY_ORDER_VARIANTS.contains(&variant.as_str())
            && !allowed.contains(&variant.as_str())
        {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0432",
                format!("`{variant}` is not a valid memory ordering for `{name}`"),
                Some(function.name.clone()),
                arg.span,
            ));
            return None;
        }
        Some(())
    }

    /// Validate a socket/network builtin argument type, reporting `L0335` on a
    /// mismatch.
    fn expect_socket_arg_type(
        &mut self,
        name: &str,
        index: usize,
        arg: &Expr,
        expected: &str,
        scope: &Scope,
        function: &Function,
    ) -> Option<()> {
        let expected = TypeRef::new(expected);
        let actual = self.check_expr(arg, scope, function);
        if actual.as_ref() == Some(&expected) {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0335",
                format!(
                    "argument {index} for `{name}` must be `{}` but got `{}`",
                    expected.name,
                    actual
                        .as_ref()
                        .map(|ty| ty.name.as_str())
                        .unwrap_or("<unknown>")
                ),
                Some(function.name.clone()),
                arg.span,
            ));
            None
        }
    }

    /// Validate an HTTP client builtin argument count, reporting `L0336` on a
    /// mismatch.
    fn expect_http_arg_count(
        &mut self,
        name: &str,
        args: &[Expr],
        expected: usize,
        function: &Function,
    ) -> Option<()> {
        if args.len() == expected {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0336",
                format!(
                    "http builtin `{name}` expects {expected} arguments but got {}",
                    args.len()
                ),
                Some(function.name.clone()),
                args.first().map(|arg| arg.span).unwrap_or(function.span),
            ));
            None
        }
    }

    /// Validate an HTTP client builtin `string` argument, reporting `L0336` on a
    /// mismatch.
    fn expect_http_arg_type(
        &mut self,
        name: &str,
        index: usize,
        arg: &Expr,
        scope: &Scope,
        function: &Function,
    ) -> Option<()> {
        let expected = TypeRef::new("string");
        let actual = self.check_expr(arg, scope, function);
        if actual.as_ref() == Some(&expected) {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0336",
                format!(
                    "argument {index} for `{name}` must be `string` but got `{}`",
                    actual
                        .as_ref()
                        .map(|ty| ty.name.as_str())
                        .unwrap_or("<unknown>")
                ),
                Some(function.name.clone()),
                arg.span,
            ));
            None
        }
    }

    /// Validate a `char`/`byte` builtin argument against an expected type,
    /// reporting `L0389` on a mismatch.
    fn expect_scalar_builtin_arg(
        &mut self,
        name: &str,
        index: usize,
        arg: &Expr,
        expected: &str,
        scope: &Scope,
        function: &Function,
    ) -> Option<()> {
        let expected = TypeRef::new(expected);
        let actual = self.check_expr(arg, scope, function);
        if actual.as_ref() == Some(&expected) {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0389",
                format!(
                    "argument {index} for `{name}` must be `{}` but got `{}`",
                    expected.name,
                    actual
                        .as_ref()
                        .map(|ty| ty.name.as_str())
                        .unwrap_or("<unknown>")
                ),
                Some(function.name.clone()),
                arg.span,
            ));
            None
        }
    }

    /// Validate a process/environment builtin (`env`/`args`) argument count,
    /// reporting `L0332` on a mismatch.
    fn expect_process_arg_count(
        &mut self,
        name: &str,
        args: &[Expr],
        expected: usize,
        call_span: Span,
        function: &Function,
    ) -> Option<()> {
        if args.len() == expected {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0332",
                format!(
                    "process builtin `{name}` expects {expected} arguments but got {}",
                    args.len()
                ),
                Some(function.name.clone()),
                args.first().map(|arg| arg.span).unwrap_or(call_span),
            ));
            None
        }
    }

    /// Validate a process/environment builtin (`env`) argument against an
    /// expected type, reporting `L0332` on a mismatch.
    fn expect_process_arg(
        &mut self,
        name: &str,
        index: usize,
        arg: &Expr,
        expected: &str,
        scope: &Scope,
        function: &Function,
    ) -> Option<()> {
        let expected = TypeRef::new(expected);
        let actual = self.check_expr(arg, scope, function);
        if actual.as_ref() == Some(&expected) {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0332",
                format!(
                    "argument {index} for `{name}` must be `{}` but got `{}`",
                    expected.name,
                    actual
                        .as_ref()
                        .map(|ty| ty.name.as_str())
                        .unwrap_or("<unknown>")
                ),
                Some(function.name.clone()),
                arg.span,
            ));
            None
        }
    }

    /// Validate a string-library builtin argument against an expected type,
    /// reporting `L0375` on a mismatch.
    fn expect_string_builtin_arg(
        &mut self,
        name: &str,
        index: usize,
        arg: &Expr,
        expected: &str,
        scope: &Scope,
        function: &Function,
    ) -> Option<()> {
        let expected = TypeRef::new(expected);
        let actual = self.check_expr(arg, scope, function);
        if actual.as_ref() == Some(&expected) {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0375",
                format!(
                    "argument {index} for `{name}` must be `{}` but got `{}`",
                    expected.name,
                    actual
                        .as_ref()
                        .map(|ty| ty.name.as_str())
                        .unwrap_or("<unknown>")
                ),
                Some(function.name.clone()),
                arg.span,
            ));
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub params: Vec<TypeRef>,
    pub return_type: TypeRef,
    /// Declared type-parameter names `<T, U>`. Empty for a non-generic function.
    /// When non-empty, a call site infers each name from the argument types by
    /// unification and substitutes the result into `return_type`.
    pub type_params: Vec<String>,
    /// Trait bounds per type parameter, in `type_params` order: each entry is the
    /// list of trait names the type variable must satisfy (empty when unbounded).
    /// A call site must check the inferred concrete type implements every bound
    /// (`L0400`).
    pub type_param_bounds: Vec<Vec<String>>,
    /// True when the function is declared `async fn`. A call to it produces a
    /// `Future<return_type>` rather than `return_type` directly, resolved by
    /// `await`.
    pub is_async: bool,
    /// True when the function is a body-less `extern fn` (an imported C symbol).
    /// A `cstr` parameter of an extern accepts a Lullaby `string` argument (the
    /// caller materializes a NUL-terminated copy across the FFI boundary); no
    /// other call path admits `string` where a `cstr` is declared.
    pub is_extern: bool,
}

#[derive(Debug, Clone, Default)]
struct Scope {
    locals: HashMap<String, TypeRef>,
}

/// The `Future<T>` type spelling for an inner type `T`, matching the canonical
/// generic spelling used everywhere else (`Future<i64>`, `Future<option<i64>>`).
fn future_type(inner: &TypeRef) -> TypeRef {
    TypeRef::new(format!("Future<{}>", inner.name))
}

/// The awaited inner type of a `Future<T>` spelling, if `ty` is one.
fn future_inner(ty: &TypeRef) -> Option<TypeRef> {
    ty.generic_arg("Future")
}

/// Result types an inline conditional (`THEN if COND else ELSE`) may produce.
/// Limited to scalars and `string` because the IR desugar seeds a
/// zero-initialized temporary, which only has a well-typed zero for these.
fn is_inline_conditional_type(ty: &TypeRef) -> bool {
    matches!(
        ty.name.as_str(),
        "i64"
            | "i8"
            | "i16"
            | "i32"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "isize"
            | "usize"
            | "byte"
            | "bool"
            | "char"
            | "f64"
            | "f32"
            | "string"
    )
}

/// If both operand types are the same numeric type (`i64` or `f64`), return it.
fn same_numeric_type(left: &Option<TypeRef>, right: &Option<TypeRef>) -> Option<TypeRef> {
    match (left, right) {
        (Some(l), Some(r)) if l == r && is_numeric_type_name(&l.name) => Some(l.clone()),
        _ => None,
    }
}

/// True for every scalar numeric type: the default `i64`/`f64` plus the
/// fixed-width integer lattice. Arithmetic and ordering require both operands to
/// share one of these (no implicit width mixing); the shared type is the result.
fn is_numeric_type_name(name: &str) -> bool {
    matches!(
        name,
        "i64"
            | "f64"
            | "f32"
            | "i8"
            | "i16"
            | "i32"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "isize"
            | "usize"
    )
}

/// The fixed-width integer type names produced by the `to_<T>` conversions (the
/// numeric lattice minus the default `i64`/`f64`). A `to_i64` argument must be
/// one of these.
fn is_fixed_width_int_name(name: &str) -> bool {
    matches!(
        name,
        "i8" | "i16" | "i32" | "u8" | "u16" | "u32" | "u64" | "isize" | "usize"
    )
}

/// True when both operands are the same orderable scalar type beyond the numeric
/// ones — `char` (ordered by code point) or `byte` (ordered numerically).
fn same_orderable_scalar(left: &Option<TypeRef>, right: &Option<TypeRef>) -> bool {
    matches!(
        (left, right),
        (Some(l), Some(r)) if l == r && matches!(l.name.as_str(), "char" | "byte")
    )
}

/// If `expr` is a resource-freeing call (`dealloc(x)` or `rc_release(x)`) whose
/// argument is a plain variable, return that variable name.
fn free_call_target(expr: &Expr) -> Option<&str> {
    let ExprKind::Call { name, args } = &expr.kind else {
        return None;
    };
    if !matches!(name.as_str(), "dealloc" | "rc_release") {
        return None;
    }
    match args.as_slice() {
        [arg] => match &arg.kind {
            ExprKind::Variable(name) => Some(name),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
#[path = "semantics_tests.rs"]
mod tests;
