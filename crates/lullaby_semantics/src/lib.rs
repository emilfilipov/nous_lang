use std::collections::{HashMap, HashSet};

use lullaby_diagnostics::Span;
use lullaby_parser::{
    AssignOp, BinaryOp, EnumVariant, Expr, ExprKind, Function, INFERRED_RETURN, MatchArm,
    MatchPattern, MethodSig, Place, Program, RegionDecl, Stmt, StructField, TypeParam, TypeRef,
    UnaryOp, function_type, generic_type,
};

mod semantics_actor_ownership;
mod semantics_aliases;
mod semantics_consts;
mod semantics_generics;
mod semantics_no_runtime;

// Re-export the generic-inference helpers at the crate root so the public API
// (`lullaby_semantics::unify_param`, ...) stays stable and the checker modules
// keep reaching them through `use super::*`.
pub(crate) use semantics_aliases::resolve_program_aliases;
pub use semantics_generics::{
    GenericInferenceError, first_unresolved_type_var, infer_generic_return, substitute_type,
    unify_param,
};
pub(crate) use semantics_generics::{decompose_generic, substitute_self, type_contains_var};

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

    // Evaluate named compile-time constants and fold every reference to a
    // constant into its literal value. After this the checker (and every
    // backend) only ever sees ordinary literals, so no downstream stage needs
    // any `const` awareness.
    let (const_types, const_diagnostics) = semantics_consts::resolve_and_fold_consts(&mut resolved);

    let mut checker = Checker::new(&resolved);
    checker.consts = const_types;
    checker.diagnostics = alias_diagnostics;
    checker.diagnostics.extend(const_diagnostics);
    checker.validate();
    // Freestanding-tier gate: in a `no-runtime` module, reject any construct that
    // requires the safe-tier runtime (growable heap allocation, actors/`spawn`/
    // `tell`, heap closures, `rc`/`ref` handles, host-allocator builtins) with
    // `L0441`. A no-op for a module without the directive, so ordinary programs are
    // completely unaffected. Runs after the main pass so it can consult the
    // recorded per-expression types (to catch heap-typed values without an
    // annotation, e.g. string building).
    semantics_no_runtime::enforce(
        &resolved,
        &checker.expression_types,
        &mut checker.diagnostics,
    );
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

/// Split a value type spelling into its head constructor name and its top-level
/// type arguments: `Box<i64>` -> (`"Box"`, `[i64]`), `Pair<i64, bool>` ->
/// (`"Pair"`, `[i64, bool]`), and a plain `Point` or scalar -> (`"Point"`, `[]`).
/// A function-type spelling `fn(...) -> R` has no angle-bracket head, so it is
/// returned whole with no arguments (function types never name a user struct).
fn split_named_type(ty: &TypeRef) -> (String, Vec<TypeRef>) {
    match ty.name.find('<') {
        Some(open) if ty.name.ends_with('>') && !ty.name.starts_with("fn(") => {
            let head = ty.name[..open].to_string();
            let args = ty.generic_args(&head).unwrap_or_default();
            (head, args)
        }
        _ => (ty.name.clone(), Vec::new()),
    }
}

/// Whether the value type `ty` embeds the enum named `enum_name` *by value* —
/// i.e. a recursion that would make `enum_name` infinitely sized. Used to enforce
/// the recursive-generic-enum indirection rule (`L0456`).
///
/// A payload that is the enum itself (`Tree<T>` inside `enum Tree<T>`) embeds it
/// by value. The only *other* constructors that store their arguments inline are
/// the built-in tagged unions `option`/`result`, so a recursive mention nested
/// inside one (`option<Tree<T>>`) is still by-value and is reported. Every
/// remaining constructor here — the pointer/heap-backed built-ins
/// `rc`/`ref`/`ptr`/`list`/`map`/`array`, and any user struct (a heap value on
/// the layout-driven backends) — breaks the value cycle, so recursion routed
/// through it is allowed (`node rc<Tree<T>>`, `node list<Tree<T>>`).
fn type_embeds_by_value(ty: &TypeRef, enum_name: &str) -> bool {
    let (head, args) = split_named_type(ty);
    if head == enum_name {
        return true;
    }
    if matches!(head.as_str(), "option" | "result") {
        return args.iter().any(|arg| type_embeds_by_value(arg, enum_name));
    }
    false
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
    /// Declared struct types: name -> ordered fields. For a generic struct the
    /// field types may mention the struct's type parameters (see
    /// `struct_type_params`); a use site substitutes them to concrete types.
    structs: HashMap<String, Vec<StructField>>,
    /// Declared type parameters of each struct, in `<...>` order (empty for a
    /// non-generic struct). A generic struct `Box<T>` records `["T"]`. Kept
    /// alongside `structs` so a use site can build the parameter -> concrete-type
    /// substitution for field-type resolution and instantiation checking.
    struct_type_params: HashMap<String, Vec<String>>,
    /// Declared enum types: enum name -> ordered variants. For a generic enum the
    /// variant payload types may mention the enum's type parameters (see
    /// `enum_type_params`); a use site substitutes them to concrete types.
    enums: HashMap<String, Vec<EnumVariant>>,
    /// Declared type parameters of each enum, in `<...>` order (empty for a
    /// non-generic enum). A generic enum `Opt<T>` records `["T"]`. Kept alongside
    /// `enums` so a use site can build the parameter -> concrete-type substitution
    /// for variant construction, `match`, and instantiation checking.
    enum_type_params: HashMap<String, Vec<String>>,
    /// Trait bounds on each generic type's parameters, keyed by the type name
    /// (struct or enum) and indexed by type-parameter position. Each entry is the
    /// set of trait names that a concrete argument in that position must satisfy.
    /// The bounds are the union of those written on the type declaration
    /// (`struct Sorted<T: Ord>`) and those written on any inherent impl of the
    /// type (`impl Sorted<T: Ord>`); position `i` collects every trait required of
    /// the `i`-th parameter. Used both to enforce bounds at each instantiation
    /// (`L0400`) and to make the bound's trait methods callable on a `T` value
    /// inside the type's methods.
    generic_type_bounds: HashMap<String, Vec<Vec<String>>>,
    /// Variant name -> (owning enum name, payload types). Variant names are
    /// globally unique across all enums, so this resolves construction directly.
    /// For a generic enum the payload types mention the enum's type parameters.
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
    /// Inherent (`impl Type<T>`) methods, keyed by `(base type name, method
    /// name)`. Each entry keeps the impl's type-parameter names, the `self` type
    /// spelling (`Box<T>`), the parameter types after `self`, and the return type
    /// — all still mentioning the type variables. A call site resolves the method
    /// by unifying `self` against the receiver's concrete instantiation and
    /// substituting the type variables into the parameter/return types.
    generic_impl_methods: HashMap<(String, String), InherentMethodSig>,
    /// Every method name declared in an inherent impl, so a call `m(recv, ...)`
    /// is recognized as a receiver-dispatched method rather than a free function.
    /// Disjoint from free-function and trait-method names.
    impl_method_names: HashSet<String>,
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
    /// Named compile-time constant declared types, keyed by name. References to a
    /// constant are folded into literals before checking, so this map is only a
    /// safety net: it types a reference to a constant whose *value* failed to
    /// evaluate (which left the reference un-folded), preventing a cascade of
    /// spurious `L0306` "unknown variable" diagnostics on top of the real
    /// constant error.
    consts: HashMap<String, TypeRef>,
    /// Declared actor types, keyed by actor name. Each entry records the actor's
    /// `init` parameter types (or `None` when the actor declares no `init`) and
    /// its message handlers' signatures, so `spawn`/`tell` sites can be typed and
    /// argument-checked. The private `state` fields are validated from the AST
    /// (`self.program.actors`) and are deliberately not exposed here — they are
    /// unreachable from outside the actor.
    actors: HashMap<String, ActorTypeInfo>,
}

