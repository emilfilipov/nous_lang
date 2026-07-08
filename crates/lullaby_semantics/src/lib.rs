use std::collections::{HashMap, HashSet};

use lullaby_diagnostics::Span;
use lullaby_parser::{
    AssignOp, BinaryOp, EnumDecl, EnumVariant, Expr, ExprKind, Function, IfBranch, MatchArm,
    MatchPattern, MethodSig, Param, Place, Program, RegionDecl, Stmt, StructDecl, StructField,
    TypeRef, UnaryOp, function_type, generic_type,
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
    let (resolved, alias_diagnostics) = resolve_program_aliases(program);

    let mut checker = Checker::new(&resolved);
    checker.diagnostics = alias_diagnostics;
    checker.validate();
    if !checker.diagnostics.is_empty() {
        return Err(std::mem::take(&mut checker.diagnostics));
    }

    let signatures = std::mem::take(&mut checker.signatures);
    let expression_types = std::mem::take(&mut checker.expression_types);
    drop(checker);
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
        }
    }

    fn validate(&mut self) {
        self.collect_structs();
        self.collect_enums();
        self.collect_traits();
        self.collect_signatures();
        self.collect_impls();
        for function in &self.program.functions {
            // Extern (C-ABI) declarations are body-less: there is nothing to
            // check beyond the signature, which `collect_signatures` already
            // registered so call sites type-check.
            if function.is_extern {
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

    /// Check that an `export fn` has an i64-scalar C-callable signature. The first
    /// increment exposes exports across the Win64 integer convention only, so
    /// every parameter and the return type must be `i64`; anything else is
    /// `L0424`. Generic exports are also rejected (a C symbol is monomorphic).
    fn check_export_signature(&mut self, function: &Function) {
        if !function.type_params.is_empty() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0424",
                format!(
                    "`export fn {}` cannot be generic; the first increment exports only i64-scalar functions",
                    function.name
                ),
                Some(function.name.clone()),
                function.span,
            ));
        }
        for param in &function.params {
            if param.ty.name != "i64" {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0424",
                    format!(
                        "`export fn {}` parameter `{}` has type `{}`; the first increment exports only i64 parameters and return type",
                        function.name, param.name, param.ty.name
                    ),
                    Some(function.name.clone()),
                    function.span,
                ));
            }
        }
        if function.return_type.name != "i64" {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0424",
                format!(
                    "`export fn {}` returns `{}`; the first increment exports only functions returning i64",
                    function.name, function.return_type.name
                ),
                Some(function.name.clone()),
                function.span,
            ));
        }
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
                },
            );
        }
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
        if function.return_type.is_void() {
            return;
        }

        if block_type.as_ref() != Some(&function.return_type) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0301",
                format!(
                    "function `{}` declares `{}` but has no final return value of that type",
                    function.name, function.return_type.name
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
                    if Some(index) == last_index && !function.return_type.is_void() =>
                {
                    self.check_expr_expected(expr, Some(&function.return_type), scope, function)
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
                    None => value_type
                        .clone()
                        .unwrap_or_else(|| TypeRef::new("<unknown>")),
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
                let actual = expr
                    .as_ref()
                    .map(|expr| {
                        self.check_expr_expected(expr, Some(&function.return_type), scope, function)
                    })
                    .unwrap_or_else(|| Some(TypeRef::new("void")));
                if actual.as_ref() != Some(&function.return_type) {
                    let span = expr.as_ref().map(|expr| expr.span).unwrap_or(function.span);
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0304",
                        format!(
                            "return type `{}` does not match function return `{}`",
                            actual
                                .as_ref()
                                .map(|ty| ty.name.as_str())
                                .unwrap_or("<unknown>"),
                            function.return_type.name
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

                match target_type.and_then(|ty| ty.array_element()) {
                    Some(element_type) => Some(element_type),
                    None => {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0325",
                            "index target must be an array",
                            Some(function.name.clone()),
                            target.span,
                        ));
                        None
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
                        // Bitwise NOT is `i64 -> i64` only.
                        if expr_type.as_ref() == Some(&TypeRef::new("i64")) {
                            Some(TypeRef::new("i64"))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0307",
                                "operand of `~` must be i64",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
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
                        if let Some(numeric) = same_numeric.clone() {
                            Some(numeric)
                        } else if left_type.as_ref() == Some(&string_type)
                            && right_type.as_ref() == Some(&string_type)
                        {
                            // `+` concatenates two strings.
                            Some(string_type)
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0307",
                                "operands of `+` must both be the same numeric type or both be string",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                    BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide => {
                        if let Some(numeric) = same_numeric.clone() {
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
                        // (i64, f64, i32, u32), two chars (by code point), or two
                        // bytes (numerically).
                        if same_numeric.is_some() || same_orderable_scalar(&left_type, &right_type)
                        {
                            Some(TypeRef::new("bool"))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0327",
                                "ordering comparison operands must both be the same numeric type, both be char, or both be byte",
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
                        // Integer bitwise ops are `i64 x i64 -> i64` only. Byte
                        // and wider-integer bitwise is deferred to its own ticket.
                        let i64_type = TypeRef::new("i64");
                        if left_type.as_ref() == Some(&i64_type)
                            && right_type.as_ref() == Some(&i64_type)
                        {
                            Some(i64_type)
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "L0307",
                                "bitwise operands (`& | ^ << >>`) must both be i64",
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
                "array literals must contain at least one value in the current alpha",
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
                self.expect_arg_count(name, args, 1, function)?;
                let list_ty = self.check_expr_expected(&args[0], expected, scope, function)?;
                let element = self.expect_list_arg(name, &list_ty, args[0].span, function)?;
                if element.name != "i64" {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0387",
                        format!(
                            "`sort` expects a `list<i64>` but got `list<{}>`",
                            element.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    return None;
                }
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
            "to_i8" | "to_i16" | "to_i32" | "to_u16" | "to_u32" | "to_u64" | "to_isize"
            | "to_usize" => {
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
                .get(&(dispatch.clone(), method.to_string()))
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
        "i64" | "f64" | "f32" | "i8" | "i16" | "i32" | "u16" | "u32" | "u64" | "isize" | "usize"
    )
}

/// The fixed-width integer type names produced by the `to_<T>` conversions (the
/// numeric lattice minus the default `i64`/`f64`). A `to_i64` argument must be
/// one of these.
fn is_fixed_width_int_name(name: &str) -> bool {
    matches!(
        name,
        "i8" | "i16" | "i32" | "u16" | "u32" | "u64" | "isize" | "usize"
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
mod tests {
    use lullaby_lexer::lex;
    use lullaby_parser::parse;

    use super::*;

    fn validate_source(source: &str) -> Result<CheckedProgram, Vec<SemanticDiagnostic>> {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        validate(&program)
    }

    #[test]
    fn non_void_function_may_return_last_expression() {
        assert!(validate_source("fn add x i64 y i64 -> i64\n    x + y\n").is_ok());
    }

    #[test]
    fn accepts_i64_bitwise_operators() {
        // `& | ^ << >>` and unary `~` are all `i64 -> i64`.
        let source = concat!(
            "fn main -> i64\n",
            "    let a i64 = 6 & 3\n",
            "    let b i64 = 6 | 1\n",
            "    let c i64 = 6 ^ 3\n",
            "    let d i64 = 1 << 4\n",
            "    let e i64 = 64 >> 2\n",
            "    let f i64 = ~0\n",
            "    a + b + c + d + e + f\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_non_i64_bitwise_operand() {
        // A `bool` operand to a bitwise op reuses the arithmetic operand family
        // (`L0307`); bitwise ops are strictly `i64`.
        let source = "fn main -> i64\n    let x bool = true\n    x & 1\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0307"),
            "expected L0307 for a non-i64 bitwise operand: {diagnostics:?}"
        );
    }

    #[test]
    fn rejects_non_i64_bitwise_not_operand() {
        let source = "fn main -> i64\n    let x f64 = 1.0\n    ~x\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0307"),
            "expected L0307 for a non-i64 `~` operand: {diagnostics:?}"
        );
    }

    #[test]
    fn validates_extern_declaration_and_call() {
        // A body-less `extern fn` registers a signature so calls type-check like
        // any other call (arity + i64 argument/return types), even though it has
        // no body to validate.
        let source = "extern fn llabs x i64 -> i64\n\nfn main -> i64\n    llabs(-7)\n";
        let checked = validate_source(source).expect("extern decl + call type-checks");
        let extern_fn = checked
            .program
            .functions
            .iter()
            .find(|f| f.name == "llabs")
            .expect("extern function present");
        assert!(extern_fn.is_extern, "llabs is marked extern");
        assert!(extern_fn.body.is_empty(), "extern function has no body");
    }

    #[test]
    fn validates_export_i64_scalar_function() {
        // An `export fn` with an all-i64 signature and a body type-checks like any
        // ordinary function; the `is_export` marker is preserved.
        let source = "export fn add_seven x i64 -> i64\n    x + 7\n";
        let checked = validate_source(source).expect("i64 export type-checks");
        let export_fn = checked
            .program
            .functions
            .iter()
            .find(|f| f.name == "add_seven")
            .expect("export function present");
        assert!(export_fn.is_export, "add_seven is marked export");
    }

    #[test]
    fn rejects_export_with_non_i64_signature() {
        // The first export increment supports only i64 params/return; a string
        // return is `L0424`.
        let source = "export fn label x i64 -> string\n    to_string(x)\n";
        let diagnostics = validate_source(source).expect_err("non-i64 export rejected");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0424"),
            "expected L0424: {diagnostics:?}"
        );
    }

    #[test]
    fn extern_call_arity_mismatch_is_reported() {
        // Extern call sites are checked like ordinary calls: wrong arity is L0312.
        let source = "extern fn llabs x i64 -> i64\n\nfn main -> i64\n    llabs(1, 2)\n";
        let diagnostics = validate_source(source).expect_err("arity mismatch");
        assert!(diagnostics.iter().any(|d| d.code == "L0312"));
    }

    #[test]
    fn validates_calls_and_bindings() {
        let source = "fn add x i64 y i64 -> i64\n    x + y\n\nfn main -> i64\n    let value i64 = add(1, 2)\n    value\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_inferred_bindings() {
        let source = "fn add x i64 y i64 -> i64\n    x + y\n\nfn main -> i64\n    let value = add(1, 2)\n    let values = [value, 4]\n    values[0]\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_memory_builtins() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_store_builtin() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_assignment_and_loops() {
        let source = "fn main -> i64\n    let x i64 = 0\n    while x < 3\n        x += 1\n    x\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_for_loop() {
        let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    total\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_logical_expressions() {
        let source = "fn main -> bool\n    not false and true or false\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_array_literal_and_index() {
        let source = "fn main -> i64\n    let values array<i64> = [1, 2, 3]\n    values[1]\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn checked_program_exposes_function_signatures() {
        let checked = validate_source("fn add x i64 y i64 -> i64\n    x + y\n").expect("semantic");
        let signature = checked.info.signatures.get("add").expect("signature");
        assert_eq!(
            signature.params,
            vec![TypeRef::new("i64"), TypeRef::new("i64")]
        );
        assert_eq!(signature.return_type, TypeRef::new("i64"));
    }

    #[test]
    fn checked_program_exposes_expression_types() {
        let checked = validate_source(
            "fn main -> i64\n    let values array<i64> = [1, 2, 3]\n    values[1]\n",
        )
        .expect("semantic");
        assert!(checked.info.expression_types.iter().any(|expr_type| {
            expr_type.function == "main" && expr_type.ty == TypeRef::new("array<i64>")
        }));
        assert!(checked.info.expression_types.iter().any(|expr_type| {
            expr_type.function == "main" && expr_type.ty == TypeRef::new("i64")
        }));
    }

    #[test]
    fn non_void_function_rejects_empty_return() {
        let diagnostics = validate_source("fn bad -> i64\n    return\n").expect_err("semantic");
        assert_eq!(diagnostics[0].code, "L0304");
    }

    #[test]
    fn catches_type_mismatch() {
        let diagnostics = validate_source("fn bad -> i64\n    let value bool = 1\n    value\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0303")
        );
    }

    #[test]
    fn accepts_first_class_function_values() {
        let source = concat!(
            "fn inc x i64 -> i64\n",
            "    x + 1\n\n",
            "fn apply f fn(i64) -> i64 v i64 -> i64\n",
            "    f(v)\n\n",
            "fn main -> i64\n",
            "    let g fn(i64) -> i64 = inc\n",
            "    apply(inc, 10) + g(5)\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn function_name_is_a_function_value() {
        let source = concat!(
            "fn inc x i64 -> i64\n",
            "    x + 1\n\n",
            "fn main -> fn(i64) -> i64\n",
            "    inc\n",
        );
        let checked = validate_source(source).expect("semantic");
        assert_eq!(
            checked
                .info
                .signatures
                .get("main")
                .expect("main")
                .return_type,
            TypeRef::new("fn(i64) -> i64")
        );
    }

    #[test]
    fn rejects_calling_a_non_function_local() {
        let source = concat!("fn main -> i64\n", "    let x i64 = 3\n", "    x(1)\n",);
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0390")
        );
    }

    #[test]
    fn rejects_passing_a_wrong_signature_function() {
        let source = concat!(
            "fn two x i64 y i64 -> i64\n",
            "    x + y\n\n",
            "fn apply f fn(i64) -> i64 v i64 -> i64\n",
            "    f(v)\n\n",
            "fn main -> i64\n",
            "    apply(two, 10)\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0313")
        );
    }

    #[test]
    fn validates_string_builtins() {
        let source = concat!(
            "fn main -> i64\n",
            "    let parts array<string> = split(\"a,b\", \",\")\n",
            "    let joined string = join(parts, \"-\")\n",
            "    let head string = substring(joined, 0, 1)\n",
            "    let ok bool = contains(head, \"a\")\n",
            "    let cleaned string = trim(upper(lower(replace(joined, \"-\", \"+\"))))\n",
            "    find(cleaned, head)\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_string_builtin_wrong_type() {
        let diagnostics = validate_source("fn main -> i64\n    substring(42, 0, 1)\n    0\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0375")
        );
    }

    #[test]
    fn os_random_type_checks_and_yields_result_of_list_byte() {
        // `os_random(len i64) -> result<list<byte>, string>`: an `i64` argument
        // type-checks, and the `ok` payload is a `list<byte>` (so `len` on it is
        // valid and the whole program is well-typed).
        let source = concat!(
            "fn count n i64 -> i64\n",
            "    match os_random(n)\n",
            "        ok(bytes) -> len(bytes)\n",
            "        err(_) -> 0\n\n",
            "fn main -> i64\n",
            "    count(16)\n",
        );
        assert!(
            validate_source(source).is_ok(),
            "os_random should type-check with an i64 argument and a list<byte> ok payload"
        );
    }

    #[test]
    fn rejects_os_random_wrong_argument_type() {
        // A `string` where `os_random` expects an `i64` is an argument-type
        // error (`L0313`), never accepted.
        let diagnostics =
            validate_source("fn main -> i64\n    match os_random(\"16\")\n        ok(b) -> len(b)\n        err(_) -> 0\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0313"),
            "expected L0313 for a non-i64 os_random argument: {diagnostics:?}"
        );
    }

    #[test]
    fn rejects_repeat_wrong_count_type() {
        let diagnostics = validate_source("fn main -> i64\n    repeat(\"ab\", \"x\")\n    0\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0375")
        );
    }

    #[test]
    fn rejects_join_non_array_argument() {
        let diagnostics = validate_source("fn main -> i64\n    join(\"a\", \"-\")\n    0\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0375")
        );
    }

    #[test]
    fn accepts_bit_intrinsics() {
        let source = "fn main -> i64\n    let a i64 = rotate_left(1, 4)\n    let b i64 = rotate_right(a, 4)\n    let c i64 = count_ones(255)\n    let d i64 = leading_zeros(1)\n    let e i64 = trailing_zeros(16)\n    let f i64 = reverse_bytes(b)\n    a + b + c + d + e + f\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_rotate_left_with_non_i64_argument() {
        let diagnostics =
            validate_source("fn main -> i64\n    let x i64 = rotate_left(1, 2.0)\n    x\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0374")
        );
    }

    #[test]
    fn rejects_count_ones_with_non_i64_argument() {
        let diagnostics =
            validate_source("fn main -> i64\n    let x i64 = count_ones(1.0)\n    x\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0374")
        );
    }

    #[test]
    fn catches_assignment_type_mismatch() {
        let diagnostics = validate_source(
            "fn bad -> bool\n    let value bool = false\n    value = 1\n    value\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0314")
        );
    }

    #[test]
    fn catches_assignment_type_mismatch_after_inference() {
        let diagnostics =
            validate_source("fn bad -> i64\n    let value = 1\n    value = false\n    value\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0314")
        );
    }

    #[test]
    fn catches_undeclared_assignment() {
        let diagnostics =
            validate_source("fn bad -> i64\n    value = 1\n    value\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0316")
        );
    }

    #[test]
    fn catches_break_outside_loop() {
        let diagnostics = validate_source("fn bad -> void\n    break\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0317")
        );
    }

    #[test]
    fn catches_invalid_logical_operand() {
        let diagnostics =
            validate_source("fn bad -> bool\n    1 and true\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0320")
        );
    }

    #[test]
    fn catches_invalid_for_range_type() {
        let diagnostics =
            validate_source("fn bad -> i64\n    for i from false to 3\n        i\n    0\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0321")
        );
    }

    #[test]
    fn catches_invalid_for_step_type() {
        let diagnostics =
            validate_source("fn bad -> i64\n    for i from 1 to 3 by false\n        i\n    0\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0322")
        );
    }

    #[test]
    fn catches_array_literal_type_mismatch() {
        let diagnostics =
            validate_source("fn bad -> array<i64>\n    [1, false]\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0324")
        );
    }

    #[test]
    fn catches_array_index_type_mismatch() {
        let diagnostics = validate_source(
            "fn bad -> i64\n    let values array<i64> = [1, 2]\n    values[true]\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0326")
        );
    }

    #[test]
    fn catches_ordering_type_mismatch() {
        let diagnostics =
            validate_source("fn bad -> bool\n    false < true\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0327")
        );
    }

    #[test]
    fn catches_store_value_type_mismatch() {
        let diagnostics = validate_source(
            "fn bad -> void\n    let ptr ptr_i64 = alloc(1)\n    store(ptr, false)\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0328")
        );
    }

    #[test]
    fn validates_io_and_system_builtins() {
        let source = "fn main -> bool\n    write_file(\"target/lullaby_semantics_io.txt\", \"alpha\")\n    append_file(\"target/lullaby_semantics_io.txt\", \" beta\")\n    let content string = read_file(\"target/lullaby_semantics_io.txt\")\n    let exists bool = file_exists(\"target/lullaby_semantics_io.txt\")\n    let status i64 = sys_status(\"rustc\", [\"--version\"])\n    content == \"alpha beta\" and exists and status == 0\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn resolves_type_aliases_structurally() {
        // `Count` is an alias for `i64`, so alias and target are interchangeable.
        let source = "alias Count = i64\n\nfn main -> Count\n    let a Count = 41\n    let b i64 = a\n    b + 1\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn resolves_alias_inside_generic_argument() {
        let source = "alias Count = i64\n\nfn main -> i64\n    let values array<Count> = [1, 2]\n    values[0]\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_duplicate_type_alias() {
        let diagnostics =
            validate_source("alias A = i64\nalias A = bool\n\nfn main -> i64\n    0\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0360")
        );
    }

    #[test]
    fn rejects_cyclic_type_alias() {
        let diagnostics = validate_source("alias A = B\nalias B = A\n\nfn main -> i64\n    0\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0361")
        );
    }

    #[test]
    fn detects_use_after_free_at_compile_time() {
        let diagnostics = validate_source(
            "fn main -> i64\n    let p ptr_i64 = alloc(1)\n    dealloc(p)\n    unsafe\n        ptr_read(p)\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0350")
        );
    }

    #[test]
    fn detects_double_free_at_compile_time() {
        let diagnostics = validate_source(
            "fn main -> void\n    let p ptr_i64 = alloc(1)\n    dealloc(p)\n    dealloc(p)\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0350")
        );
    }

    #[test]
    fn allows_use_before_free() {
        let source = "fn main -> i64\n    let p ptr_i64 = alloc(1)\n    let v i64 = 0\n    unsafe\n        v = ptr_read(p)\n    dealloc(p)\n    v\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_returning_borrowed_reference() {
        let diagnostics = validate_source("fn leak h rc<i64> -> ref<i64>\n    rc_borrow(h)\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0351")
        );
    }

    #[test]
    fn validates_try_catch_and_throw() {
        let source = "fn main -> void\n    try\n        throw \"oops\"\n    catch message\n        warn(message)\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn try_catch_is_a_value_expression() {
        // Both arms yield a string, so the try/catch can be the function's final value.
        let source = "fn main -> string\n    try\n        throw \"x\"\n    catch message\n        \"caught: \" + message\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_throwing_non_string() {
        let diagnostics = validate_source("fn main -> void\n    throw 42\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0313")
        );
    }

    #[test]
    fn validates_region_declarations() {
        let source = "fn main -> i64\n    region pool: size=4096, align=16, kind=static\n    region scratch: size=1024, kind=dynamic, mutable=true\n    0\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_region_with_bad_size() {
        let diagnostics = validate_source("fn main -> i64\n    region pool: size=0\n    0\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0340")
        );
    }

    #[test]
    fn rejects_region_with_non_power_of_two_alignment() {
        let diagnostics =
            validate_source("fn main -> i64\n    region pool: size=1024, align=15\n    0\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0340")
        );
    }

    #[test]
    fn rejects_duplicate_region() {
        let diagnostics = validate_source(
            "fn main -> i64\n    region pool: size=16\n    region pool: size=32\n    0\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0341")
        );
    }

    #[test]
    fn validates_reference_builtins() {
        let source = "fn main -> i64\n    let h rc<i64> = rc_new(1)\n    let s rc<i64> = rc_clone(h)\n    let v ref<i64> = rc_borrow(h)\n    let a i64 = ref_get(v)\n    rc_release(s)\n    rc_release(h)\n    a\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn requires_unsafe_for_raw_pointer_read() {
        let diagnostics =
            validate_source("fn main -> i64\n    let p ptr_i64 = alloc(1)\n    ptr_read(p)\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0330")
        );
    }

    #[test]
    fn validates_asm_inside_unsafe() {
        // A well-formed `asm` inside `unsafe` with in-range bytes type-checks; a
        // trailing `asm` satisfies the `i64` final-value requirement.
        let source = "fn main -> i64\n    unsafe\n        asm 72, 199, 192, 42, 0, 0, 0\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_asm_outside_unsafe() {
        let diagnostics =
            validate_source("fn main -> i64\n    asm 72, 199, 192, 42, 0, 0, 0\n    0\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0330"),
            "asm outside unsafe must be L0330"
        );
    }

    #[test]
    fn rejects_asm_byte_out_of_range() {
        let diagnostics = validate_source("fn main -> i64\n    unsafe\n        asm 256\n    0\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0425"),
            "out-of-range asm byte must be L0425"
        );
    }

    #[test]
    fn allows_raw_pointer_read_inside_unsafe() {
        let source = "fn main -> i64\n    let p ptr_i64 = alloc(1)\n    let v i64 = 0\n    unsafe\n        v = ptr_read(p)\n    dealloc(p)\n    v\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_reference_builtin_type_mismatch() {
        let diagnostics =
            validate_source("fn main -> i64\n    let x i64 = 1\n    rc_get(x)\n    x\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0331")
        );
    }

    #[test]
    fn validates_struct_field_mutation() {
        let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(1, 2)\n    p.x = 9\n    p.y += 1\n    p.x + p.y\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_field_mutation_type_mismatch() {
        let diagnostics = validate_source(
            "struct Point\n    x i64\n\nfn main -> i64\n    let p Point = Point(1)\n    p.x = true\n    p.x\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0314")
        );
    }

    #[test]
    fn rejects_mutation_of_unknown_field() {
        let diagnostics = validate_source(
            "struct Point\n    x i64\n\nfn main -> i64\n    let p Point = Point(1)\n    p.z = 5\n    p.x\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0371")
        );
    }

    #[test]
    fn validates_array_element_mutation_and_len() {
        let source = "fn main -> i64\n    let xs array<i64> = [1, 2, 3]\n    xs[0] = 10\n    xs[1] += 5\n    xs[len(xs) - 1]\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_array_element_type_mismatch() {
        let diagnostics = validate_source(
            "fn main -> i64\n    let xs array<i64> = [1]\n    xs[0] = true\n    xs[0]\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0314")
        );
    }

    #[test]
    fn rejects_index_assignment_on_non_array() {
        let diagnostics =
            validate_source("fn main -> i64\n    let n i64 = 1\n    n[0] = 2\n    n\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0325")
        );
    }

    #[test]
    fn rejects_len_on_non_collection() {
        let diagnostics = validate_source("fn main -> i64\n    len(5)\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0373")
        );
    }

    #[test]
    fn validates_math_builtins() {
        let source = "fn main -> i64\n    let a i64 = abs(0 - 5)\n    let b i64 = min(a, max(2, 9))\n    let c i64 = pow(2, 3)\n    let d f64 = sqrt(floor(ceil(round(2.5))))\n    if d > 0.0\n        b + c\n    else\n        0\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_math_builtin_on_wrong_type() {
        let diagnostics = validate_source("fn main -> i64\n    sqrt(4)\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0374")
        );
    }

    #[test]
    fn accepts_list_search_and_rejects_element_mismatch() {
        let ok = concat!(
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, 3)\n",
            "    list_index_of(l, 3)\n",
        );
        assert!(validate_source(ok).is_ok(), "{:?}", validate_source(ok));

        let bad = concat!(
            "fn main -> bool\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, 3)\n",
            "    list_contains(l, true)\n",
        );
        let diagnostics = validate_source(bad).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0387"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn accepts_chars_round_trip_and_rejects_wrong_types() {
        let ok = concat!(
            "fn main -> i64\n",
            "    let cs list<char> = chars(\"hi\")\n",
            "    len(string_from_chars(cs))\n",
        );
        assert!(validate_source(ok).is_ok(), "{:?}", validate_source(ok));

        // `chars` needs a string; `string_from_chars` needs a list<char>.
        let bad1 = validate_source("fn main -> i64\n    len(chars(5))\n").expect_err("semantic");
        assert!(bad1.iter().any(|d| d.code == "L0375"), "{bad1:?}");
        let bad2 = validate_source("fn main -> i64\n    len(string_from_chars(\"x\"))\n")
            .expect_err("semantic");
        assert!(bad2.iter().any(|d| d.code == "L0375"), "{bad2:?}");
    }

    #[test]
    fn validates_trig_exp_and_log_builtins() {
        let source = concat!(
            "fn main -> i64\n",
            "    let a f64 = sin(cos(tan(atan(0.0))))\n",
            "    let b f64 = atan2(1.0, 2.0)\n",
            "    let c f64 = exp(ln(log10(1000.0)))\n",
            "    if a + b + c > 0.0\n",
            "        1\n",
            "    else\n",
            "        0\n",
        );
        assert!(
            validate_source(source).is_ok(),
            "{:?}",
            validate_source(source)
        );
    }

    #[test]
    fn rejects_trig_builtin_on_wrong_type() {
        let diagnostics = validate_source("fn main -> i64\n    let x f64 = sin(1)\n    0\n")
            .expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0374"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_atan2_with_non_f64_argument() {
        let diagnostics = validate_source("fn main -> i64\n    let x f64 = atan2(1.0, 2)\n    0\n")
            .expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0374"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_min_with_mismatched_operands() {
        let diagnostics =
            validate_source("fn main -> i64\n    min(1, 2.0)\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0374")
        );
    }

    #[test]
    fn validates_struct_construction_and_field_access() {
        let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(3, 4)\n    p.x + p.y\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_unknown_struct_field() {
        let diagnostics = validate_source(
            "struct Point\n    x i64\n\nfn main -> i64\n    let p Point = Point(1)\n    p.z\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0371")
        );
    }

    #[test]
    fn rejects_wrong_struct_construction() {
        let diagnostics = validate_source(
            "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(1)\n    p.x\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0372")
        );
    }

    #[test]
    fn rejects_struct_field_type_mismatch() {
        let diagnostics = validate_source(
            "struct Point\n    x i64\n\nfn main -> i64\n    let p Point = Point(true)\n    p.x\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0372")
        );
    }

    #[test]
    fn validates_f64_arithmetic() {
        let source = "fn main -> f64\n    let x f64 = 1.5\n    x * 2.0 - 0.5\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_mixing_i64_and_f64() {
        let diagnostics = validate_source("fn main -> f64\n    let x f64 = 1.5\n    x + 2\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0307")
        );
    }

    #[test]
    fn validates_named_field_construction_in_any_order() {
        let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(y: 4, x: 3)\n    p.x\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_named_construction_missing_field() {
        let diagnostics = validate_source(
            "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(x: 3)\n    p.x\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0372")
        );
    }

    #[test]
    fn rejects_named_construction_unknown_field() {
        let diagnostics = validate_source(
            "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(x: 3, y: 4, z: 5)\n    p.x\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0372")
        );
    }

    #[test]
    fn validates_string_concatenation_and_to_string() {
        let source =
            "fn main -> string\n    \"n=\" + to_string(1 + 2) + \" b=\" + to_string(true)\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_char_and_byte_builtins_and_comparisons() {
        let source = concat!(
            "fn main -> i64\n",
            "    let a char = 'a'\n",
            "    let b char = char_from(char_code(a) + 1)\n",
            "    let ordered i64 = 0\n",
            "    if a < b\n",
            "        ordered = 1\n",
            "    let small byte = byte(10)\n",
            "    let big byte = byte(250)\n",
            "    let s string = to_string(a) + to_string(small)\n",
            "    char_code(b) + byte_val(big) + ordered + len(s)\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_char_builtin_with_wrong_argument_type() {
        let diagnostics =
            validate_source("fn main -> i64\n    char_code(65)\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0389")
        );
    }

    #[test]
    fn validates_char_classification_predicates() {
        // The six `char -> bool` classification predicates each take one `char`
        // and yield a `bool` usable in a condition.
        let source = concat!(
            "fn main -> i64\n",
            "    let c char = '7'\n",
            "    let flags i64 = 0\n",
            "    if is_digit(c)\n",
            "        flags = flags + 1\n",
            "    if is_alpha(c)\n",
            "        flags = flags + 1\n",
            "    if is_alnum(c)\n",
            "        flags = flags + 1\n",
            "    if is_whitespace(c)\n",
            "        flags = flags + 1\n",
            "    if is_upper(c)\n",
            "        flags = flags + 1\n",
            "    if is_lower(c)\n",
            "        flags = flags + 1\n",
            "    flags\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_char_classification_predicate_with_wrong_argument_type() {
        // `is_digit` requires a `char`; passing an `i64` is an `L0389` argument
        // type error.
        let diagnostics =
            validate_source("fn main -> bool\n    is_digit(7)\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0389")
        );
    }

    #[test]
    fn validates_env_and_args_process_builtins() {
        // `env(name)` yields `option<string>`; `args()` yields `list<string>`.
        let source = concat!(
            "fn env_flag name string -> i64\n",
            "    match env(name)\n",
            "        some(_) -> 1\n",
            "        none -> 0\n\n",
            "fn main -> i64\n",
            "    env_flag(\"HOME\") + len(args())\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_env_with_wrong_argument_type() {
        let diagnostics = validate_source(concat!(
            "fn main -> i64\n",
            "    match env(5)\n",
            "        some(_) -> 1\n",
            "        none -> 0\n",
        ))
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0332")
        );
    }

    #[test]
    fn rejects_args_with_wrong_arity() {
        let diagnostics =
            validate_source("fn main -> i64\n    len(args(1))\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0332")
        );
    }

    #[test]
    fn rejects_mixed_string_and_int_addition() {
        let diagnostics =
            validate_source("fn main -> string\n    \"n=\" + 5\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0307")
        );
    }

    #[test]
    fn validates_standard_stream_builtins() {
        let source = "fn main -> void\n    println(\"hello\")\n    print(\"partial\")\n    warn(\"careful\")\n    flush()\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn catches_stream_builtin_argument_type_mismatch() {
        let diagnostics =
            validate_source("fn bad -> void\n    println(1)\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0313")
        );
    }

    #[test]
    fn catches_stream_builtin_arity_mismatch() {
        let diagnostics =
            validate_source("fn bad -> void\n    flush(\"x\")\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0312")
        );
    }

    #[test]
    fn validates_time_builtins() {
        let source = concat!(
            "fn main -> void\n",
            "    let a i64 = mono_now()\n",
            "    let b i64 = wall_now()\n",
            "    sleep_millis(0)\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn catches_sleep_millis_argument_type_mismatch() {
        let diagnostics =
            validate_source("fn bad -> void\n    sleep_millis(\"x\")\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0313")
        );
    }

    #[test]
    fn catches_file_builtin_argument_type_mismatch() {
        let diagnostics =
            validate_source("fn bad -> string\n    read_file(1)\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0313")
        );
    }

    #[test]
    fn catches_fs_builtin_argument_type_mismatch() {
        // A non-`string` path to a file-system builtin reports the dedicated
        // `L0333` code.
        let diagnostics =
            validate_source("fn bad -> i64\n    file_size(1)\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0333")
        );
    }

    #[test]
    fn catches_fs_builtin_arity_mismatch() {
        // Wrong arity for a file-system builtin also reports `L0333`.
        let diagnostics =
            validate_source("fn bad -> void\n    make_dir()\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0333")
        );
    }

    #[test]
    fn accepts_parallel_map_with_matching_types() {
        let source = "fn sq x i64 -> i64\n    x * x\n\nfn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    parallel_map(sq, base)\n";
        validate_source(source).expect("semantic");
    }

    #[test]
    fn rejects_parallel_map_non_function_first_argument() {
        // The first argument must be a `fn(i64) -> i64`, not a plain list.
        let source = "fn main -> list<i64>\n    let base list<i64> = list_new()\n    parallel_map(base, base)\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0334")
        );
    }

    #[test]
    fn rejects_parallel_map_non_list_second_argument() {
        // The second argument must be a `list<i64>`, not an `i64`.
        let source =
            "fn sq x i64 -> i64\n    x * x\n\nfn main -> list<i64>\n    parallel_map(sq, 5)\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0334")
        );
    }

    #[test]
    fn accepts_concurrency_builtins_with_matching_types() {
        let source = "fn worker ch Chan v i64 -> void\n    send(ch, v * v)\n\nfn main -> i64\n    let ch Chan = chan_new()\n    let t Task = spawn(worker, ch, 3)\n    task_join(t)\n    let m Mutex = mutex_new(0)\n    mutex_set(m, 5)\n    mutex_add(m, 2)\n    recv(ch) + mutex_get(m)\n";
        validate_source(source).expect("semantic");
    }

    #[test]
    fn rejects_spawn_non_function_first_argument() {
        // `spawn`'s first argument must be a `fn(Chan, i64) -> void`.
        let source = "fn main -> i64\n    let ch Chan = chan_new()\n    let t Task = spawn(ch, ch, 3)\n    task_join(t)\n    0\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0337")
        );
    }

    #[test]
    fn rejects_send_non_chan_first_argument() {
        // `send` requires a `Chan` handle as its first argument.
        let source = "fn main -> void\n    send(5, 5)\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0337")
        );
    }

    #[test]
    fn rejects_mutex_add_non_mutex_first_argument() {
        // `mutex_add` requires a `Mutex` handle as its first argument.
        let source = "fn main -> i64\n    mutex_add(5, 1)\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0337")
        );
    }

    #[test]
    fn accepts_atomic_builtins_with_matching_types() {
        // The full `atomic_i64` surface type-checks: construct, load/store,
        // swap, strong CAS, and every fetch-and-op. Each op takes the
        // `atomic_i64` handle first and returns the documented type.
        let source = concat!(
            "fn main -> i64\n",
            "    let a atomic_i64 = atomic_new(10)\n",
            "    let p i64 = atomic_add(a, 5)\n",
            "    atomic_sub(a, 1)\n",
            "    atomic_and(a, 15)\n",
            "    atomic_or(a, 1)\n",
            "    atomic_xor(a, 2)\n",
            "    atomic_store(a, 42)\n",
            "    let s i64 = atomic_swap(a, 7)\n",
            "    let c i64 = atomic_cas(a, 7, 99)\n",
            "    p + s + c + atomic_load(a)\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_atomic_load_non_atomic_first_argument() {
        // `atomic_load` requires an `atomic_i64` handle as its first argument;
        // a bare `i64` is rejected with the concurrency-builtin code L0337.
        let source = "fn main -> i64\n    atomic_load(5)\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0337"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_atomic_add_wrong_operand_type() {
        // The `v` operand of a fetch-and-op must be an `i64`, not a string.
        let source =
            "fn main -> i64\n    let a atomic_i64 = atomic_new(0)\n    atomic_add(a, \"x\")\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0337"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn catches_write_bytes_data_type_mismatch() {
        // `write_bytes` requires a `list<byte>` data argument.
        let diagnostics =
            validate_source("fn bad -> void\n    write_bytes(\"p\", \"not bytes\")\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0333")
        );
    }

    #[test]
    fn string_bytes_builtins_type_check() {
        // `to_bytes` -> `list<byte>`, `from_bytes` -> `result<string, string>`
        // (unwrapped with `match`), and `byte_len` -> `i64`.
        validate_source(concat!(
            "fn f -> i64\n",
            "    let b list<byte> = to_bytes(\"hi\")\n",
            "    let n i64 = byte_len(\"café\")\n",
            "    match from_bytes(b)\n",
            "        ok(s) -> len(s) + n + byte_val(get(b, 0))\n",
            "        err(m) -> 0 - len(m)\n",
        ))
        .expect("string↔bytes builtins type-check");
    }

    #[test]
    fn catches_to_bytes_argument_type_mismatch() {
        // `to_bytes` requires a `string` argument; a wrong type reports the
        // string-builtin family code `L0375`.
        let diagnostics =
            validate_source("fn bad -> i64\n    len(to_bytes(7))\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0375")
        );
    }

    #[test]
    fn catches_from_bytes_argument_type_mismatch() {
        // `from_bytes` requires a `list<byte>` argument; a `string` is rejected
        // with the string-builtin family code `L0375`.
        let diagnostics = validate_source(concat!(
            "fn bad -> i64\n",
            "    match from_bytes(\"not bytes\")\n",
            "        ok(s) -> len(s)\n",
            "        err(m) -> 0 - len(m)\n",
        ))
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0375")
        );
    }

    #[test]
    fn parse_number_builtins_type_check() {
        // `parse_i64` -> `result<i64, string>` and `parse_f64` ->
        // `result<f64, string>`, unwrapped with `match` in a helper's tail.
        validate_source(concat!(
            "fn to_int s string -> i64\n",
            "    match parse_i64(s)\n",
            "        ok(n) -> n\n",
            "        err(m) -> 0 - len(m)\n",
            "fn to_float s string -> f64\n",
            "    match parse_f64(s)\n",
            "        ok(x) -> x\n",
            "        err(m) -> 0.0\n",
            "fn f -> f64\n",
            "    to_float(\"3.5\") + sqrt(to_float(\"4.0\"))\n",
            "fn g -> i64\n",
            "    to_int(\"42\")\n",
        ))
        .expect("parse_i64/parse_f64 builtins type-check");
    }

    #[test]
    fn catches_parse_i64_argument_type_mismatch() {
        // `parse_i64` requires a `string` argument; an `i64` is rejected with
        // the string-builtin family code `L0375`.
        let diagnostics = validate_source(concat!(
            "fn bad -> i64\n",
            "    match parse_i64(7)\n",
            "        ok(n) -> n\n",
            "        err(m) -> 0 - len(m)\n",
        ))
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0375")
        );
    }

    #[test]
    fn catches_parse_f64_argument_type_mismatch() {
        // `parse_f64` requires a `string` argument; an `i64` is rejected with
        // the string-builtin family code `L0375`.
        let diagnostics = validate_source(concat!(
            "fn bad -> f64\n",
            "    match parse_f64(7)\n",
            "        ok(x) -> x\n",
            "        err(m) -> 0.0\n",
        ))
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0375")
        );
    }

    #[test]
    fn catches_system_builtin_argument_type_mismatch() {
        let diagnostics = validate_source("fn bad -> i64\n    sys_status(\"rustc\", [1])\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0313")
        );
    }

    #[test]
    fn executable_validation_requires_main_entrypoint() {
        let tokens = lex("fn add x i64 y i64 -> i64\n    x + y\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        let diagnostics = validate_executable(&program).expect_err("entrypoint");

        assert_eq!(diagnostics[0].code, "L0329");
    }

    #[test]
    fn executable_validation_rejects_main_parameters() {
        let tokens = lex("fn main arg i64 -> i64\n    arg\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        let diagnostics = validate_executable(&program).expect_err("entrypoint");

        assert_eq!(diagnostics[0].code, "L0329");
        assert_eq!(diagnostics[0].function.as_deref(), Some("main"));
    }

    #[test]
    fn validates_enum_declaration_and_construction() {
        let source = "enum Color\n    Red\n    Green\n    Blue\n\nenum Shape\n    Circle f64\n    Rect f64 f64\n    Empty\n\nfn main -> i64\n    let c Color = Green\n    let s Shape = Circle(2.0)\n    let r Shape = Rect(3.0, 4.0)\n    let e Shape = Empty\n    0\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn enum_construction_returns_owning_enum_type() {
        let source = "enum Shape\n    Circle f64\n    Empty\n\nfn area s Shape -> i64\n    0\n\nfn main -> i64\n    area(Circle(1.0)) + area(Empty)\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_duplicate_variant_within_enum() {
        let source = "enum Color\n    Red\n    Red\n\nfn main -> i64\n    0\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0380")
        );
    }

    #[test]
    fn rejects_enum_construction_arity_mismatch() {
        let source = "enum Shape\n    Circle f64\n    Empty\n\nfn main -> i64\n    let s Shape = Circle(1.0, 2.0)\n    0\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0381")
        );
    }

    #[test]
    fn rejects_enum_construction_payload_type_mismatch() {
        let source = "enum Shape\n    Circle f64\n    Empty\n\nfn main -> i64\n    let s Shape = Circle(1)\n    0\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0381")
        );
    }

    #[test]
    fn rejects_cross_enum_variant_collision() {
        let source = "enum A\n    Shared\n\nenum B\n    Shared\n\nfn main -> i64\n    0\n";
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "L0382")
        );
    }

    #[test]
    fn validates_exhaustive_match_with_bindings() {
        let source = concat!(
            "enum Shape\n    Circle i64\n    Rect i64 i64\n    Empty\n\n",
            "fn area s Shape -> i64\n",
            "    match s\n",
            "        Circle(r) -> r * r\n",
            "        Rect(w, h) -> w * h\n",
            "        Empty -> 0\n\n",
            "fn main -> i64\n    area(Circle(3))\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_match_with_wildcard_arm() {
        let source = concat!(
            "enum Color\n    Red\n    Green\n    Blue\n\n",
            "fn rank c Color -> i64\n",
            "    match c\n",
            "        Green -> 10\n",
            "        _ -> 1\n\n",
            "fn main -> i64\n    rank(Blue)\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_match_on_non_enum_scrutinee() {
        let source = concat!(
            "fn main -> i64\n",
            "    let x i64 = 1\n",
            "    match x\n",
            "        _ -> 0\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0383"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_non_exhaustive_match() {
        let source = concat!(
            "enum Shape\n    Circle i64\n    Rect i64 i64\n    Empty\n\n",
            "fn area s Shape -> i64\n",
            "    match s\n",
            "        Circle(r) -> r\n",
            "        Empty -> 0\n\n",
            "fn main -> i64\n    area(Empty)\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0384"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_match_arm_with_wrong_binding_arity() {
        let source = concat!(
            "enum Shape\n    Circle i64\n    Empty\n\n",
            "fn area s Shape -> i64\n",
            "    match s\n",
            "        Circle(a, b) -> a\n",
            "        Empty -> 0\n\n",
            "fn main -> i64\n    area(Empty)\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0385"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_match_arm_with_unknown_variant() {
        let source = concat!(
            "enum Shape\n    Circle i64\n    Empty\n\n",
            "fn area s Shape -> i64\n",
            "    match s\n",
            "        Circle(r) -> r\n",
            "        Square -> 0\n",
            "        Empty -> 0\n\n",
            "fn main -> i64\n    area(Empty)\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0385"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn accepts_annotated_option_and_result_construction() {
        let source = concat!(
            "fn main -> i64\n",
            "    let a option<i64> = some(3)\n",
            "    let b option<i64> = none\n",
            "    let r result<i64, string> = ok(3)\n",
            "    let e result<i64, string> = err(\"x\")\n",
            "    0\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn accepts_option_in_return_and_final_expression() {
        let source = concat!(
            "fn pick flag bool -> option<i64>\n",
            "    if flag\n",
            "        return none\n",
            "    some(1)\n\n",
            "fn main -> i64\n",
            "    match pick(true)\n",
            "        some(v) -> v\n",
            "        none -> 0\n",
        );
        assert!(
            validate_source(source).is_ok(),
            "{:?}",
            validate_source(source)
        );
    }

    #[test]
    fn accepts_match_over_option_and_result() {
        let source = concat!(
            "fn unwrap_or o option<i64> fallback i64 -> i64\n",
            "    match o\n",
            "        some(v) -> v\n",
            "        none -> fallback\n\n",
            "fn describe r result<i64, string> -> i64\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> len(m)\n\n",
            "fn main -> i64\n",
            "    let o option<i64> = some(2)\n",
            "    let r result<i64, string> = ok(5)\n",
            "    unwrap_or(o, 0) + describe(r)\n",
        );
        assert!(
            validate_source(source).is_ok(),
            "{:?}",
            validate_source(source)
        );
    }

    #[test]
    fn rejects_none_without_expected_type() {
        let source = concat!("fn main -> i64\n", "    let x = none\n", "    0\n");
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0386"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_ok_without_expected_type() {
        let source = concat!("fn main -> i64\n", "    let x = ok(3)\n", "    0\n");
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0386"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_option_payload_type_mismatch() {
        let source = concat!(
            "fn main -> i64\n",
            "    let a option<i64> = some(true)\n",
            "    0\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0303"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn accepts_list_builtins() {
        let source = concat!(
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, 10)\n",
            "    l = push(l, 20)\n",
            "    l = set(l, 0, 5)\n",
            "    let head i64 = get(l, 0)\n",
            "    let n i64 = len(l)\n",
            "    l = pop(l)\n",
            "    head + n + len(l)\n",
        );
        assert!(
            validate_source(source).is_ok(),
            "{:?}",
            validate_source(source)
        );
    }

    #[test]
    fn rejects_list_new_without_expected_type() {
        let source = concat!("fn main -> i64\n", "    let l = list_new()\n", "    0\n");
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0387"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_push_element_type_mismatch() {
        let source = concat!(
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, true)\n",
            "    0\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0387"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn accepts_list_ext_builtins() {
        let source = concat!(
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, 10)\n",
            "    l = push(l, 20)\n",
            "    let r list<i64> = reverse(l)\n",
            "    let both list<i64> = concat(l, r)\n",
            "    let mid list<i64> = slice(both, 1, 3)\n",
            "    len(both) + len(mid)\n",
        );
        assert!(
            validate_source(source).is_ok(),
            "{:?}",
            validate_source(source)
        );
    }

    #[test]
    fn rejects_concat_element_type_mismatch() {
        let source = concat!(
            "fn main -> i64\n",
            "    let a list<i64> = list_new()\n",
            "    a = push(a, 1)\n",
            "    let b list<bool> = list_new()\n",
            "    b = push(b, true)\n",
            "    let c list<i64> = concat(a, b)\n",
            "    len(c)\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0387"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn accepts_sort_on_i64_list_and_rejects_other_element_types() {
        let ok = concat!(
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, 3)\n",
            "    len(sort(l))\n",
        );
        assert!(validate_source(ok).is_ok(), "{:?}", validate_source(ok));

        let bad = concat!(
            "fn main -> i64\n",
            "    let l list<bool> = list_new()\n",
            "    l = push(l, true)\n",
            "    len(sort(l))\n",
        );
        let diagnostics = validate_source(bad).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0387"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn infers_nested_constructor_in_call_argument_position() {
        // Argument-position inference: a nested `list_new()`/`map_new()` inside a
        // collection-growing builtin, and a nested `ok`/`none`/`some` inside a
        // user call, take their type from the surrounding context.
        let source = concat!(
            "fn describe r result<i64, string> -> i64\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> 0 - len(m)\n\n",
            "fn count o option<i64> -> i64\n",
            "    match o\n",
            "        some(v) -> v\n",
            "        none -> 0\n\n",
            "fn main -> i64\n",
            "    let data list<byte> = push(list_new(), byte(65))\n",
            "    let m map<string, i64> = map_set(map_new(), \"x\", 7)\n",
            "    let a i64 = describe(ok(5))\n",
            "    let b i64 = count(none)\n",
            "    byte_val(get(data, 0)) + map_len(m) + a + b\n",
        );
        assert!(
            validate_source(source).is_ok(),
            "{:?}",
            validate_source(source)
        );
    }

    #[test]
    fn accepts_map_builtins() {
        let source = concat!(
            "fn lookup m map<string, i64> k string -> i64\n",
            "    match map_get(m, k)\n",
            "        some(v) -> v\n",
            "        none -> 0\n\n",
            "fn main -> i64\n",
            "    let m map<string, i64> = map_new()\n",
            "    m = map_set(m, \"a\", 1)\n",
            "    let has i64 = 0\n",
            "    if map_has(m, \"a\")\n",
            "        has = 1\n",
            "    let n i64 = map_len(m)\n",
            "    m = map_del(m, \"a\")\n",
            "    has + n + lookup(m, \"a\") + map_len(m)\n",
        );
        assert!(
            validate_source(source).is_ok(),
            "{:?}",
            validate_source(source)
        );
    }

    #[test]
    fn accepts_map_keys_and_values() {
        let source = concat!(
            "fn main -> i64\n",
            "    let m map<string, i64> = map_new()\n",
            "    m = map_set(m, \"a\", 5)\n",
            "    let ks list<string> = map_keys(m)\n",
            "    let vs list<i64> = map_values(m)\n",
            "    len(ks) + get(vs, 0)\n",
        );
        assert!(
            validate_source(source).is_ok(),
            "{:?}",
            validate_source(source)
        );
    }

    #[test]
    fn rejects_map_keys_on_non_map() {
        let diagnostics =
            validate_source("fn main -> i64\n    len(map_keys(3))\n").expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0388"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_map_new_without_expected_type() {
        let source = concat!("fn main -> i64\n", "    let m = map_new()\n", "    0\n");
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0388"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_map_with_unsupported_key_type() {
        let source = concat!(
            "fn main -> i64\n",
            "    let m map<bool, i64> = map_new()\n",
            "    0\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0388"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_result_payload_type_mismatch() {
        let source = concat!(
            "fn main -> i64\n",
            "    let r result<i64, string> = ok(\"x\")\n",
            "    0\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0303"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_non_exhaustive_option_match() {
        let source = concat!(
            "fn get o option<i64> -> i64\n",
            "    match o\n",
            "        some(v) -> v\n\n",
            "fn main -> i64\n",
            "    get(some(1))\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0384"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_user_variant_named_none() {
        let source = concat!(
            "enum Maybe\n    just i64\n    none\n\n",
            "fn main -> i64\n    0\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0382"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn validates_generic_functions_at_several_types() {
        let source = concat!(
            "fn identity<T> x T -> T\n",
            "    x\n\n",
            "fn wrap<T> x T -> option<T>\n",
            "    some(x)\n\n",
            "fn choose<T> pick bool a T b T -> T\n",
            "    if pick\n",
            "        return a\n",
            "    b\n\n",
            "fn main -> i64\n",
            "    let n i64 = identity(41)\n",
            "    let s string = identity(\"hi\")\n",
            "    let picked i64 = choose(true, 10, 20)\n",
            "    let maybe option<i64> = wrap(1)\n",
            "    n + picked + len(s)\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn generic_identity_return_type_is_the_argument_type() {
        // A `let string = identity("hi")` binding proves the call inferred and
        // substituted `T = string` into the return type; a mismatch would be an
        // `L0301`/binding error.
        let source = concat!(
            "fn identity<T> x T -> T\n",
            "    x\n\n",
            "fn main -> string\n",
            "    let s string = identity(\"hi\")\n",
            "    s\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_conflicting_generic_inference() {
        // `same(1, "x")` binds `T` to both `i64` and `string`: `L0395`.
        let source = concat!(
            "fn same<T> a T b T -> T\n",
            "    a\n\n",
            "fn main -> i64\n",
            "    same(1, \"x\")\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0395"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_arithmetic_on_bare_type_parameter() {
        // `a + b` where both are the bare type variable `T` has no bounds, so
        // arithmetic is rejected (`L0307`).
        let source = concat!(
            "fn plus<T> a T b T -> T\n",
            "    a + b\n\n",
            "fn main -> i64\n",
            "    plus(1, 2)\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0307"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_ordering_on_bare_type_parameter() {
        let source = concat!(
            "fn less<T> a T b T -> bool\n",
            "    a < b\n\n",
            "fn main -> bool\n",
            "    less(1, 2)\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0327"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn allows_equality_between_two_same_type_parameters() {
        let source = concat!(
            "fn eq<T> a T b T -> bool\n",
            "    a == b\n\n",
            "fn main -> bool\n",
            "    eq(1, 1)\n",
        );
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_duplicate_type_parameter_list() {
        // A duplicate `<T, T>` list is caught at parse time as `L0394`, so the
        // program never reaches semantics.
        let source = concat!(
            "fn dup<T, T> a T -> T\n",
            "    a\n\n",
            "fn main -> i64\n",
            "    dup(1)\n",
        );
        let tokens = lex(source).expect("lex");
        let diagnostics = parse(&tokens).expect_err("parse");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0394"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_type_parameter_shadowing_builtin_type() {
        let source = concat!(
            "fn bad<i64> a i64 -> i64\n",
            "    a\n\n",
            "fn main -> i64\n",
            "    bad(1)\n",
        );
        let tokens = lex(source).expect("lex");
        let diagnostics = parse(&tokens).expect_err("parse");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0394"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_return_only_type_parameter() {
        // `T` appears only in the return type; nothing pins it: `L0396`.
        let source = concat!(
            "fn make<T> -> option<T>\n",
            "    none\n\n",
            "fn main -> i64\n",
            "    let _v option<i64> = make()\n",
            "    0\n",
        );
        let diagnostics = validate_source(source).expect_err("semantic");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0396"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn infers_generic_over_list_argument() {
        let source = concat!(
            "fn first<T> xs list<T> -> option<T>\n",
            "    if len(xs) == 0\n",
            "        return none\n",
            "    some(get(xs, 0))\n\n",
            "fn main -> i64\n",
            "    let xs list<i64> = list_new()\n",
            "    let ys list<i64> = push(xs, 7)\n",
            "    match first(ys)\n",
            "        some(v) -> v\n",
            "        none -> 0\n",
        );
        assert!(
            validate_source(source).is_ok(),
            "{:?}",
            validate_source(source).err()
        );
    }

    #[test]
    fn accepts_trait_impl_and_bounded_generic() {
        let source = concat!(
            "trait Show\n",
            "    fn show self -> string\n\n",
            "struct Point\n",
            "    x i64\n",
            "    y i64\n\n",
            "impl Show for Point\n",
            "    fn show self -> string\n",
            "        to_string(self.x)\n\n",
            "fn describe<T: Show> v T -> string\n",
            "    v.show()\n\n",
            "fn main -> i64\n",
            "    let p Point = Point(3, 4)\n",
            "    len(p.show()) + len(describe(p))\n",
        );
        assert!(
            validate_source(source).is_ok(),
            "{:?}",
            validate_source(source).err()
        );
    }

    #[test]
    fn rejects_incomplete_impl_with_l0398() {
        let source = concat!(
            "trait Show\n",
            "    fn show self -> string\n\n",
            "struct Point\n",
            "    x i64\n\n",
            "impl Show for Point\n",
            "    fn other self -> string\n",
            "        to_string(self.x)\n\n",
            "fn main -> i64\n",
            "    0\n",
        );
        let diagnostics = validate_source(source).expect_err("incomplete impl");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0398"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_duplicate_impl_with_l0399() {
        let source = concat!(
            "trait Show\n",
            "    fn show self -> string\n\n",
            "struct Point\n",
            "    x i64\n\n",
            "impl Show for Point\n",
            "    fn show self -> string\n",
            "        to_string(self.x)\n\n",
            "impl Show for Point\n",
            "    fn show self -> string\n",
            "        to_string(self.x)\n\n",
            "fn main -> i64\n",
            "    0\n",
        );
        let diagnostics = validate_source(source).expect_err("duplicate impl");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0399"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_unbounded_type_at_bounded_call_with_l0400() {
        // `describe` requires `T: Show`, but `i64` does not implement `Show`.
        let source = concat!(
            "trait Show\n",
            "    fn show self -> string\n\n",
            "struct Point\n",
            "    x i64\n\n",
            "impl Show for Point\n",
            "    fn show self -> string\n",
            "        to_string(self.x)\n\n",
            "fn describe<T: Show> v T -> string\n",
            "    v.show()\n\n",
            "fn main -> i64\n",
            "    len(describe(7))\n",
        );
        let diagnostics = validate_source(source).expect_err("unimplemented bound");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0400"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn accepts_socket_builtins() {
        // The socket builtins type-check with a `Socket` handle threaded through
        // a `result` `match`; `tcp_connect` yields `result<Socket, string>`.
        let source = concat!(
            "fn main -> i64\n",
            "    let outcome result<Socket, string> = tcp_connect(\"127.0.0.1\", 80)\n",
            "    match outcome\n",
            "        ok(conn) -> use_conn(conn)\n",
            "        err(message) -> len(message)\n\n",
            "fn use_conn conn Socket -> i64\n",
            "    let sent result<i64, string> = tcp_write(conn, \"hi\")\n",
            "    tcp_close(conn)\n",
            "    match sent\n",
            "        ok(count) -> count\n",
            "        err(message) -> 0 - 1\n",
        );
        validate_source(source).expect("socket builtins type-check");
    }

    #[test]
    fn rejects_wrong_type_socket_arg_with_l0335() {
        // `tcp_connect` expects `(string, i64)`; passing an i64 host is a type
        // error reported as L0335.
        let source = concat!(
            "fn main -> i64\n",
            "    let outcome result<Socket, string> = tcp_connect(5, 80)\n",
            "    match outcome\n",
            "        ok(conn) -> 0\n",
            "        err(message) -> 1\n",
        );
        let diagnostics = validate_source(source).expect_err("wrong socket arg type");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0335"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn accepts_http_builtins() {
        // `http_get`/`http_post` both yield `result<string, string>`.
        let source = concat!(
            "fn main -> i64\n",
            "    let got result<string, string> = http_get(\"http://127.0.0.1/\")\n",
            "    let posted result<string, string> = http_post(\"http://127.0.0.1/\", \"body\")\n",
            "    match got\n",
            "        ok(body) -> len(body)\n",
            "        err(message) -> len(message)\n",
        );
        validate_source(source).expect("http builtins type-check");
    }

    #[test]
    fn rejects_wrong_type_http_arg_with_l0336() {
        // `http_get` expects `(string)`; passing an i64 url is a type error
        // reported as L0336.
        let source = concat!(
            "fn main -> i64\n",
            "    let got result<string, string> = http_get(5)\n",
            "    match got\n",
            "        ok(body) -> 0\n",
            "        err(message) -> 1\n",
        );
        let diagnostics = validate_source(source).expect_err("wrong http arg type");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0336"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn accepts_assert_with_bool_argument() {
        // `assert(cond bool) -> void` type-checks when its argument is a bool.
        let source = "fn main -> void\n    assert(2 + 2 == 4)\n";
        validate_source(source).expect("assert with bool argument");
    }

    #[test]
    fn rejects_non_bool_assert_with_l0342() {
        // `assert` expects a single `bool`; passing an i64 is reported as L0342.
        let source = "fn main -> void\n    assert(5)\n";
        let diagnostics = validate_source(source).expect_err("non-bool assert argument");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0342"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn try_operator_on_result_type_checks_and_types_as_payload() {
        // `checked(a)?` is `i64` (the `ok` payload) inside a `result<i64, string>`
        // function; the `?` desugars to a propagate-on-`err` early return.
        let source = concat!(
            "fn checked n i64 -> result<i64, string>\n",
            "    if n < 0\n",
            "        return err(\"bad\")\n",
            "    ok(n)\n\n",
            "fn use_it a i64 -> result<i64, string>\n",
            "    let x i64 = checked(a)?\n",
            "    ok(x + 1)\n",
        );
        validate_source(source).expect("`?` on result in a result-returning fn");
    }

    #[test]
    fn try_operator_on_option_type_checks() {
        let source = concat!(
            "fn maybe present bool -> option<i64>\n",
            "    if present\n",
            "        return some(7)\n",
            "    none\n\n",
            "fn use_it p bool -> option<i64>\n",
            "    let x i64 = maybe(p)?\n",
            "    some(x + 1)\n",
        );
        validate_source(source).expect("`?` on option in an option-returning fn");
    }

    #[test]
    fn try_operator_in_incompatible_return_type_is_l0427() {
        // `?` on a `result` inside a plain `i64`-returning function has no
        // compatible propagation target, so it is `L0427`.
        let source = concat!(
            "fn checked n i64 -> result<i64, string>\n",
            "    ok(n)\n\n",
            "fn bad a i64 -> i64\n",
            "    let x i64 = checked(a)?\n",
            "    x\n",
        );
        let diagnostics = validate_source(source).expect_err("`?` needs a compatible return type");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0427"),
            "expected L0427: {diagnostics:?}"
        );
    }

    #[test]
    fn try_operator_with_mismatched_error_type_is_l0429() {
        // A `result<i64, string>` operand requires the function to return a
        // `result` with the SAME error type; returning `result<i64, i64>` is
        // `L0429`.
        let source = concat!(
            "fn checked n i64 -> result<i64, string>\n",
            "    ok(n)\n\n",
            "fn bad a i64 -> result<i64, i64>\n",
            "    let x i64 = checked(a)?\n",
            "    ok(x)\n",
        );
        let diagnostics = validate_source(source).expect_err("`?` error type must match");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0429"),
            "expected L0429: {diagnostics:?}"
        );
    }

    #[test]
    fn try_operator_on_non_option_result_is_l0428() {
        // `?` on a plain `i64` value is `L0428`.
        let source = concat!(
            "fn bad -> result<i64, string>\n",
            "    let n i64 = 5\n",
            "    let x i64 = n?\n",
            "    ok(x)\n",
        );
        let diagnostics = validate_source(source).expect_err("`?` needs an option/result operand");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0428"),
            "expected L0428: {diagnostics:?}"
        );
    }

    #[test]
    fn process_builtins_type_check_end_to_end() {
        // `proc_spawn` yields `result<process, string>`; the `process` handle
        // threads into `proc_wait`/`proc_stdout`/`proc_stderr`/`proc_kill`, each
        // returning the documented `result` type. This exercises the whole
        // process handle surface through the type checker.
        let source = concat!(
            "fn main -> i64\n",
            "    let spawned result<process, string> = proc_spawn(\"echo\", [\"hello\"])\n",
            "    match spawned\n",
            "        ok(p) -> drive(p)\n",
            "        err(message) -> 1\n",
            "\n",
            "fn drive p process -> i64\n",
            "    let waited result<i64, string> = proc_wait(p)\n",
            "    let out result<string, string> = proc_stdout(p)\n",
            "    let errs result<string, string> = proc_stderr(p)\n",
            "    let killed result<i64, string> = proc_kill(p)\n",
            "    0\n",
        );
        validate_source(source).expect("process builtins type-check");
    }

    #[test]
    fn rejects_proc_spawn_wrong_arg_type_with_l0335() {
        // `proc_spawn` expects `(string, array<string>)`; passing an i64 command
        // is rejected with the socket/network handle diagnostic family `L0335`.
        let source = concat!(
            "fn main -> i64\n",
            "    let spawned result<process, string> = proc_spawn(5, [\"hello\"])\n",
            "    match spawned\n",
            "        ok(p) -> 0\n",
            "        err(message) -> 1\n",
        );
        let diagnostics = validate_source(source).expect_err("wrong proc_spawn arg type");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0335"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn fixed_width_integer_arithmetic_and_conversions_type_check() {
        // i32/u32 values arithmetic and compare among themselves and convert to
        // and from i64 through the explicit `to_*` builtins.
        let source = concat!(
            "fn main -> i64\n",
            "    let a u32 = to_u32(10)\n",
            "    let b u32 = to_u32(3)\n",
            "    let c i32 = to_i32(0 - 1)\n",
            "    if c < to_i32(0)\n",
            "        return to_i64(a - b)\n",
            "    to_i64(a + b)\n",
        );
        validate_source(source).expect("i32/u32 arithmetic and conversions type-check");
    }

    #[test]
    fn overflow_arith_builtins_type_check_and_reject_i64() {
        // checked/saturating/wrapping on a fixed-width integer type-check; the
        // checked form yields option<T>, the others yield T.
        let ok = concat!(
            "fn main -> i64\n",
            "    let s u32 = saturating_add(to_u32(1), to_u32(2))\n",
            "    let w u32 = wrapping_mul(to_u32(3), to_u32(4))\n",
            "    match checked_sub(to_u32(1), to_u32(2))\n",
            "        some(v) -> to_i64(v)\n",
            "        none -> to_i64(s) + to_i64(w)\n",
        );
        validate_source(ok).expect("overflow arithmetic on u32 type-checks");
        // i64 is rejected: its default arithmetic already traps on overflow.
        let bad = concat!("fn main -> i64\n", "    checked_add(5, 6)\n");
        let diagnostics = validate_source(bad).expect_err("checked_add on i64 rejected");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0307"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn to_string_accepts_the_numeric_lattice() {
        // to_string renders every numeric type, not just i64/f64.
        let source = concat!(
            "fn main -> string\n",
            "    to_string(to_i32(1)) + to_string(to_u64(2)) + to_string(to_f32(3.0))\n",
        );
        validate_source(source).expect("to_string on fixed-width ints and f32 type-checks");
    }

    #[test]
    fn f32_arithmetic_and_conversions_type_check() {
        // f32 values arithmetic and compare among themselves and convert to and
        // from f64 through `to_f32`/`to_f64`; f32 never mixes with f64 directly.
        let source = concat!(
            "fn main -> i64\n",
            "    let a f32 = to_f32(1.5)\n",
            "    let b f32 = to_f32(2.5)\n",
            "    let sum f32 = a + b\n",
            "    if to_f64(sum) > 3.0\n",
            "        return 1\n",
            "    0\n",
        );
        validate_source(source).expect("f32 arithmetic and conversions type-check");
    }

    #[test]
    fn rejects_f32_f64_mixed_operands_with_l0307() {
        // No implicit float coercion: `f32 + f64` has no common numeric type.
        let source = concat!(
            "fn main -> i64\n",
            "    let a f32 = to_f32(1.0)\n",
            "    let bad f32 = a + 2.0\n",
            "    0\n",
        );
        let diagnostics = validate_source(source).expect_err("f32 + f64 must be rejected");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0307"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn wide_fixed_width_conversions_type_check() {
        // The full integer lattice (i8/i16/u16/u64/isize/usize) converts from and
        // back to i64, and each width arithmetic and compares among itself.
        let source = concat!(
            "fn main -> i64\n",
            "    let a i8 = to_i8(127)\n",
            "    let b i16 = to_i16(32767)\n",
            "    let c u16 = to_u16(0 - 1)\n",
            "    let d u64 = to_u64(0 - 1)\n",
            "    let e isize = to_isize(0 - 5)\n",
            "    let f usize = to_usize(9)\n",
            "    let g u64 = d / to_u64(2)\n",
            "    if a < to_i8(0)\n",
            "        return to_i64(g)\n",
            "    to_i64(b) + to_i64(c) + to_i64(e) + to_i64(f)\n",
        );
        validate_source(source).expect("wide fixed-width conversions type-check");
    }

    #[test]
    fn rejects_mixed_width_integer_operands_with_l0307() {
        // No implicit width mixing: `u32 + i32` has no common numeric type.
        let source = concat!(
            "fn main -> i64\n",
            "    let a u32 = to_u32(1)\n",
            "    let b i32 = to_i32(1)\n",
            "    let c u32 = a + b\n",
            "    to_i64(c)\n",
        );
        let diagnostics = validate_source(source).expect_err("u32 + i32 must be rejected");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0307"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_fixed_width_mixed_with_i64_with_l0307() {
        // A fixed-width integer does not silently mix with the default `i64`.
        let source = concat!(
            "fn main -> i64\n",
            "    let a i32 = to_i32(1)\n",
            "    let total i32 = a + 5\n",
            "    to_i64(total)\n",
        );
        let diagnostics = validate_source(source).expect_err("i32 + i64 must be rejected");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0307"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_to_i64_on_plain_i64_with_l0307() {
        // `to_i64` widens an i32/u32; a plain i64 argument is a type error.
        let source = concat!("fn main -> i64\n", "    to_i64(5)\n");
        let diagnostics = validate_source(source).expect_err("to_i64 needs an i32/u32 argument");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0307"),
            "{diagnostics:?}"
        );
    }
}