/// The externally visible signature surface of an actor: its `init` parameter
/// types and its message handlers. Used to type and argument-check `spawn` and
/// `tell` expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActorTypeInfo {
    /// `init` parameter types in order, or `None` when the actor has no `init`
    /// (its `state` is zero-initialized and `spawn` takes no arguments).
    init_params: Option<Vec<TypeRef>>,
    /// Message handlers, keyed by handler name.
    handlers: HashMap<String, HandlerSig>,
}

/// A single actor handler's checkable signature: its parameter types and its
/// optional reply type. `reply_type = None` marks a fire-and-forget (`tell`)
/// handler; `Some(T)` marks a request-reply (`ask`) handler that `tell` may not
/// target.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HandlerSig {
    params: Vec<TypeRef>,
    reply_type: Option<TypeRef>,
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
            struct_type_params: HashMap::new(),
            enums: HashMap::new(),
            enum_type_params: HashMap::new(),
            generic_type_bounds: HashMap::new(),
            variants: HashMap::new(),
            traits: HashMap::new(),
            trait_methods: HashMap::new(),
            impl_methods: HashMap::new(),
            impl_traits: HashSet::new(),
            generic_impl_methods: HashMap::new(),
            impl_method_names: HashSet::new(),
            resolved_returns: HashMap::new(),
            inferring: HashSet::new(),
            deferred_diagnostics: Vec::new(),
            inference_failed: HashSet::new(),
            consts: HashMap::new(),
            actors: HashMap::new(),
        }
    }

    fn validate(&mut self) {
        self.collect_structs();
        self.collect_enums();
        self.collect_actors();
        self.validate_declared_type_wf();
        self.collect_traits();
        self.collect_signatures();
        self.collect_impls();
        self.collect_generic_type_bounds();
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
        //
        // For an inherent impl of a generic type, a method body may call a bound
        // trait's methods on a `T` value. The bound can be declared either on the
        // type (`struct Sorted<T: Ord>`) or on the impl (`impl Sorted<T: Ord>`);
        // both are unioned into `generic_type_bounds`. We attach that union onto a
        // clone of the method's type parameters (position-matched) so the body's
        // trait-method resolution (`type_param_has_bound`) sees the bound
        // regardless of where it was written.
        for decl in &self.program.impls {
            let type_bounds = if decl.is_inherent() {
                self.generic_type_bounds.get(&decl.type_name).cloned()
            } else {
                None
            };
            for method in &decl.methods {
                match &type_bounds {
                    // `generic_type_bounds` only retains types that carry at least
                    // one bound, so `Some` always means there is a bound to merge.
                    Some(bounds) => {
                        let mut method = method.clone();
                        for (index, tp) in method.type_params.iter_mut().enumerate() {
                            let Some(extra) = bounds.get(index) else {
                                continue;
                            };
                            for bound in extra {
                                if !tp.bounds.contains(bound) {
                                    tp.bounds.push(bound.clone());
                                }
                            }
                        }
                        self.validate_function(&method);
                    }
                    None => self.validate_function(method),
                }
            }
        }
        self.validate_actors();
    }

    /// Register every actor's externally visible signature (`init` parameters and
    /// handler signatures) and check declaration-level well-formedness: unique
    /// actor names (not colliding with a struct/enum), unique handler names, and
    /// unique state field names (`L0348`).
    fn collect_actors(&mut self) {
        for decl in &self.program.actors {
            if self.actors.contains_key(&decl.name)
                || self.structs.contains_key(&decl.name)
                || self.enums.contains_key(&decl.name)
            {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0348",
                    format!(
                        "`{}` is already declared; an actor name must be unique across actors, structs, and enums",
                        decl.name
                    ),
                    None,
                    decl.span,
                ));
                continue;
            }
            // Unique state field names.
            let mut seen_fields = HashSet::new();
            for field in &decl.state {
                if !seen_fields.insert(field.name.clone()) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0348",
                        format!(
                            "duplicate state field `{}` in actor `{}`",
                            field.name, decl.name
                        ),
                        None,
                        decl.span,
                    ));
                }
            }
            // Unique handler names; build the handler signature map.
            let mut handlers: HashMap<String, HandlerSig> = HashMap::new();
            for handler in &decl.handlers {
                if handlers.contains_key(&handler.name) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0348",
                        format!(
                            "duplicate handler `{}` in actor `{}`",
                            handler.name, decl.name
                        ),
                        None,
                        handler.span,
                    ));
                    continue;
                }
                handlers.insert(
                    handler.name.clone(),
                    HandlerSig {
                        params: handler.params.iter().map(|p| p.ty.clone()).collect(),
                        reply_type: handler.reply_type.clone(),
                    },
                );
            }
            let init_params = decl
                .init
                .as_ref()
                .map(|init| init.params.iter().map(|p| p.ty.clone()).collect());
            self.actors.insert(
                decl.name.clone(),
                ActorTypeInfo {
                    init_params,
                    handlers,
                },
            );
        }
    }

    /// Validate every actor's `init` and handler bodies. Each is checked like an
    /// ordinary function whose scope is the actor's `state` fields (assignable
    /// locals) plus the handler/init parameters; a reply (`ask`) handler's body
    /// must end in a value of its declared reply type. Also validates the
    /// well-formedness of state-field, init-parameter, and handler-parameter
    /// types. An actor with no handlers is rejected (`L0348`).
    fn validate_actors(&mut self) {
        for decl in &self.program.actors {
            if decl.handlers.is_empty() {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0348",
                    format!("actor `{}` declares no `on` handlers", decl.name),
                    None,
                    decl.span,
                ));
            }
            // State field types must be well-formed.
            for field in &decl.state {
                self.validate_type_wf(&field.ty, decl.span, Some(&decl.name), &[]);
            }
            // The `init` constructor: check its parameter types and its body with
            // the state fields (assignable) plus the parameters in scope.
            if let Some(init) = &decl.init {
                for param in &init.params {
                    self.validate_type_wf(&param.ty, init.span, Some(&decl.name), &[]);
                }
                let mut scope = self.actor_body_scope(decl, &init.params);
                let synth = self.synth_actor_function(
                    &format!("actor {}.init", decl.name),
                    TypeRef::new("void"),
                );
                self.check_function_body(&init.body, &mut scope, &synth);
                self.check_message_ownership(&synth.name, &init.body);
            }
            // Each handler: check its parameter types and its body. A reply
            // handler's body must produce a value of the reply type.
            for handler in &decl.handlers {
                for param in &handler.params {
                    self.validate_type_wf(&param.ty, handler.span, Some(&decl.name), &[]);
                }
                if let Some(reply) = &handler.reply_type {
                    self.validate_type_wf(reply, handler.span, Some(&decl.name), &[]);
                    // The reply value crosses the actor boundary back to the
                    // asker, so it obeys the same sendability rule as message
                    // arguments: a non-atomic `rc`/`ref`/`ptr` reply is `L0353`.
                    // Checked here at the declaration so a reply handler can never
                    // promise a non-sendable reply, whether or not it is `ask`ed.
                    let synth = self.synth_actor_function(
                        &format!("actor {}.{}", decl.name, handler.name),
                        reply.clone(),
                    );
                    self.check_sendable(reply, handler.span, &synth, "returned as an actor reply");
                }
                let return_type = handler
                    .reply_type
                    .clone()
                    .unwrap_or_else(|| TypeRef::new("void"));
                let mut scope = self.actor_body_scope(decl, &handler.params);
                let synth = self.synth_actor_function(
                    &format!("actor {}.{}", decl.name, handler.name),
                    return_type.clone(),
                );
                let block_type = self.check_function_body(&handler.body, &mut scope, &synth);
                self.check_message_ownership(&synth.name, &handler.body);
                if !return_type.is_void() && block_type.as_ref() != Some(&return_type) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0348",
                        format!(
                            "handler `{}` of actor `{}` declares reply type `{}` but has no final value of that type",
                            handler.name, decl.name, return_type.name
                        ),
                        Some(format!("actor {}.{}", decl.name, handler.name)),
                        handler.span,
                    ));
                }
            }
        }
    }

    /// Build the local scope for an actor `init`/handler body: the actor's state
    /// fields (mutable, assignable locals) with the handler/init parameters
    /// layered on top (a parameter shadows a state field of the same name).
    fn actor_body_scope(
        &self,
        decl: &lullaby_parser::ActorDecl,
        params: &[lullaby_parser::Param],
    ) -> Scope {
        let mut scope = Scope::default();
        for field in &decl.state {
            scope.locals.insert(field.name.clone(), field.ty.clone());
        }
        for param in params {
            scope.locals.insert(param.name.clone(), param.ty.clone());
        }
        scope
    }

    /// Synthesize a throwaway `Function` for an actor `init`/handler body so the
    /// existing body checker can name diagnostics and drive
    /// tail-value/return-type handling. Its `params`/`body` are empty (the real
    /// scope and body are passed separately to `check_function_body`); only its
    /// `name` and `return_type` are consulted.
    fn synth_actor_function(&self, name: &str, return_type: TypeRef) -> Function {
        Function {
            name: name.to_string(),
            type_params: Vec::new(),
            params: Vec::new(),
            return_type,
            body: Vec::new(),
            span: Span::new(1, 1),
            is_public: false,
            is_async: false,
            is_extern: false,
            is_export: false,
        }
    }

    /// Type-check a `spawn NAME(args)` expression, producing `Actor<NAME>`.
    /// Rejects an unknown actor, an argument count that does not match the
    /// actor's `init`, an argument whose type differs from the corresponding
    /// `init` parameter, and a non-sendable argument (`L0349`/`L0353`).
    fn check_spawn(
        &mut self,
        actor: &str,
        args: &[Expr],
        span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let arg_types: Vec<Option<TypeRef>> = args
            .iter()
            .map(|arg| self.check_expr(arg, scope, function))
            .collect();
        let Some(info) = self.actors.get(actor) else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0349",
                format!("`spawn` names unknown actor `{actor}`"),
                Some(function.name.clone()),
                span,
            ));
            return None;
        };
        let expected: Vec<TypeRef> = info.init_params.clone().unwrap_or_default();
        let init_note = if info.init_params.is_some() {
            format!("actor `{actor}`'s `init`")
        } else {
            format!("actor `{actor}` (which declares no `init`)")
        };
        if args.len() != expected.len() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0349",
                format!(
                    "`spawn {actor}` expects {} argument(s) for {init_note} but got {}",
                    expected.len(),
                    args.len()
                ),
                Some(function.name.clone()),
                span,
            ));
        } else {
            for (index, (arg, param_ty)) in args.iter().zip(expected.iter()).enumerate() {
                if let Some(actual) = &arg_types[index] {
                    if actual != param_ty {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0349",
                            format!(
                                "`spawn {actor}` argument {} expects `{}` but got `{}`",
                                index + 1,
                                param_ty.name,
                                actual.name
                            ),
                            Some(function.name.clone()),
                            arg.span,
                        ));
                    }
                    self.check_sendable(actual, arg.span, function, "spawned into an actor");
                }
            }
        }
        Some(generic_type("Actor", &[TypeRef::new(actor)]))
    }

    /// Type-check a message send. The one entry point serves both forms:
    ///
    /// - `tell TARGET.HANDLER(args)` (`is_ask == false`) — fire-and-forget,
    ///   produces `void`; the handler must be a `tell` handler (no `-> T`).
    /// - `ask TARGET.HANDLER(args)` (`is_ask == true`) — request-reply, produces
    ///   `Future<R>`; the handler must be a reply handler (declared `-> R`).
    ///
    /// In both cases the target must be an `Actor<T>` handle, the handler must
    /// exist, and the arguments must match the handler's parameters in count,
    /// type, and sendability (`L0352`/`L0353`). A mismatched send form (a `tell`
    /// to a reply handler, or an `ask` to a fire-and-forget handler) is `L0352`.
    #[allow(clippy::too_many_arguments)]
    fn check_send(
        &mut self,
        target: &Expr,
        handler: &str,
        args: &[Expr],
        is_ask: bool,
        span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let verb = if is_ask { "ask" } else { "tell" };
        // The result type on any early (error) exit: `void` for `tell`, and `None`
        // for `ask` (its `Future<R>` is unknown, so callers/`await` should not see
        // a bogus type).
        let error_result = || {
            if is_ask {
                None
            } else {
                Some(TypeRef::new("void"))
            }
        };

        let target_type = self.check_expr(target, scope, function);
        let arg_types: Vec<Option<TypeRef>> = args
            .iter()
            .map(|arg| self.check_expr(arg, scope, function))
            .collect();
        let target_type = target_type?;
        let Some(actor_name) = actor_handle_target(&target_type) else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0352",
                format!(
                    "`{verb}` target must be an `Actor<T>` handle but got `{}`",
                    target_type.name
                ),
                Some(function.name.clone()),
                target.span,
            ));
            return error_result();
        };
        let Some(info) = self.actors.get(&actor_name) else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0352",
                format!("`Actor<{actor_name}>` does not name a declared actor"),
                Some(function.name.clone()),
                target.span,
            ));
            return error_result();
        };
        let Some(sig) = info.handlers.get(handler).cloned() else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0352",
                format!("actor `{actor_name}` has no handler `{handler}`"),
                Some(function.name.clone()),
                span,
            ));
            return error_result();
        };
        // Enforce that the send form matches the handler kind.
        match (is_ask, &sig.reply_type) {
            (false, Some(reply)) => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0352",
                    format!(
                        "cannot `tell` handler `{handler}` of actor `{actor_name}`: it declares a reply type (`-> {}`), so it is an `ask` handler — send it with `ask` and `await` the reply",
                        reply.name
                    ),
                    Some(function.name.clone()),
                    span,
                ));
                return Some(TypeRef::new("void"));
            }
            (true, None) => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0352",
                    format!(
                        "cannot `ask` handler `{handler}` of actor `{actor_name}`: it is a fire-and-forget handler (no `-> T` reply type), so it has no reply to await — send it with `tell`"
                    ),
                    Some(function.name.clone()),
                    span,
                ));
                return None;
            }
            _ => {}
        }
        if args.len() != sig.params.len() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0352",
                format!(
                    "`{verb} {actor_name}.{handler}` expects {} argument(s) but got {}",
                    sig.params.len(),
                    args.len()
                ),
                Some(function.name.clone()),
                span,
            ));
        } else {
            for (index, (arg, param_ty)) in args.iter().zip(sig.params.iter()).enumerate() {
                if let Some(actual) = &arg_types[index] {
                    if actual != param_ty {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0352",
                            format!(
                                "`{verb} {actor_name}.{handler}` argument {} expects `{}` but got `{}`",
                                index + 1,
                                param_ty.name,
                                actual.name
                            ),
                            Some(function.name.clone()),
                            arg.span,
                        ));
                    }
                    self.check_sendable(actual, arg.span, function, "sent in an actor message");
                }
            }
        }
        // `tell` yields `void`; `ask` yields `Future<R>` for the handler's reply
        // type `R`. The reply value crosses the actor boundary back to the asker,
        // so `R`'s sendability is enforced at the handler declaration
        // (`validate_actors`); here we only need to surface the future type.
        match sig.reply_type {
            Some(reply) => Some(future_type(&reply)),
            None => Some(TypeRef::new("void")),
        }
    }

    /// Enforce that a value crossing an actor boundary is **sendable**: it must
    /// not be (or transitively contain) a non-atomic `rc<T>`, a borrowed
    /// `ref<T>`, or a raw `ptr<T>` — none of which may be aliased into a second
    /// actor's isolated heap. A violation is `L0353`. This is the structural,
    /// compiler-derived analogue of Rust's `Send`; keeping `rc`/`ref`/`ptr` off
    /// the wire is exactly what lets per-actor reference counting stay
    /// non-atomic.
    fn check_sendable(&mut self, ty: &TypeRef, span: Span, function: &Function, context: &str) {
        if let Some(offender) = self.first_non_sendable(ty) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0353",
                format!(
                    "value of type `{}` cannot be {context}: `{}` is not sendable across an actor boundary (a non-atomic `rc`/`ref`/`ptr` must not be shared between actors)",
                    ty.name, offender
                ),
                Some(function.name.clone()),
                span,
            ));
        }
    }

    /// Build `generic_type_bounds`: for every generic struct and enum, the set of
    /// trait bounds required of each type parameter position. The bounds are the
    /// union of those written on the type declaration (`struct Sorted<T: Ord>`,
    /// `enum Opt<T: Show>`) and those written on any inherent impl of that type
    /// (`impl Sorted<T: Ord>`). Runs after `collect_impls` so every inherent impl
    /// is known. Position `i` in the returned vector holds the trait names for the
    /// `i`-th parameter; a type with no bounds anywhere maps to all-empty vectors
    /// (and is treated as unbounded at instantiation).
    fn collect_generic_type_bounds(&mut self) {
        let mut bounds: HashMap<String, Vec<Vec<String>>> = HashMap::new();
        let mut merge = |name: &str, type_params: &[TypeParam]| {
            let entry = bounds
                .entry(name.to_string())
                .or_insert_with(|| vec![Vec::new(); type_params.len()]);
            if entry.len() < type_params.len() {
                entry.resize(type_params.len(), Vec::new());
            }
            for (index, tp) in type_params.iter().enumerate() {
                for bound in &tp.bounds {
                    if !entry[index].contains(bound) {
                        entry[index].push(bound.clone());
                    }
                }
            }
        };
        for decl in &self.program.structs {
            merge(&decl.name, &decl.type_params);
        }
        for decl in &self.program.enums {
            merge(&decl.name, &decl.type_params);
        }
        for decl in &self.program.impls {
            if decl.is_inherent() {
                merge(&decl.type_name, &decl.type_params);
            }
        }
        // Drop entries with no bounds at all so `generic_type_bounds` lookups are a
        // fast "is anything required here" test.
        bounds.retain(|_, positions| positions.iter().any(|b| !b.is_empty()));
        self.generic_type_bounds = bounds;
    }

    /// Enforce that every concrete type argument in a generic instantiation
    /// satisfies the bounds declared for that parameter position (`L0400`).
    /// `owner` is the enclosing function/method name for diagnostics; a type
    /// argument that is itself an in-scope type variable is skipped (its bound, if
    /// any, is enforced where that outer variable is pinned). Only the top-level
    /// head of `type_args` is checked here; nested spellings are validated by the
    /// caller that walks them (`validate_type_wf`) or by their own construction.
    fn enforce_type_arg_bounds(
        &mut self,
        head: &str,
        type_args: &[TypeRef],
        type_params_in_scope: &[String],
        owner: Option<&str>,
        span: Span,
    ) {
        let Some(bounds) = self.generic_type_bounds.get(head).cloned() else {
            return;
        };
        for (index, arg) in type_args.iter().enumerate() {
            let Some(required) = bounds.get(index) else {
                continue;
            };
            if required.is_empty() {
                continue;
            }
            // A bare type-variable argument cannot be checked concretely here.
            if type_params_in_scope.iter().any(|p| p == &arg.name) {
                continue;
            }
            let dispatch = dispatch_type_name(arg);
            for bound in required {
                if !self
                    .impl_traits
                    .contains(&(dispatch.clone(), bound.clone()))
                {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0400",
                        format!(
                            "type `{}` for type parameter of generic type `{head}` does not implement bound trait `{bound}`",
                            arg.name
                        ),
                        owner.map(str::to_string),
                        span,
                    ));
                }
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
    /// pointer `ptr<T>` (a machine address, passed/returned by value), a
    /// **callback** `fn(A...) -> R` whose own signature is C-marshallable (a
    /// function pointer C can invoke — §7), and — for a **parameter only** — the
    /// FFI-only `cstr` marker (a Lullaby `string` is materialized into a
    /// NUL-terminated buffer across the boundary). A return type may additionally be
    /// `void`. Anything else — a `string`, `list`/`map`, non-`repr(C)` struct/enum,
    /// a callback whose own signature is *not* C-marshallable, array, or a `cstr`
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
                        "`extern fn {}` parameter `{}` has type `{}`; an extern parameter must be a C scalar (`i8`…`u64`/`isize`/`usize`/`bool`/`char`/`byte`/`f32`/`f64`), a raw pointer `ptr<T>`, `cstr`, or a callback `fn(...) -> R` whose own parameters/return are C scalars or raw pointers (structs by value, a callback taking a `string`/`list`/struct, `string`, `list`/`map` are not yet marshallable)",
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
    /// pointer `ptr<T>`, the FFI-only `cstr` marker (a materialized C string), or a
    /// **callback** — a function pointer `fn(A...) -> R` whose own signature is
    /// itself C-marshallable (§7). A callback lets a Lullaby top-level function be
    /// passed to C as a C-ABI function pointer; a callback whose own parameters or
    /// return are not C-marshallable (e.g. it takes a `string`/`list`/struct) stays
    /// rejected with `L0424`.
    fn is_extern_param_type(ty: &TypeRef) -> bool {
        Self::is_ffi_scalar(&ty.name)
            || ty.name == "cstr"
            || ty.is_raw_pointer()
            || Self::is_marshallable_callback(ty)
    }

    /// Whether `ty` spells a **callback** (function-pointer) type `fn(A...) -> R`
    /// whose own signature is C-marshallable, so it can cross the FFI boundary as a
    /// C function pointer `R (*)(A...)`. Every parameter must be a C scalar or a raw
    /// pointer, and the return must be `void`, a C scalar, or a raw pointer. A
    /// callback is invoked *by C*, so — unlike an outbound `extern` parameter — its
    /// own parameters cannot use the outbound-only `cstr` marker (an inbound C
    /// string is the deferred `ptr<byte>`/owned-string case) and cannot be a nested
    /// callback; those forms keep the C boundary's marshalling all-register and
    /// trampoline-free, matching the top-level-function callback increment (§7).
    fn is_marshallable_callback(ty: &TypeRef) -> bool {
        let Some((params, ret)) = ty.function_signature() else {
            return false;
        };
        params
            .iter()
            .all(|param| Self::is_ffi_scalar(&param.name) || param.is_raw_pointer())
            && (ret.is_void() || Self::is_ffi_scalar(&ret.name) || ret.is_raw_pointer())
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
            // An inherent impl (`impl Box<T>`) registers receiver-dispatched
            // methods with their type variables intact, rather than satisfying a
            // trait. Handled separately from the trait-impl path below.
            if decl.is_inherent() {
                self.collect_inherent_impl(decl);
                continue;
            }
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

    /// Register the methods of an inherent impl (`impl Box<T>`) for receiver-type
    /// dispatch. Each method keeps its type variables; a call site pins them by
    /// unifying `self` against the receiver's concrete instantiation. Method names
    /// must be globally unique and disjoint from free-function and trait-method
    /// names (`L0398`); a method redeclared for the same type is a duplicate
    /// (`L0399`).
    fn collect_inherent_impl(&mut self, decl: &lullaby_parser::ImplDecl) {
        let base = &decl.type_name;
        let type_params: Vec<String> = decl.type_params.iter().map(|tp| tp.name.clone()).collect();
        for method in &decl.methods {
            // The `self` receiver type is the method's first parameter (the full
            // instantiation spelling `Box<T>` injected by the parser).
            let self_ty = method
                .params
                .first()
                .map(|param| param.ty.clone())
                .unwrap_or_else(|| TypeRef::new(base.clone()));
            let params: Vec<TypeRef> = method
                .params
                .iter()
                .skip(1)
                .map(|param| param.ty.clone())
                .collect();
            let key = (base.clone(), method.name.clone());
            if self.generic_impl_methods.contains_key(&key) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0399",
                    format!(
                        "method `{}` is declared more than once for type `{base}`",
                        method.name
                    ),
                    Some(method.name.clone()),
                    method.span,
                ));
                continue;
            }
            // A method name must not collide with a free function or a trait
            // method: a call `m(recv, ...)` must resolve to exactly one of them.
            if self.signatures.contains_key(&method.name)
                || self.trait_methods.contains_key(&method.name)
            {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0398",
                    format!(
                        "method `{}` collides with a free function or trait method of the same name; method, free-function, and trait-method names must be disjoint",
                        method.name
                    ),
                    Some(method.name.clone()),
                    method.span,
                ));
                continue;
            }
            self.generic_impl_methods.insert(
                key,
                InherentMethodSig {
                    type_params: type_params.clone(),
                    self_ty,
                    params,
                    return_type: method.return_type.clone(),
                },
            );
            self.impl_method_names.insert(method.name.clone());
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
            self.struct_type_params.insert(
                declaration.name.clone(),
                declaration
                    .type_params
                    .iter()
                    .map(|tp| tp.name.clone())
                    .collect(),
            );
        }
    }

    /// Validate that every declared struct field type and enum variant payload
    /// type is well-formed with respect to the declared generic user types: any
    /// generic struct or enum it mentions must be given the right number of type
    /// arguments (`L0454`), and only the owning declaration's own type parameters
    /// are in scope (so a bare type variable `T` is accepted, an out-of-scope
    /// name is treated as an opaque type). Runs after both `collect_structs` and
    /// `collect_enums` so a struct field may reference a generic enum and vice
    /// versa and still get its arity checked.
    fn validate_declared_type_wf(&mut self) {
        for declaration in &self.program.structs {
            let in_scope: Vec<String> = declaration
                .type_params
                .iter()
                .map(|tp| tp.name.clone())
                .collect();
            for field in &declaration.fields {
                self.validate_type_wf(&field.ty, declaration.span, None, &in_scope);
            }
        }
        for declaration in &self.program.enums {
            let in_scope: Vec<String> = declaration
                .type_params
                .iter()
                .map(|tp| tp.name.clone())
                .collect();
            for variant in &declaration.variants {
                for payload in &variant.payload {
                    self.validate_type_wf(payload, declaration.span, None, &in_scope);
                }
            }
        }
    }

    /// Validate that a type spelling is well-formed with respect to declared
    /// generic user structs: every mention of a generic struct must supply the
    /// exact number of type arguments its declaration expects (`L0454`).
    /// Recurses into the type arguments of compound spellings. A name that is a
    /// type parameter currently in scope is never treated as a struct head, so a
    /// bare type variable `T` is accepted. Non-struct heads (primitives, built-in
    /// generics, unknown names) are ignored here — they are validated, or left
    /// opaque, by the existing rules.
    fn validate_type_wf(
        &mut self,
        ty: &TypeRef,
        span: Span,
        owner: Option<&str>,
        type_params_in_scope: &[String],
    ) {
        let (head, args) = split_named_type(ty);
        // `Actor<Name>` is the actor-handle type: exactly one argument naming a
        // declared actor. Handle it before recursing into the arguments, because
        // the sole argument names an actor (which is *not* a standalone value
        // type and would otherwise be flagged by the bare-actor rule below).
        if head == "Actor" {
            match args.as_slice() {
                [arg] if self.actors.contains_key(&arg.name) => {}
                _ => {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0354",
                        format!(
                            "`{}` is not a valid actor handle type; write `Actor<Name>` where `Name` is a declared actor",
                            ty.name
                        ),
                        owner.map(str::to_string),
                        span,
                    ));
                }
            }
            return;
        }
        for arg in &args {
            self.validate_type_wf(arg, span, owner, type_params_in_scope);
        }
        if type_params_in_scope.iter().any(|p| p == &head) {
            return;
        }
        // A bare actor name is not a value type: an actor is reachable only
        // through an `Actor<Name>` handle, never stored or passed by value.
        if self.actors.contains_key(&head) && args.is_empty() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0354",
                format!(
                    "actor `{head}` cannot be used as a value type; use the handle type `Actor<{head}>`"
                ),
                owner.map(str::to_string),
                span,
            ));
            return;
        }
        if let Some(params) = self.struct_type_params.get(&head) {
            let expected = params.len();
            if args.len() != expected {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0454",
                    format!(
                        "generic struct `{head}` expects {expected} type argument(s) but got {} in `{}`",
                        args.len(),
                        ty.name
                    ),
                    owner.map(str::to_string),
                    span,
                ));
            } else {
                self.enforce_type_arg_bounds(&head, &args, type_params_in_scope, owner, span);
            }
        } else if let Some(params) = self.enum_type_params.get(&head) {
            let expected = params.len();
            if args.len() != expected {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0454",
                    format!(
                        "generic enum `{head}` expects {expected} type argument(s) but got {} in `{}`",
                        args.len(),
                        ty.name
                    ),
                    owner.map(str::to_string),
                    span,
                ));
            } else {
                self.enforce_type_arg_bounds(&head, &args, type_params_in_scope, owner, span);
            }
        }
    }

    /// The field list of a struct value type, with any generic type parameters
    /// substituted to the concrete type arguments carried by the spelling. For a
    /// non-generic struct `Point` this is its declared fields verbatim; for a
    /// generic instance `Box<i64>` the field types have the parameter `T`
    /// replaced by `i64`. Returns `None` when the head name is not a declared
    /// struct, or when the type-argument arity does not match its declaration
    /// (that arity error is reported at the use site by `validate_type_wf`).
    pub(crate) fn struct_fields_for(&self, ty: &TypeRef) -> Option<Vec<StructField>> {
        let (head, args) = split_named_type(ty);
        let fields = self.structs.get(&head)?;
        let params = self
            .struct_type_params
            .get(&head)
            .cloned()
            .unwrap_or_default();
        if params.is_empty() {
            return Some(fields.clone());
        }
        if params.len() != args.len() {
            return None;
        }
        let mut subst: HashMap<String, TypeRef> = HashMap::new();
        for (param, arg) in params.iter().zip(args.iter()) {
            subst.insert(param.clone(), arg.clone());
        }
        Some(
            fields
                .iter()
                .map(|field| StructField {
                    name: field.name.clone(),
                    ty: substitute_type(&field.ty, &subst),
                })
                .collect(),
        )
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
            self.enum_type_params.insert(
                declaration.name.clone(),
                declaration
                    .type_params
                    .iter()
                    .map(|tp| tp.name.clone())
                    .collect(),
            );
        }
        // With every enum registered, reject a generic enum that recurses on
        // itself *directly* (by value): such a type is infinitely sized. The
        // recursion must pass through a built-in indirection (`rc`/`ref`/`ptr`/
        // `list`/`map`/`array`), e.g. `node rc<Tree<T>>` — else `L0456`. Scoped to
        // generic enums, the surface this stage introduces; non-generic recursive
        // enums keep their existing handling.
        for declaration in &self.program.enums {
            if declaration.type_params.is_empty() {
                continue;
            }
            for variant in &declaration.variants {
                for payload in &variant.payload {
                    if type_embeds_by_value(payload, &declaration.name) {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "L0456",
                            format!(
                                "enum `{}` recurses on itself directly through variant `{}` payload `{}`, which is infinitely sized; route the recursion through an indirection such as `rc<{}<...>>`, `list<{}<...>>`, or `ptr<{}<...>>`",
                                declaration.name,
                                variant.name,
                                payload.name,
                                declaration.name,
                                declaration.name,
                                declaration.name,
                            ),
                            None,
                            declaration.span,
                        ));
                    }
                }
            }
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
        // Any generic struct named in a parameter or return type must be given
        // the right number of type arguments. The function's own type
        // parameters are in scope so a bare type variable is not mistaken for a
        // struct head.
        let fn_type_params: Vec<String> = function
            .type_params
            .iter()
            .map(|tp| tp.name.clone())
            .collect();
        for param in &function.params {
            self.validate_type_wf(
                &param.ty,
                function.span,
                Some(&function.name),
                &fn_type_params,
            );
        }
        self.validate_type_wf(
            &function.return_type,
            function.span,
            Some(&function.name),
            &fn_type_params,
        );
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
        // Move-by-default use-after-send analysis (concurrency stage 3): a
        // non-copy value moved into a `tell`/`ask`/`spawn` message may not be used
        // again (`L0357`). Runs after the body is type-checked so argument types
        // are recorded.
        self.check_message_ownership(&function.name, &function.body);
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
                // A generic struct named in the binding's annotation must carry
                // the right number of type arguments (the function's type
                // parameters stay in scope so a bare type variable is allowed).
                if let Some(declared) = ty {
                    let fn_type_params: Vec<String> = function
                        .type_params
                        .iter()
                        .map(|tp| tp.name.clone())
                        .collect();
                    self.validate_type_wf(
                        declared,
                        value.span,
                        Some(&function.name),
                        &fn_type_params,
                    );
                }
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
                // An actor's `state` is private: an external write through an
                // `Actor<T>` handle (`c.count = ...`) is rejected (`L0354`).
                if !path.is_empty()
                    && let Some(actor_name) = actor_handle_target(&root)
                {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0354",
                        format!(
                            "cannot assign to the private state of `Actor<{actor_name}>`; actor state is reachable only from the actor's own handlers"
                        ),
                        Some(function.name.clone()),
                        *span,
                    ));
                    return None;
                }
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
            ExprKind::Spawn { args, .. } => {
                for arg in args {
                    self.check_freed_uses(arg, freed, function);
                }
            }
            ExprKind::Tell { target, args, .. } => {
                self.check_freed_uses(target, freed, function);
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
                            // A unit variant of a generic enum (`absent` of
                            // `Opt<T>`) cannot pin its type parameters from the
                            // (absent) payload, so the concrete instantiation must
                            // come from the contextual expected `Opt<...>` type;
                            // without one it is uninferable (`L0455`).
                            self.resolve_unit_variant_type(
                                &enum_name, name, expected, expr.span, function,
                            )
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
                    } else if let Some(const_type) = self.consts.get(name) {
                        // A reference to a named constant. Cleanly-evaluated
                        // constants were already folded to literals, so this only
                        // fires for a constant whose value failed to evaluate; its
                        // real error is already reported, and typing it by its
                        // declared type keeps the failure from cascading into a
                        // spurious `L0306`.
                        Some(const_type.clone())
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
                    self.check_enum_construction(name, args, expected, expr.span, scope, function)
                } else if self.structs.contains_key(name) {
                    self.check_struct_construction(name, args, expected, expr.span, scope, function)
                } else {
                    self.check_call(name, args, expr.span, expected, scope, function)
                }
            }
            ExprKind::StructLiteral { name, fields } => {
                self.check_struct_literal(name, fields, expected, expr.span, scope, function)
            }
            ExprKind::Spawn { actor, args } => {
                self.check_spawn(actor, args, expr.span, scope, function)
            }
            ExprKind::Tell {
                target,
                handler,
                args,
                is_ask,
            } => self.check_send(target, handler, args, *is_ask, expr.span, scope, function),
            ExprKind::Field { target, field } => {
                let target_type = self.check_expr(target, scope, function)?;
                // An actor's `state` is private: an `Actor<T>` handle exposes no
                // fields, so any field access on one is rejected (`L0354`). This
                // is what keeps actor state single-writer and race-free.
                if let Some(actor_name) = actor_handle_target(&target_type) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "L0354",
                        format!(
                            "cannot access field `{field}` on `Actor<{actor_name}>`: an actor's `state` is private and reachable only from its own handlers"
                        ),
                        Some(function.name.clone()),
                        expr.span,
                    ));
                    return None;
                }
                match self.struct_fields_for(&target_type) {
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
                self.check_match(scrutinee, arms, expected, expr.span, scope, function)
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
}

#[path = "semantics_checker_calls.rs"]
mod checker_calls;

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

/// The resolved signature of an inherent (`impl Type<T>`) method, kept with its
/// type variables intact. A call site unifies `self_ty` against the receiver's
/// concrete instantiation to pin the type variables, then substitutes them into
/// `params` and `return_type`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InherentMethodSig {
    /// The impl's type-parameter names (`["T"]` for `impl Box<T>`).
    type_params: Vec<String>,
    /// The `self` parameter's type spelling (`Box<T>`), unified against the
    /// receiver type to pin the type variables.
    self_ty: TypeRef,
    /// Parameter types after `self`, still mentioning the type variables.
    params: Vec<TypeRef>,
    /// The return type, still mentioning the type variables.
    return_type: TypeRef,
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

/// The actor name `T` of an `Actor<T>` handle spelling, if `ty` is one.
fn actor_handle_target(ty: &TypeRef) -> Option<String> {
    ty.generic_arg("Actor").map(|inner| inner.name)
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
