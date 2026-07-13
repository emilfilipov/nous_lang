//! The IR tree-walking interpreter (`IrRuntime`). Split out of lib.rs; it
//! evaluates an `IrModule` directly (the middle interpreter tier between the AST
//! runtime and the bytecode VM). Uses the crate's IR types via `use super::*`.

use super::*;

/// The function value an IR `parallel_map` runs on each worker thread: either a
/// named top-level function or a self-contained capturing closure. Both are
/// `Send`, so they cross the scoped-thread boundary safely.
#[derive(Debug, Clone)]
enum IrParallelCallable {
    Func(String),
    Closure(Closure),
}

/// One entry in the interpreter's active call stack, mirroring the AST
/// runtime's `CallFrame`: the function name is *borrowed* from the program
/// (`&'a str`) so pushing a frame per call is allocation-free; owned
/// [`TraceFrame`]s are materialized only when a traceback is attached on error.
struct CallFrame<'a> {
    function: &'a str,
    span: Option<Span>,
}

pub(crate) struct IrRuntime<'a> {
    /// The whole IR module, borrowed so a builtin can spawn sibling interpreters
    /// over the same shared `&IrModule` (used by `parallel_map`'s scoped threads).
    module: &'a IrModule,
    /// An owned share of the same module, handed by `.clone()` to detached
    /// threads created by `spawn` so they can build their own interpreter over
    /// `&*arc` and outlive the `spawn` call. Separate handle, not self-referential.
    module_arc: Arc<IrModule>,
    functions: HashMap<&'a str, &'a IrFunction>,
    /// The running program's CLI arguments, exposed by the `args()` builtin.
    pub(crate) program_args: Vec<String>,
    structs: HashMap<&'a str, Vec<String>>,
    /// Enum variant name -> owning enum name. Variant names are globally unique.
    variants: HashMap<&'a str, &'a str>,
    heap: Vec<Option<Value>>,
    refcounts: HashMap<usize, usize>,
    /// Per-runtime table of open network sockets, mirroring the AST interpreter.
    /// A `Value::Socket(i)` indexes this vector; closing a socket clears its slot.
    sockets: Vec<Option<SocketResource>>,
    /// Per-runtime table of live external processes, mirroring the AST interpreter.
    /// A `Value::Process(i)` indexes this vector.
    processes: Vec<Option<ProcessResource>>,
    call_stack: Vec<CallFrame<'a>>,
    /// Trait-method dispatch table: `(receiver type name, method name)` -> impl
    /// function. Built once from every `impl` in the module.
    impl_methods: HashMap<(String, String), &'a IrFunction>,
    /// Names that are trait methods; a call to one dispatches via `impl_methods`.
    trait_method_names: std::collections::HashSet<String>,
    /// Names of `async fn` functions. Calling one spawns an OS thread running its
    /// body and yields a `Value::Future` that `await` resolves.
    async_functions: std::collections::HashSet<String>,
    /// Names of `extern fn` (C-ABI) functions. The interpreter cannot execute C,
    /// so a call to one raises `L0423` rather than dispatching a body.
    extern_functions: std::collections::HashSet<String>,
    /// Closure-body table: `closure id -> lowered closure def`. Built once from
    /// `module.closures`. A `Value::Closure` carries only its id, so an invocation
    /// looks its body up here. Bodies borrow the module with lifetime `'a`.
    closures: HashMap<usize, &'a IrClosureDef>,
    /// A free-list of reusable per-call environments. Each call needs a fresh
    /// `Env`; borrowing a reset one from here and returning it on the normal exit
    /// path lets deep/repeated calls reuse the scope buffers instead of
    /// reallocating. Only returned on success; error paths drop theirs.
    env_pool: Vec<Env>,
    /// The bytecode tier sets this: eligible functions are compiled to the flat
    /// [`VmProgram`] and executed by the dispatch-loop VM ([`Self::run_vm`])
    /// instead of the recursive tree-walker, so the bytecode tier is distinctly
    /// faster than the IR tier. The IR tier leaves it `false` and always
    /// tree-walks. Both produce identical results (backend parity).
    pub(crate) use_vm: bool,
    /// Per-function compiled `VmProgram` cache, keyed by the function's identity
    /// (its address in the borrowed module — stable for the run). It must NOT be
    /// keyed by name: trait-method impls share a method name across types
    /// (`Card::rank` and `Coin::rank` are both `rank`), so a name key would reuse
    /// one type's compiled body for another's. `Some` holds the program; a cached
    /// `None` records VM-ineligibility so it tree-walks without recompiling. Only
    /// populated when `use_vm` is set.
    vm_cache: HashMap<*const IrFunction, Option<Rc<VmProgram>>>,
}

impl<'a> IrRuntime<'a> {
    /// Build an interpreter over the borrowed module `module` while retaining an
    /// owned `Arc<IrModule>` (`module_arc`) that points at the same data, used
    /// only to hand a share to detached `spawn`ed threads. The caller passes both
    /// handles (e.g. `IrRuntime::new(&arc, Arc::clone(&arc))`).
    pub(crate) fn new(
        module: &'a IrModule,
        module_arc: Arc<IrModule>,
    ) -> Result<Self, RuntimeError> {
        let functions = module
            .functions
            .iter()
            .map(|function| (function.name.as_str(), function))
            .collect::<HashMap<_, _>>();

        if !functions.contains_key("main") {
            return Err(RuntimeError::new("L0422", "missing `main` function"));
        }

        let structs = module
            .structs
            .iter()
            .map(|declaration| {
                (
                    declaration.name.as_str(),
                    declaration
                        .fields
                        .iter()
                        .map(|(field, _)| field.clone())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();

        let mut variants = HashMap::new();
        // Built-in `option`/`result` generic-enum variants, registered like user
        // variants so construction and `match` reuse the same `Value::Enum` path.
        variants.insert("some", "option");
        variants.insert("none", "option");
        variants.insert("ok", "result");
        variants.insert("err", "result");
        // Compiler-provided `MemoryOrder` enum, registered like `option`/`result`
        // so bare `acquire`/`seq_cst`/… build the ordering `Value::Enum` consumed
        // by the ordering-taking atomic builtins and `fence`.
        for variant in MEMORY_ORDER_VARIANTS {
            variants.insert(variant, "MemoryOrder");
        }
        for declaration in &module.enums {
            for variant in &declaration.variants {
                variants.insert(variant.name.as_str(), declaration.name.as_str());
            }
        }

        // Build the trait-method dispatch table from all impls in the module.
        let mut impl_methods = HashMap::new();
        for impl_method in &module.impls {
            impl_methods.insert(
                (
                    impl_method.type_name.clone(),
                    impl_method.method_name.clone(),
                ),
                &impl_method.function,
            );
        }
        let trait_method_names = module.trait_methods.iter().cloned().collect();
        let async_functions = module.async_functions.iter().cloned().collect();
        let extern_functions = module.extern_functions.iter().cloned().collect();

        let closures = module
            .closures
            .iter()
            .map(|def| (def.id, def))
            .collect::<HashMap<_, _>>();

        Ok(Self {
            module,
            module_arc,
            functions,
            program_args: Vec::new(),
            structs,
            variants,
            heap: Vec::new(),
            refcounts: HashMap::new(),
            sockets: Vec::new(),
            processes: Vec::new(),
            call_stack: Vec::new(),
            impl_methods,
            trait_method_names,
            async_functions,
            extern_functions,
            closures,
            env_pool: Vec::new(),
            use_vm: false,
            vm_cache: HashMap::new(),
        })
    }

    /// Spawn an `async fn` call on a new OS thread that owns a share of the
    /// module (an `Arc<IrModule>` clone) and builds its own interpreter, then
    /// return a `Value::Future` handle so `await` retrieves the produced value.
    /// The already-evaluated argument values are `Send`; heaps are per-thread.
    fn spawn_async(&self, name: &str, args: Vec<Value>) -> Value {
        let arc = Arc::clone(&self.module_arc);
        let func_name = name.to_string();
        let handle = std::thread::spawn(move || {
            let mut runtime = IrRuntime::new(&arc, Arc::clone(&arc))?;
            runtime.call_function(&func_name, args)
        });
        Value::Future(Future {
            handle: Arc::new(std::sync::Mutex::new(Some(handle))),
        })
    }

    /// Safety gate for the move-on-functional-update fast path (IR twin of the
    /// AST runtime's method): true when a call to `name` is a plain builtin (or
    /// infallible enum/struct constructor) that cannot raise a *catchable*
    /// `L0420` user error, so moving the consumed argument out can never leave a
    /// moved-out placeholder observable by a surrounding `catch`. Excludes
    /// closure/func-valued variables, `extern`/`async` functions, trait methods,
    /// user functions, and `assert` (the one builtin that raises `L0420`).
    fn is_move_safe_builtin(&self, name: &str, env: &Env) -> bool {
        if matches!(
            env.get_ref(name),
            Some(Value::Closure(_)) | Some(Value::Func(_))
        ) {
            return false;
        }
        name != "assert"
            && !self.extern_functions.contains(name)
            && !self.async_functions.contains(name)
            && !self.trait_method_names.contains(name)
            && !self.functions.contains_key(name)
    }

    /// The move-on-functional-update fast path for the `x = f(x, …)` (CALL) and
    /// `x = x <binop> e` / `x = e <binop> x` (BINARY) accumulation idioms (IR twin
    /// of the AST runtime's method). When the target `name` appears exactly once —
    /// as a bare call argument, or as exactly one bare operand of a binary op —
    /// and nowhere else in the RHS, and `name` is a local, this evaluates the RHS
    /// with that occurrence **moved** out of the environment instead of cloned and
    /// returns `Some(result)`. `None` means the caller must fall back to the
    /// ordinary clone path. The binary form makes `s = s + piece` loops O(n) by
    /// letting `eval_binary` reuse the moved left operand's buffer. See the AST
    /// runtime for the full safety argument; the two implementations are kept in
    /// lockstep.
    fn try_move_functional_update(
        &mut self,
        name: &str,
        rhs: &IrExpr,
        env: &mut Env,
        require_innermost: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        match &rhs.kind {
            IrExprKind::Call { name: callee, args } => {
                self.try_move_call_update(name, callee, args, env, require_innermost)
            }
            IrExprKind::Binary { op, left, right } => {
                self.try_move_binary_update(name, *op, left, right, env, require_innermost)
            }
            _ => Ok(None),
        }
    }

    /// `x = f(x, …)` arm of [`Self::try_move_functional_update`].
    fn try_move_call_update(
        &mut self,
        name: &str,
        callee: &str,
        args: &[IrExpr],
        env: &mut Env,
        require_innermost: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        if callee == name {
            return Ok(None);
        }
        if !self.is_move_safe_builtin(callee, env) {
            return Ok(None);
        }
        // For a `let` re-binding the binding must be innermost (`let` shadows into
        // the innermost scope); for a plain reassignment any-scope is fine (moved
        // from, and written back to, the nearest binding).
        let bound = if require_innermost {
            env.innermost_has(name)
        } else {
            env.is_bound(name)
        };
        if !bound {
            return Ok(None);
        }
        let mut target_idx: Option<usize> = None;
        for (i, arg) in args.iter().enumerate() {
            let is_bare = bare_local_name(&arg.kind) == Some(name);
            if is_bare && target_idx.is_none() {
                target_idx = Some(i);
            } else if expr_mentions_var(arg, name) {
                return Ok(None);
            }
        }
        let Some(target_idx) = target_idx else {
            return Ok(None);
        };
        // Evaluate every *other* argument first, in source order, so a failure
        // there leaves `name` intact and the env consistent.
        let mut evaluated: Vec<Option<Value>> = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            if i == target_idx {
                evaluated.push(None);
            } else {
                evaluated.push(Some(self.eval_expr(arg, env)?));
            }
        }
        let moved = env
            .move_out_nearest(name)
            .expect("target verified bound as a local");
        let mut moved = Some(moved);
        let values: Vec<Value> = evaluated
            .into_iter()
            .enumerate()
            .map(|(i, slot)| {
                if i == target_idx {
                    moved.take().expect("single target slot")
                } else {
                    slot.expect("non-target slots are evaluated")
                }
            })
            .collect();
        Ok(Some(self.call_function(callee, values)?))
    }

    /// `x = x <binop> e` / `x = e <binop> x` arm of
    /// [`Self::try_move_functional_update`]. Fires when exactly one operand is the
    /// bare variable `name` and `name` appears nowhere else in either operand.
    /// Short-circuit `and`/`or` are excluded (they do not route through
    /// `eval_binary` and their right operand is conditional).
    fn try_move_binary_update(
        &mut self,
        name: &str,
        op: BinaryOp,
        left: &IrExpr,
        right: &IrExpr,
        env: &mut Env,
        require_innermost: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            return Ok(None);
        }
        let bound = if require_innermost {
            env.innermost_has(name)
        } else {
            env.is_bound(name)
        };
        if !bound {
            return Ok(None);
        }
        let left_bare = bare_local_name(&left.kind) == Some(name);
        let right_bare = bare_local_name(&right.kind) == Some(name);
        let target_is_left = if left_bare && !expr_mentions_var(right, name) {
            true
        } else if right_bare && !expr_mentions_var(left, name) {
            false
        } else {
            return Ok(None);
        };
        // Evaluate the non-target operand *before* moving the target.
        let other = if target_is_left {
            self.eval_expr(right, env)?
        } else {
            self.eval_expr(left, env)?
        };
        let moved = env
            .move_out_nearest(name)
            .expect("target verified bound as a local");
        let (l, r) = if target_is_left {
            (moved, other)
        } else {
            (other, moved)
        };
        Ok(Some(self.eval_binary(l, op, r)?))
    }

    /// Dispatch a call to an already-resolved top-level function name: reject an
    /// `extern fn` (C-ABI, native-only) with `L0423`, spawn an `async fn` on its
    /// own OS thread yielding a `Future`, or invoke the function / builtin /
    /// constructor synchronously through [`Self::call_function`].
    fn dispatch_named_call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        if self.extern_functions.contains(name) {
            return Err(extern_call_error(name));
        }
        if self.async_functions.contains(name) {
            Ok(self.spawn_async(name, args))
        } else {
            self.call_function(name, args)
        }
    }

    pub(crate) fn call_function(
        &mut self,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        // Trait-method dispatch: select the impl by the receiver's runtime type.
        if self.trait_method_names.contains(name) {
            let receiver_type = args.first().map(value_type_name).ok_or_else(|| {
                RuntimeError::new(
                    "L0401",
                    format!("trait method `{name}` called without a receiver"),
                )
            })?;
            let method = *self
                .impl_methods
                .get(&(receiver_type.clone(), name.to_string()))
                .ok_or_else(|| {
                    RuntimeError::new(
                        "L0401",
                        format!("type `{receiver_type}` does not implement trait method `{name}`"),
                    )
                })?;
            return self.invoke_function(method, args);
        }
        if let Some(enum_name) = self.variants.get(name) {
            return Ok(Value::Enum(Box::new(EnumValue {
                enum_name: enum_name.to_string(),
                variant: name.to_string(),
                payload: args,
            })));
        }
        if let Some(field_names) = self.structs.get(name) {
            return Ok(Value::Struct(Box::new(StructValue {
                name: name.to_string(),
                fields: field_names.iter().cloned().zip(args).collect(),
            })));
        }
        match name {
            "alloc" => self.builtin_alloc(args),
            "load" => self.builtin_load(args),
            "store" => self.builtin_store(args),
            "dealloc" => self.builtin_dealloc(args),
            "read_file" => self.builtin_read_file(args),
            "write_file" => self.builtin_write_file(args),
            "append_file" => self.builtin_append_file(args),
            "file_exists" => self.builtin_file_exists(args),
            "read_lines" => self.builtin_read_lines(args),
            "read_bytes" => self.builtin_read_bytes(args),
            "write_bytes" => self.builtin_write_bytes(args),
            "file_size" => self.builtin_file_size(args),
            "is_file" => self.builtin_is_file(args),
            "is_dir" => self.builtin_is_dir(args),
            "list_dir" => self.builtin_list_dir(args),
            "make_dir" => self.builtin_make_dir(args),
            "remove_file" => self.builtin_remove_file(args),
            "remove_dir" => self.builtin_remove_dir(args),
            "sys_status" => self.builtin_sys_status(args),
            "sys_output" => self.builtin_sys_output(args),
            "print" => self.builtin_print("print", args, false),
            "println" => self.builtin_print("println", args, true),
            "warn" => self.builtin_warn(args),
            "wasm_log" => self.builtin_wasm_log(args),
            "console_log" => self.builtin_console_log(args),
            "dom_set_text" => self.builtin_dom_set_text(args),
            "flush" => self.builtin_flush(args),
            "mono_now" => Self::builtin_mono_now(args),
            "wall_now" => Self::builtin_wall_now(args),
            "sleep_millis" => Self::builtin_sleep_millis(args),
            "assert" => Self::builtin_assert(args),
            "to_string" => Self::builtin_to_string(args),
            "char_code" => Self::builtin_char_code(args),
            "char_from" => Self::builtin_char_from(args),
            "is_digit" => Self::builtin_is_digit(args),
            "is_alpha" => Self::builtin_is_alpha(args),
            "is_alnum" => Self::builtin_is_alnum(args),
            "is_whitespace" => Self::builtin_is_whitespace(args),
            "is_upper" => Self::builtin_is_upper(args),
            "is_lower" => Self::builtin_is_lower(args),
            "byte" => Self::builtin_byte(args),
            "byte_val" => Self::builtin_byte_val(args),
            "to_i8" => Self::builtin_to_int("to_i8", args, IntKind::I8),
            "to_u8" => Self::builtin_to_int("to_u8", args, IntKind::U8),
            "to_i16" => Self::builtin_to_int("to_i16", args, IntKind::I16),
            "to_i32" => Self::builtin_to_int("to_i32", args, IntKind::I32),
            "to_u16" => Self::builtin_to_int("to_u16", args, IntKind::U16),
            "to_u32" => Self::builtin_to_int("to_u32", args, IntKind::U32),
            "to_u64" => Self::builtin_to_int("to_u64", args, IntKind::U64),
            "to_isize" => Self::builtin_to_int("to_isize", args, IntKind::Isize),
            "to_usize" => Self::builtin_to_int("to_usize", args, IntKind::Usize),
            "to_i64" => Self::builtin_to_i64(args),
            "to_f32" => Self::builtin_to_f32(args),
            "to_f64" => Self::builtin_to_f64(args),
            "checked_add" => overflow_arith(name, args, ArithOp::Add, OverflowMode::Checked),
            "checked_sub" => overflow_arith(name, args, ArithOp::Sub, OverflowMode::Checked),
            "checked_mul" => overflow_arith(name, args, ArithOp::Mul, OverflowMode::Checked),
            "saturating_add" => overflow_arith(name, args, ArithOp::Add, OverflowMode::Saturating),
            "saturating_sub" => overflow_arith(name, args, ArithOp::Sub, OverflowMode::Saturating),
            "saturating_mul" => overflow_arith(name, args, ArithOp::Mul, OverflowMode::Saturating),
            "wrapping_add" => overflow_arith(name, args, ArithOp::Add, OverflowMode::Wrapping),
            "wrapping_sub" => overflow_arith(name, args, ArithOp::Sub, OverflowMode::Wrapping),
            "wrapping_mul" => overflow_arith(name, args, ArithOp::Mul, OverflowMode::Wrapping),
            "len" => Self::builtin_len(args),
            "array_fill" => Self::builtin_array_fill(args),
            "list_new" => Self::builtin_list_new(args),
            "push" => Self::builtin_push(args),
            "get" => Self::builtin_get(args),
            "set" => Self::builtin_set(args),
            "pop" => Self::builtin_pop(args),
            "list_index_of" => Self::builtin_list_index_of(args),
            "list_contains" => Self::builtin_list_contains(args),
            "reverse" => Self::builtin_reverse(args),
            "sort" => Self::builtin_sort(args),
            "sort_by" => self.builtin_sort_by(args),
            "concat" => Self::builtin_concat(args),
            "slice" => Self::builtin_slice(args),
            "list_map" => self.builtin_list_map(args),
            "list_filter" => self.builtin_list_filter(args),
            "list_reduce" => self.builtin_list_reduce(args),
            "map_new" => Self::builtin_map_new(args),
            "map_set" => Self::builtin_map_set(args),
            "map_get" => Self::builtin_map_get(args),
            "map_has" => Self::builtin_map_has(args),
            "map_len" => Self::builtin_map_len(args),
            "map_keys" => Self::builtin_map_keys(args),
            "map_values" => Self::builtin_map_values(args),
            "map_del" => Self::builtin_map_del(args),
            "substring" => Self::builtin_substring(args),
            "find" => Self::builtin_find(args),
            "contains" => Self::builtin_contains(args),
            "starts_with" => Self::builtin_starts_with(args),
            "ends_with" => Self::builtin_ends_with(args),
            "repeat" => Self::builtin_repeat(args),
            "split" => Self::builtin_split(args),
            // `words`/`count` yield to a user-defined function of the same name, so
            // adding these common stdlib names never breaks existing user code.
            "words" if !self.functions.contains_key("words") => Self::builtin_words(args),
            "count" if !self.functions.contains_key("count") => Self::builtin_count(args),
            "join" => Self::builtin_join(args),
            "trim" => Self::builtin_trim(args),
            "replace" => Self::builtin_replace(args),
            "upper" => Self::builtin_upper(args),
            "chars" => Self::builtin_chars(args),
            "string_from_chars" => Self::builtin_string_from_chars(args),
            "lower" => Self::builtin_lower(args),
            "to_bytes" => Self::builtin_to_bytes(args),
            "from_bytes" => Self::builtin_from_bytes(args),
            "byte_len" => Self::builtin_byte_len(args),
            "parse_i64" => Self::builtin_parse_i64(args),
            "parse_f64" => Self::builtin_parse_f64(args),
            "abs" => Self::builtin_abs(args),
            "min" => Self::builtin_min(args),
            "max" => Self::builtin_max(args),
            "clamp" => Self::builtin_clamp(args),
            "sign" => Self::builtin_sign(args),
            "gcd" => Self::builtin_gcd(args),
            "list_sum" => Self::builtin_list_sum(args),
            "list_min" => Self::builtin_list_min(args),
            "list_max" => Self::builtin_list_max(args),
            "pow" => Self::builtin_pow(args),
            "sqrt" => Self::builtin_sqrt(args),
            "sin" => Self::builtin_unary_f64("sin", args, f64::sin),
            "cos" => Self::builtin_unary_f64("cos", args, f64::cos),
            "tan" => Self::builtin_unary_f64("tan", args, f64::tan),
            "atan" => Self::builtin_unary_f64("atan", args, f64::atan),
            "exp" => Self::builtin_unary_f64("exp", args, f64::exp),
            "ln" => Self::builtin_unary_f64("ln", args, f64::ln),
            "log10" => Self::builtin_unary_f64("log10", args, f64::log10),
            "atan2" => Self::builtin_atan2(args),
            "rotate_left" => Self::builtin_rotate_left(args),
            "rotate_right" => Self::builtin_rotate_right(args),
            "count_ones" => Self::builtin_count_ones(args),
            "leading_zeros" => Self::builtin_leading_zeros(args),
            "trailing_zeros" => Self::builtin_trailing_zeros(args),
            "reverse_bytes" => Self::builtin_reverse_bytes(args),
            "floor" => Self::builtin_floor(args),
            "ceil" => Self::builtin_ceil(args),
            "round" => Self::builtin_round(args),
            "rc_new" => self.builtin_rc_new(args),
            "rc_clone" => self.builtin_rc_clone(args),
            "rc_release" => self.builtin_rc_release(args),
            "rc_get" | "ref_get" | "ptr_read" => self.builtin_ref_get(name, args),
            "rc_borrow" => self.builtin_rc_borrow(args),
            "ptr_write" => self.builtin_store(args),
            "size_of" => Self::builtin_size_of(args),
            "align_of" => Self::builtin_align_of(args),
            "offset_of" => Self::builtin_offset_of(args),
            "ptr_to_int" => Self::builtin_ptr_to_int(args),
            "int_to_ptr" => Self::builtin_int_to_ptr(args),
            // Volatile raw-memory access behaves exactly like `load`/`store` on
            // the interpreters' single-threaded abstract heap; the no-elision /
            // no-reordering guarantee is a native-codegen concern.
            "volatile_load" => self.builtin_load(args),
            "volatile_store" => self.builtin_store(args),
            "env" => Self::builtin_env(args),
            "os_random" => Self::builtin_os_random(args),
            "args" => self.builtin_args(args),
            "parallel_map" => self.builtin_parallel_map(args),
            "chan_new" => Self::builtin_chan_new(args),
            "send" => Self::builtin_send(args),
            "recv" => Self::builtin_recv(args),
            "try_recv" => Self::builtin_try_recv(args),
            "spawn" => self.builtin_spawn(args),
            "task_join" => Self::builtin_task_join(args),
            "mutex_new" => Self::builtin_mutex_new(args),
            "mutex_get" => Self::builtin_mutex_get(args),
            "mutex_set" => Self::builtin_mutex_set(args),
            "mutex_add" => Self::builtin_mutex_add(args),
            "atomic_new" => Self::builtin_atomic_new(args),
            "atomic_load" => Self::builtin_atomic_load(args),
            "atomic_store" => Self::builtin_atomic_store(args),
            "atomic_swap" => Self::builtin_atomic_swap(args),
            "atomic_cas" => Self::builtin_atomic_cas(args),
            "atomic_add" => Self::builtin_atomic_add(args),
            "atomic_sub" => Self::builtin_atomic_sub(args),
            "atomic_and" => Self::builtin_atomic_and(args),
            "atomic_or" => Self::builtin_atomic_or(args),
            "atomic_xor" => Self::builtin_atomic_xor(args),
            "atomic_load_ordered" => builtin_atomic_load_ordered(args),
            "atomic_store_ordered" => builtin_atomic_store_ordered(args),
            "atomic_swap_ordered" => builtin_atomic_swap_ordered(args),
            "atomic_cas_ordered" => builtin_atomic_cas_ordered(args),
            "atomic_add_ordered" => builtin_atomic_add_ordered(args),
            "atomic_sub_ordered" => builtin_atomic_sub_ordered(args),
            "atomic_and_ordered" => builtin_atomic_and_ordered(args),
            "atomic_or_ordered" => builtin_atomic_or_ordered(args),
            "atomic_xor_ordered" => builtin_atomic_xor_ordered(args),
            "fence" => builtin_fence(args),
            "tcp_connect" => self.builtin_tcp_connect(args),
            "tcp_listen" => self.builtin_tcp_listen(args),
            "tcp_accept" => self.builtin_tcp_accept(args),
            "tcp_accept_nb" => self.builtin_tcp_accept_nb(args),
            "tcp_read" => self.builtin_tcp_read(args),
            "tcp_read_nb" => self.builtin_tcp_read_nb(args),
            "tcp_write" => self.builtin_tcp_write(args),
            "tcp_shutdown" => self.builtin_tcp_shutdown(args),
            "tcp_close" => self.builtin_socket_close(args),
            "set_nonblocking" => self.builtin_set_nonblocking(args),
            "udp_bind" => self.builtin_udp_bind(args),
            "udp_send_to" => self.builtin_udp_send_to(args),
            "udp_recv" => self.builtin_udp_recv(args),
            "udp_recv_nb" => self.builtin_udp_recv_nb(args),
            "http_get" => Self::builtin_http_get(args),
            "http_post" => Self::builtin_http_post(args),
            "proc_spawn" => self.builtin_proc_spawn(args),
            "proc_wait" => self.builtin_proc_wait(args),
            "proc_stdout" => self.builtin_proc_stdout(args),
            "proc_stderr" => self.builtin_proc_stderr(args),
            "proc_kill" => self.builtin_proc_kill(args),
            // A region-creation marker has no runtime effect in the current
            // analysis-only region model.
            "region_create" => Ok(Value::Void),
            _ => {
                let function = *self.functions.get(name).ok_or_else(|| {
                    RuntimeError::new("L0401", format!("unknown function `{name}`"))
                })?;
                self.invoke_function(function, args)
            }
        }
    }

    /// Execute a user function (or trait impl method) with the given argument
    /// values, threading the traceback and translating loop-control escape.
    fn invoke_function(
        &mut self,
        function: &'a IrFunction,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        if function.params.len() != args.len() {
            return Err(RuntimeError::new(
                "L0402",
                format!(
                    "function `{}` expects {} arguments but got {}",
                    function.name,
                    function.params.len(),
                    args.len()
                ),
            ));
        }

        // Bytecode tier: run the flat dispatch-loop VM for eligible functions
        // (identical results, no recursive tree-walk). Ineligible functions fall
        // through to the tree-walker below via a cached `None`.
        if self.use_vm
            && let Some(program) = self.vm_program_for(function)
        {
            self.call_stack.push(CallFrame {
                function: function.name.as_str(),
                span: Some(function.span),
            });
            let result = self.run_vm(&program, args);
            return match result {
                Err(error) => {
                    let error = if error.traceback.is_empty() {
                        error.with_traceback(self.build_traceback())
                    } else {
                        error
                    };
                    self.call_stack.pop();
                    Err(error)
                }
                Ok(value) => {
                    self.call_stack.pop();
                    Ok(value)
                }
            };
        }

        // Borrow a reset environment from the pool (or make a fresh one) instead
        // of allocating per call; returned to the pool on the normal exit below.
        let mut env = match self.env_pool.pop() {
            Some(mut env) => {
                env.reset();
                env
            }
            None => Env::default(),
        };
        for (param, value) in function.params.iter().zip(args) {
            env.define(param.name.clone(), value);
        }

        self.call_stack.push(CallFrame {
            function: function.name.as_str(),
            span: Some(function.span),
        });
        let result = self.eval_block(&function.body, &mut env);

        // Attach the traceback lazily. `with_traceback` records only the first
        // (innermost) stack, so eagerly cloning `call_stack` on every successful
        // call — and on every frame an error merely passes through — is pure
        // waste that grows with recursion depth. Clone it only when this frame is
        // the one first recording a traceback, while the frame is still on the
        // stack so it is included.
        let control = match result {
            Err(error) => {
                let error = if error.traceback.is_empty() {
                    error.with_traceback(self.build_traceback())
                } else {
                    error
                };
                self.call_stack.pop();
                return Err(error);
            }
            Ok(control) => {
                self.call_stack.pop();
                control
            }
        };
        // Normal exit: return the environment to the pool; error paths drop theirs.
        self.env_pool.push(env);

        match control {
            Control::Return(value) | Control::Value(value) => Ok(value),
            Control::Break | Control::Continue => Err(RuntimeError::new(
                "L0410",
                "loop control escaped function body",
            )),
        }
    }

    /// Fetch (compiling on first use) the flat [`VmProgram`] for `function`, or
    /// `None` if the function is VM-ineligible (a cached `None` avoids recompiling).
    fn vm_program_for(&mut self, function: &IrFunction) -> Option<Rc<VmProgram>> {
        let key = std::ptr::from_ref(function);
        if let Some(cached) = self.vm_cache.get(&key) {
            return cached.clone();
        }
        let compiled = compile_function_to_vm(function).map(Rc::new);
        self.vm_cache.insert(key, compiled.clone());
        compiled
    }

    /// Execute a compiled [`VmProgram`] with a flat operand stack and a flat local
    /// frame (slots assigned at compile time, no scope stack, no name scan). This
    /// is a single `loop { match }` dispatch instead of the recursive tree-walk;
    /// every actual operation (arithmetic, calls, indexing, field access) reuses
    /// the exact same `Value` helpers the tree-walker uses, so results are
    /// identical to the other tiers — only the control-flow dispatch differs.
    fn run_vm(&mut self, program: &VmProgram, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let mut frame: Vec<Value> = args;
        frame.resize(program.frame_size, Value::Void);
        let mut stack: Vec<Value> = Vec::with_capacity(8);
        let mut pc = 0usize;
        loop {
            // Annotate any op failure with the op's source span (and function +
            // traceback), exactly as the tree-walker annotates each node — so
            // runtime errors are byte-identical across tiers.
            match self.vm_exec(&program.ops[pc], &mut stack, &mut frame) {
                Ok(VmStep::Next) => pc += 1,
                Ok(VmStep::Jump(target)) => pc = target,
                Ok(VmStep::Return(value)) => return Ok(value),
                Err(error) => return Err(self.annotate_error(error, program.spans[pc])),
            }
        }
    }

    /// Execute a single VM op against the operand `stack` and local `frame`,
    /// returning the control outcome. Every actual operation reuses the exact
    /// `Value` helper the tree-walker uses, so results match tier-for-tier.
    fn vm_exec(
        &mut self,
        op: &VmOp,
        stack: &mut Vec<Value>,
        frame: &mut [Value],
    ) -> Result<VmStep, RuntimeError> {
        match op {
            VmOp::PushConst(value) => stack.push(value.clone()),
            VmOp::PushVoid => stack.push(Value::Void),
            VmOp::LoadLocal(slot) => stack.push(frame[*slot].clone()),
            VmOp::StoreLocal(slot) => {
                frame[*slot] = stack.pop().expect("vm: store underflow");
            }
            VmOp::Binary(op) => {
                let right = stack.pop().expect("vm: binary underflow");
                let left = stack.pop().expect("vm: binary underflow");
                stack.push(self.eval_binary(left, *op, right)?);
            }
            VmOp::Unary(op) => {
                let value = stack.pop().expect("vm: unary underflow");
                stack.push(eval_unary_value(*op, value)?);
            }
            VmOp::Index => {
                let index = stack.pop().expect("vm: index underflow").as_i64()?;
                let target = stack.pop().expect("vm: index underflow");
                stack.push(index_into(&target, index)?);
            }
            VmOp::IndexLocal(slot) => {
                let index = stack.pop().expect("vm: index underflow").as_i64()?;
                stack.push(index_into(&frame[*slot], index)?);
            }
            VmOp::Field(name) => {
                let target = stack.pop().expect("vm: field underflow");
                stack.push(field_of(&target, name)?);
            }
            VmOp::FieldLocal(slot, name) => {
                stack.push(field_of(&frame[*slot], name)?);
            }
            VmOp::Call(name, argc) => {
                let at = stack.len() - argc;
                let call_args = stack.split_off(at);
                stack.push(self.dispatch_named_call(name, call_args)?);
            }
            VmOp::MakeArray(count) => {
                let at = stack.len() - count;
                let elements = stack.split_off(at);
                stack.push(Value::Array(elements.into()));
            }
            VmOp::Jump(target) => return Ok(VmStep::Jump(*target)),
            VmOp::JumpIfFalse(target) => {
                if !stack.pop().expect("vm: jz underflow").as_bool()? {
                    return Ok(VmStep::Jump(*target));
                }
            }
            VmOp::JumpIfTrue(target) => {
                if stack.pop().expect("vm: jnz underflow").as_bool()? {
                    return Ok(VmStep::Jump(*target));
                }
            }
            VmOp::Pop => {
                stack.pop();
            }
            VmOp::CheckStepNonzero(slot) => {
                if frame[*slot].as_i64()? == 0 {
                    return Err(RuntimeError::new("L0411", "for loop step cannot be zero"));
                }
            }
            VmOp::ForCheck { var, end, step } => {
                let i = frame[*var].as_i64()?;
                let end = frame[*end].as_i64()?;
                let step = frame[*step].as_i64()?;
                let running = if step > 0 { i <= end } else { i >= end };
                stack.push(Value::Bool(running));
            }
            VmOp::ForStep { var, step } => {
                let i = frame[*var].as_i64()?;
                let step = frame[*step].as_i64()?;
                frame[*var] = Value::I64(i.wrapping_add(step));
            }
            VmOp::Return => return Ok(VmStep::Return(stack.pop().unwrap_or(Value::Void))),
        }
        Ok(VmStep::Next)
    }

    /// Invoke a closure value: look its body up in the id-keyed closure table,
    /// bind the captured snapshot first and then the parameters (parameters shadow
    /// captures), evaluate the single-expression body, and return the value.
    /// Mirrors the AST runtime's `invoke_closure` one-to-one for backend parity.
    fn invoke_closure(
        &mut self,
        closure: &Closure,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let def = *self.closures.get(&closure.id).ok_or_else(|| {
            RuntimeError::new(
                "L0402",
                format!("closure #{} has no registered body", closure.id),
            )
        })?;
        if def.params.len() != args.len() {
            return Err(RuntimeError::new(
                "L0402",
                format!(
                    "closure expects {} arguments but got {}",
                    def.params.len(),
                    args.len()
                ),
            ));
        }
        let mut env = Env::default();
        for (name, value) in &closure.captured {
            env.define(name.clone(), value.clone());
        }
        for (name, value) in def.params.iter().zip(args) {
            env.define(name.clone(), value);
        }
        self.eval_expr(&def.body, &env)
    }

    fn eval_block(
        &mut self,
        statements: &[IrStmt],
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        let mut last = Value::Void;

        for statement in statements {
            match self.eval_statement(statement, env)? {
                Control::Return(value) => return Ok(Control::Return(value)),
                Control::Break => return Ok(Control::Break),
                Control::Continue => return Ok(Control::Continue),
                Control::Value(value) => last = value,
            }
        }

        Ok(Control::Value(last))
    }

    fn eval_statement(
        &mut self,
        statement: &IrStmt,
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        let span = statement_span(statement);
        let result = match statement {
            IrStmt::Let { name, value, .. } => {
                // Move-on-functional-update fast path: `let x = f(x, …)` re-binding
                // an existing innermost local consumes it by move, not clone.
                let value = match self.try_move_functional_update(name, value, env, true)? {
                    Some(result) => result,
                    None => self.eval_expr(value, env)?,
                };
                env.define(name.clone(), value);
                Ok(Control::Value(Value::Void))
            }
            IrStmt::Assign {
                name,
                path,
                op,
                value,
                ..
            } => {
                if path.is_empty() && matches!(op, AssignOp::Replace) {
                    // Whole-variable reassignment `x = RHS`: try the
                    // move-on-functional-update fast path (`x = f(x, …)`) before
                    // falling back to the ordinary clone path.
                    let new = match self.try_move_functional_update(name, value, env, false)? {
                        Some(result) => result,
                        None => self.eval_expr(value, env)?,
                    };
                    env.assign(name, new)?;
                } else {
                    let rhs = self.eval_expr(value, env)?;
                    if path.is_empty() {
                        let new = apply_compound(env.get(name)?, op, rhs)?;
                        env.assign(name, new)?;
                    } else {
                        // Mutate the element/field in place instead of cloning the
                        // whole array/struct, mutating a copy, and writing it back
                        // (O(len) per write, O(len^2) in a write loop).
                        let resolved = self.resolve_places(path, env)?;
                        let root = env.get_mut(name).ok_or_else(|| {
                            RuntimeError::new("L0403", format!("unknown variable `{name}`"))
                        })?;
                        let new = match op {
                            AssignOp::Replace => rhs,
                            _ => apply_compound(get_place(root, &resolved)?, op, rhs)?,
                        };
                        set_place(root, &resolved, new)?;
                    }
                }
                Ok(Control::Value(Value::Void))
            }
            IrStmt::Return(expr) => {
                let value = expr
                    .as_ref()
                    .map(|expr| self.eval_expr(expr, env))
                    .unwrap_or(Ok(Value::Void))?;
                Ok(Control::Return(value))
            }
            IrStmt::Break(_) => Ok(Control::Break),
            IrStmt::Continue(_) => Ok(Control::Continue),
            IrStmt::Expr(expr) => self.eval_expr(expr, env).map(Control::Value),
            IrStmt::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    let condition = self.eval_expr(&branch.condition, env)?;
                    if condition.as_bool()? {
                        return self.eval_scoped_block(&branch.body, env);
                    }
                }
                self.eval_scoped_block(else_body, env)
            }
            IrStmt::While {
                condition, body, ..
            } => {
                while self.eval_expr(condition, env)?.as_bool()? {
                    match self.eval_scoped_block(body, env)? {
                        Control::Return(value) => return Ok(Control::Return(value)),
                        Control::Break => break,
                        Control::Continue | Control::Value(_) => {}
                    }
                }
                Ok(Control::Value(Value::Void))
            }
            IrStmt::For {
                name,
                start,
                end,
                step,
                body,
                ..
            } => {
                let mut current = self.eval_expr(start, env)?.as_i64()?;
                let end = self.eval_expr(end, env)?.as_i64()?;
                let step = step
                    .as_ref()
                    .map(|expr| self.eval_expr(expr, env))
                    .unwrap_or(Ok(Value::I64(1)))?
                    .as_i64()?;
                if step == 0 {
                    return Err(RuntimeError::new("L0411", "for loop step cannot be zero"));
                }

                // Bind the loop variable once and update it in place each pass
                // rather than re-`define`ing it (which clones the name and
                // reallocates a scope every iteration — measured ~2x on for-loops).
                // The body still runs in a fresh scope so its `let`s are cleared.
                // The final `pop_scope` always runs so the stack stays balanced for
                // `try`/`catch`.
                env.push_scope();
                env.define(name.clone(), Value::I64(current));
                let outcome: Result<Control, RuntimeError> = loop {
                    let running = if step > 0 {
                        current <= end
                    } else {
                        current >= end
                    };
                    if !running {
                        break Ok(Control::Value(Value::Void));
                    }
                    env.set_loop_var(name, Value::I64(current));
                    env.push_scope();
                    let result = self.eval_block(body, env);
                    env.pop_scope();

                    match result {
                        Ok(Control::Return(value)) => break Ok(Control::Return(value)),
                        Ok(Control::Break) => break Ok(Control::Value(Value::Void)),
                        Ok(Control::Continue) | Ok(Control::Value(_)) => {}
                        Err(error) => break Err(error),
                    }

                    current += step;
                };
                env.pop_scope();
                outcome
            }
            IrStmt::Loop { body, .. } => {
                loop {
                    match self.eval_scoped_block(body, env)? {
                        Control::Return(value) => return Ok(Control::Return(value)),
                        Control::Break => break,
                        Control::Continue | Control::Value(_) => {}
                    }
                }
                Ok(Control::Value(Value::Void))
            }
            // Inline assembly cannot run on the IR interpreter (raw machine code
            // requires native codegen + linking); reject it with `L0425`.
            IrStmt::Asm { .. } => Err(asm_interpreter_error()),
            IrStmt::Throw { value, .. } => {
                let message = self.eval_expr(value, env)?.as_string()?;
                Err(RuntimeError::new("L0420", message))
            }
            IrStmt::Try {
                body,
                catch_name,
                catch_body,
                ..
            } => match self.eval_scoped_block(body, env) {
                Err(error) if error.code == "L0420" => {
                    env.push_scope();
                    env.define(
                        catch_name.clone(),
                        Value::String((error.message.clone()).into()),
                    );
                    let result = self.eval_block(catch_body, env);
                    env.pop_scope();
                    result
                }
                other => other,
            },
            IrStmt::Match {
                scrutinee, arms, ..
            } => self.eval_match(scrutinee, arms, env),
        };
        result.map_err(|error| self.annotate_error(error, span))
    }

    fn eval_scoped_block(
        &mut self,
        statements: &[IrStmt],
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        env.push_scope();
        let result = self.eval_block(statements, env);
        env.pop_scope();
        result
    }

    /// Evaluate an IR `match` identically to the AST runtime: select the arm
    /// whose variant matches the scrutinee's enum value (or the `_` wildcard),
    /// bind payloads to arm-scoped locals, and evaluate the arm block.
    fn eval_match(
        &mut self,
        scrutinee: &IrExpr,
        arms: &[IrMatchArm],
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        let value = self.eval_expr(scrutinee, env)?;
        let Value::Enum(e) = value else {
            return Err(RuntimeError::new(
                "L0383",
                "match scrutinee did not evaluate to an enum value",
            ));
        };
        let EnumValue {
            variant, payload, ..
        } = *e;
        for arm in arms {
            match &arm.pattern {
                IrMatchPattern::Wildcard => {
                    return self.eval_scoped_block(&arm.body, env);
                }
                IrMatchPattern::Variant { name, bindings } if name == &variant => {
                    env.push_scope();
                    for (binding, value) in bindings.iter().zip(payload.iter()) {
                        env.define(binding.clone(), value.clone());
                    }
                    let result = self.eval_block(&arm.body, env);
                    env.pop_scope();
                    return result;
                }
                IrMatchPattern::Variant { .. } => {}
            }
        }
        Err(RuntimeError::new(
            "L0384",
            format!("no match arm covered variant `{variant}`"),
        ))
    }

    fn resolve_places(
        &mut self,
        path: &[IrPlace],
        env: &Env,
    ) -> Result<Vec<ResolvedPlace>, RuntimeError> {
        path.iter()
            .map(|place| match place {
                IrPlace::Field(field) => Ok(ResolvedPlace::Field(field.clone())),
                IrPlace::Index(expr) => {
                    Ok(ResolvedPlace::Index(self.eval_expr(expr, env)?.as_i64()?))
                }
            })
            .collect()
    }

    /// Resolve a bare name to a value: a local binding (name scan), else a known
    /// unit enum variant, else a first-class function value. Shared by the
    /// `Variable` and (on a slot miss) `Local` evaluation arms.
    fn eval_variable_name(&self, name: &str, env: &Env) -> Result<Value, RuntimeError> {
        match env.get(name) {
            Ok(value) => Ok(value),
            Err(error) => {
                if let Some(enum_name) = self.variants.get(name) {
                    Ok(Value::Enum(Box::new(EnumValue {
                        enum_name: enum_name.to_string(),
                        variant: name.to_string(),
                        payload: Vec::new(),
                    })))
                } else if self.functions.contains_key(name) {
                    Ok(Value::Func((name.to_string()).into()))
                } else {
                    Err(error)
                }
            }
        }
    }

    fn eval_expr(&mut self, expr: &IrExpr, env: &Env) -> Result<Value, RuntimeError> {
        let result = match &expr.kind {
            IrExprKind::Integer(value) => Ok(Value::I64(*value)),
            IrExprKind::Float(value) => Ok(Value::F64(*value)),
            IrExprKind::Bool(value) => Ok(Value::Bool(*value)),
            IrExprKind::String(value) => Ok(Value::String((value.clone()).into())),
            IrExprKind::Char(value) => Ok(Value::Char(*value)),
            IrExprKind::Array(values) => values
                .iter()
                .map(|value| self.eval_expr(value, env))
                .collect::<Result<Vec<_>, _>>()
                .map(|v| Value::Array(v.into())),
            // A slot-resolved local read: index the binding directly (no name
            // scan). The lookup is validated, so a miss (resolver/runtime scope
            // divergence, or a name that is really an enum variant / function
            // rather than a local) falls back to the exact `Variable` path.
            IrExprKind::Local { name, packed } => match env.get_slot(*packed, name) {
                Some(value) => Ok(value.clone()),
                None => self.eval_variable_name(name, env),
            },
            IrExprKind::Variable(name) => self.eval_variable_name(name, env),
            IrExprKind::Index { target, index } => {
                // Fast path: a bare-variable target (name-scanned or slot-resolved)
                // is borrowed (clone only the element), so `a[i]` does not clone the
                // whole array/string every access (which is O(len) per read).
                if bare_local_name(&target.kind).is_some() {
                    let idx = self.eval_expr(index, env)?.as_i64()?;
                    if let Some(container) = bare_local_ref(&target.kind, env) {
                        return index_into(container, idx);
                    }
                    let owned = self.eval_expr(target, env)?;
                    return index_into(&owned, idx);
                }
                let target = self.eval_expr(target, env)?;
                let index = self.eval_expr(index, env)?.as_i64()?;
                index_into(&target, index)
            }
            IrExprKind::Field { target, field } => {
                // Fast path: borrow a bare-variable struct (name-scanned or
                // slot-resolved) and clone only the field read, instead of cloning
                // the whole struct on every `s.field`.
                if let Some(Value::Struct(s)) = bare_local_ref(&target.kind, env) {
                    return s
                        .fields
                        .iter()
                        .find(|(n, _)| n == field)
                        .map(|(_, value)| value.clone())
                        .ok_or_else(|| RuntimeError::new("L0371", format!("no field `{field}`")));
                }
                match self.eval_expr(target, env)? {
                    Value::Struct(s) => s
                        .fields
                        .into_iter()
                        .find(|(name, _)| name == field)
                        .map(|(_, value)| value)
                        .ok_or_else(|| RuntimeError::new("L0371", format!("no field `{field}`"))),
                    _ => Err(RuntimeError::new(
                        "L0371",
                        format!("cannot access field `{field}` on non-struct value"),
                    )),
                }
            }
            IrExprKind::Unary { op, expr } => {
                let value = self.eval_expr(expr, env)?;
                match op {
                    UnaryOp::Not => Ok(Value::Bool(!value.as_bool()?)),
                    // Bitwise NOT (one's complement); a fixed-width integer is
                    // re-normalized to its width.
                    UnaryOp::BitNot => match value {
                        Value::Int { value, ty } => Ok(Value::int(!value, ty)),
                        other => Ok(Value::I64(!other.as_i64()?)),
                    },
                    // Arithmetic negation, preserving the operand's numeric type.
                    UnaryOp::Negate => match value {
                        Value::Int { value, ty } => Ok(Value::int(value.wrapping_neg(), ty)),
                        Value::F64(f) => Ok(Value::F64(-f)),
                        Value::F32(f) => Ok(Value::F32(-f)),
                        other => Ok(Value::I64(other.as_i64()?.wrapping_neg())),
                    },
                }
            }
            IrExprKind::Binary { left, op, right } => {
                if *op == BinaryOp::And {
                    let left = self.eval_expr(left, env)?.as_bool()?;
                    if !left {
                        return Ok(Value::Bool(false));
                    }
                    let right = self.eval_expr(right, env)?.as_bool()?;
                    return Ok(Value::Bool(right));
                }
                if *op == BinaryOp::Or {
                    let left = self.eval_expr(left, env)?.as_bool()?;
                    if left {
                        return Ok(Value::Bool(true));
                    }
                    let right = self.eval_expr(right, env)?.as_bool()?;
                    return Ok(Value::Bool(right));
                }
                let left = self.eval_expr(left, env)?;
                let right = self.eval_expr(right, env)?;
                self.eval_binary(left, *op, right)
            }
            IrExprKind::Call { name, args } => {
                let values = args
                    .iter()
                    .map(|arg| self.eval_expr(arg, env))
                    .collect::<Result<Vec<_>, _>>()?;
                // Resolve the call target with a single borrowing lookup. A call
                // name bound to a closure value invokes that closure (binding its
                // captured snapshot then the arguments); a name bound to a
                // function value dispatches through it; otherwise `name` is a plain
                // top-level function / builtin / constructor. Using `get_ref`
                // keeps the common case — an ordinary top-level call, where `name`
                // is not a local — free of the clone and the discarded "unknown
                // variable" error a bare `env.get` allocates on every such call.
                let target: &str = match env.get_ref(name) {
                    Some(Value::Closure(closure)) => {
                        let closure = closure.clone();
                        return self.invoke_closure(&closure, values);
                    }
                    Some(Value::Func(func)) => {
                        let func = func.clone();
                        return self.dispatch_named_call(&func, values);
                    }
                    _ => name,
                };
                self.dispatch_named_call(target, values)
            }
            IrExprKind::Await { expr } => {
                let value = self.eval_expr(expr, env)?;
                let future = expect_future("await", value)?;
                await_future(&future)
            }
            // Evaluating a closure literal snapshots the current environment's
            // in-scope locals by value and yields a `Value::Closure` carrying the
            // literal's id plus that snapshot. The body lives in `self.closures`
            // (keyed by id) and is looked up at invocation time, mirroring the AST
            // runtime exactly for backend parity.
            IrExprKind::Closure { id } => Ok(Value::Closure(Box::new(Closure {
                id: *id,
                captured: env.snapshot_locals(),
            }))),
        };
        result.map_err(|error| self.annotate_error(error, expr.span))
    }

    fn annotate_error(&self, error: RuntimeError, span: Span) -> RuntimeError {
        let error = error.with_span(span);
        match self.call_stack.last() {
            Some(frame) => error
                .with_function(frame.function.to_string())
                .with_traceback(self.build_traceback()),
            None => error,
        }
    }

    /// Materialize the active call stack as owned [`TraceFrame`]s for a
    /// `RuntimeError` — called only on the error path, so the per-frame name
    /// clone stays off the hot call path (the live `call_stack` borrows each
    /// name from the program).
    fn build_traceback(&self) -> Vec<TraceFrame> {
        self.call_stack
            .iter()
            .map(|frame| TraceFrame {
                function: frame.function.to_string(),
                span: frame.span,
            })
            .collect()
    }

    fn eval_binary(&self, left: Value, op: BinaryOp, right: Value) -> Result<Value, RuntimeError> {
        if let (Value::F64(l), Value::F64(r)) = (&left, &right) {
            let (l, r) = (*l, *r);
            return Ok(match op {
                BinaryOp::Add => Value::F64(l + r),
                BinaryOp::Subtract => Value::F64(l - r),
                BinaryOp::Multiply => Value::F64(l * r),
                BinaryOp::Divide => Value::F64(l / r),
                BinaryOp::Remainder => {
                    unreachable!("`%` requires integer operands (rejected by semantics)")
                }
                BinaryOp::Equal => Value::Bool(l == r),
                BinaryOp::NotEqual => Value::Bool(l != r),
                BinaryOp::Less => Value::Bool(l < r),
                BinaryOp::LessEqual => Value::Bool(l <= r),
                BinaryOp::Greater => Value::Bool(l > r),
                BinaryOp::GreaterEqual => Value::Bool(l >= r),
                BinaryOp::And | BinaryOp::Or => {
                    unreachable!("logical ops short-circuit in eval_expr")
                }
                BinaryOp::BitAnd
                | BinaryOp::BitOr
                | BinaryOp::BitXor
                | BinaryOp::Shl
                | BinaryOp::Shr => {
                    unreachable!("bitwise ops require i64 operands (rejected by semantics)")
                }
            });
        }
        // 32-bit float arithmetic/comparison, identical to the AST runtime; the
        // native f32 storage rounds each result to f32 precision.
        if let (Value::F32(l), Value::F32(r)) = (&left, &right) {
            let (l, r) = (*l, *r);
            return Ok(match op {
                BinaryOp::Add => Value::F32(l + r),
                BinaryOp::Subtract => Value::F32(l - r),
                BinaryOp::Multiply => Value::F32(l * r),
                BinaryOp::Divide => Value::F32(l / r),
                BinaryOp::Remainder => {
                    unreachable!("`%` requires integer operands (rejected by semantics)")
                }
                BinaryOp::Equal => Value::Bool(l == r),
                BinaryOp::NotEqual => Value::Bool(l != r),
                BinaryOp::Less => Value::Bool(l < r),
                BinaryOp::LessEqual => Value::Bool(l <= r),
                BinaryOp::Greater => Value::Bool(l > r),
                BinaryOp::GreaterEqual => Value::Bool(l >= r),
                BinaryOp::And | BinaryOp::Or => {
                    unreachable!("logical ops short-circuit in eval_expr")
                }
                BinaryOp::BitAnd
                | BinaryOp::BitOr
                | BinaryOp::BitXor
                | BinaryOp::Shl
                | BinaryOp::Shr => {
                    unreachable!("bitwise ops require i64 operands (rejected by semantics)")
                }
            });
        }
        // Fixed-width integer arithmetic/comparison, identical to the AST runtime:
        // same-tag operands, wrap-normalized result, plain `i64` ordering of the
        // normalized cells. Kept byte-for-byte in step with the other backends.
        if let (Value::Int { value: l, ty }, Value::Int { value: r, ty: rk }) = (&left, &right) {
            debug_assert_eq!(ty, rk, "mixed-width integer operands reached eval_binary");
            let (l, r, ty) = (*l, *r, *ty);
            return match op {
                BinaryOp::Add => Ok(Value::int(l.wrapping_add(r), ty)),
                BinaryOp::Subtract => Ok(Value::int(l.wrapping_sub(r), ty)),
                BinaryOp::Multiply => Ok(Value::int(l.wrapping_mul(r), ty)),
                BinaryOp::Divide => {
                    if r == 0 {
                        Err(RuntimeError::new("L0404", "division by zero"))
                    } else {
                        Ok(Value::int(int_div(l, r, ty), ty))
                    }
                }
                BinaryOp::Remainder => {
                    if r == 0 {
                        Err(RuntimeError::new("L0404", "remainder by zero"))
                    } else {
                        Ok(Value::int(int_rem(l, r, ty), ty))
                    }
                }
                BinaryOp::Equal => Ok(Value::Bool(l == r)),
                BinaryOp::NotEqual => Ok(Value::Bool(l != r)),
                BinaryOp::Less => Ok(Value::Bool(int_cmp(l, r, ty).is_lt())),
                BinaryOp::LessEqual => Ok(Value::Bool(int_cmp(l, r, ty).is_le())),
                BinaryOp::Greater => Ok(Value::Bool(int_cmp(l, r, ty).is_gt())),
                BinaryOp::GreaterEqual => Ok(Value::Bool(int_cmp(l, r, ty).is_ge())),
                // Bitwise ops mirror the AST runtime exactly.
                BinaryOp::BitAnd => Ok(Value::int(l & r, ty)),
                BinaryOp::BitOr => Ok(Value::int(l | r, ty)),
                BinaryOp::BitXor => Ok(Value::int(l ^ r, ty)),
                BinaryOp::Shl => Ok(Value::int(int_shl(l, r, ty), ty)),
                BinaryOp::Shr => Ok(Value::int(int_shr(l, r, ty), ty)),
                BinaryOp::And | BinaryOp::Or => {
                    unreachable!("logical ops short-circuit in eval_expr")
                }
            };
        }
        match op {
            BinaryOp::Add if matches!((&left, &right), (Value::String(_), Value::String(_))) => {
                // Reuse the left operand's heap buffer (see the AST runtime): the
                // `String + &str` is a `push_str`, keeping `s = s + piece` loops
                // O(n) overall rather than reallocating on every concat.
                Ok(Value::String(
                    (left.into_string()? + &right.as_string()?).into(),
                ))
            }
            BinaryOp::Add => Ok(Value::I64(left.as_i64()? + right.as_i64()?)),
            BinaryOp::Subtract => Ok(Value::I64(left.as_i64()? - right.as_i64()?)),
            BinaryOp::Multiply => Ok(Value::I64(left.as_i64()? * right.as_i64()?)),
            BinaryOp::Divide => {
                let divisor = right.as_i64()?;
                if divisor == 0 {
                    Err(RuntimeError::new("L0404", "division by zero"))
                } else {
                    // Wrap `i64::MIN / -1` to `i64::MIN` (rather than panicking),
                    // matching the AST runtime and the native backend.
                    Ok(Value::I64(left.as_i64()?.wrapping_div(divisor)))
                }
            }
            BinaryOp::Remainder => {
                let divisor = right.as_i64()?;
                if divisor == 0 {
                    Err(RuntimeError::new("L0404", "remainder by zero"))
                } else {
                    Ok(Value::I64(left.as_i64()?.wrapping_rem(divisor)))
                }
            }
            BinaryOp::Equal => Ok(Value::Bool(left == right)),
            BinaryOp::NotEqual => Ok(Value::Bool(left != right)),
            // String ordering is lexicographic by Unicode code point, matching
            // the AST runtime.
            BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual
                if matches!((&left, &right), (Value::String(_), Value::String(_))) =>
            {
                let (l, r) = (left.as_string()?, right.as_string()?);
                Ok(Value::Bool(match op {
                    BinaryOp::Less => l < r,
                    BinaryOp::LessEqual => l <= r,
                    BinaryOp::Greater => l > r,
                    BinaryOp::GreaterEqual => l >= r,
                    _ => unreachable!("guarded to ordering operators"),
                }))
            }
            // Char ordering compares by Unicode code point; byte ordering is
            // numeric. Both fall through to i64 ordering otherwise.
            BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual
                if scalar_order_keys(&left, &right).is_some() =>
            {
                let (l, r) = scalar_order_keys(&left, &right)
                    .expect("guarded by the match arm condition above");
                Ok(Value::Bool(match op {
                    BinaryOp::Less => l < r,
                    BinaryOp::LessEqual => l <= r,
                    BinaryOp::Greater => l > r,
                    BinaryOp::GreaterEqual => l >= r,
                    _ => unreachable!("guarded to ordering operators"),
                }))
            }
            BinaryOp::Less => Ok(Value::Bool(left.as_i64()? < right.as_i64()?)),
            BinaryOp::LessEqual => Ok(Value::Bool(left.as_i64()? <= right.as_i64()?)),
            BinaryOp::Greater => Ok(Value::Bool(left.as_i64()? > right.as_i64()?)),
            BinaryOp::GreaterEqual => Ok(Value::Bool(left.as_i64()? >= right.as_i64()?)),
            // Integer bitwise ops on two i64s, using the shared masked-shift
            // helpers so the AST, IR, and bytecode backends are bit-identical.
            BinaryOp::BitAnd => Ok(Value::I64(left.as_i64()? & right.as_i64()?)),
            BinaryOp::BitOr => Ok(Value::I64(left.as_i64()? | right.as_i64()?)),
            BinaryOp::BitXor => Ok(Value::I64(left.as_i64()? ^ right.as_i64()?)),
            BinaryOp::Shl => Ok(Value::I64(shift_left(left.as_i64()?, right.as_i64()?))),
            BinaryOp::Shr => Ok(Value::I64(shift_right(left.as_i64()?, right.as_i64()?))),
            BinaryOp::And | BinaryOp::Or => unreachable!("logical ops short-circuit in eval_expr"),
        }
    }

    fn builtin_alloc(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("alloc", 1, args.len()))?;
        self.heap.push(Some(value));
        Ok(Value::Ptr(self.heap.len() - 1))
    }

    fn builtin_load(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("load", 1, args.len()))?;
        let slot = ptr.as_ptr()?;
        self.heap
            .get(slot)
            .and_then(|value| value.clone())
            .ok_or_else(|| RuntimeError::new("L0406", format!("invalid pointer `{slot}`")))
    }

    fn builtin_store(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("store", 2, args.len()))?;
        let slot = ptr.as_ptr()?;
        let Some(target) = self.heap.get_mut(slot) else {
            return Err(RuntimeError::new(
                "L0406",
                format!("invalid pointer `{slot}`"),
            ));
        };
        if target.is_none() {
            return Err(RuntimeError::new(
                "L0406",
                format!("invalid pointer `{slot}`"),
            ));
        }
        *target = Some(value);
        Ok(Value::Void)
    }

    fn builtin_dealloc(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("dealloc", 1, args.len()))?;
        let slot = ptr.as_ptr()?;
        let Some(value) = self.heap.get_mut(slot) else {
            return Err(RuntimeError::new(
                "L0406",
                format!("invalid pointer `{slot}`"),
            ));
        };
        if value.take().is_none() {
            return Err(RuntimeError::new(
                "L0406",
                format!("invalid pointer `{slot}`"),
            ));
        }
        Ok(Value::Void)
    }

    /// `size_of(x) -> i64`: the C-natural byte size of `x`'s type. See
    /// [`Value::layout_size`].
    fn builtin_size_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("size_of", 1, args.len()))?;
        value.layout_size().map(Value::I64).ok_or_else(|| {
            RuntimeError::new(
                "L0431",
                "size_of requires a type with a defined memory layout",
            )
        })
    }

    /// `align_of(x) -> i64`: the C-natural alignment of `x`'s type.
    fn builtin_align_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("align_of", 1, args.len()))?;
        value.layout_align().map(Value::I64).ok_or_else(|| {
            RuntimeError::new(
                "L0431",
                "align_of requires a type with a defined memory layout",
            )
        })
    }

    /// `offset_of(x, "field") -> i64`: the C-natural byte offset of `field`
    /// within struct value `x`.
    fn builtin_offset_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value, field]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("offset_of", 2, args.len()))?;
        let field = field.as_string()?;
        value
            .layout_field_offset(&field)
            .map(Value::I64)
            .ok_or_else(|| {
                RuntimeError::new(
                    "L0431",
                    format!("offset_of could not resolve field `{field}` in a struct value"),
                )
            })
    }

    /// `ptr_to_int(p) -> i64`: the integer handle of a raw pointer; round-trips
    /// with `int_to_ptr`.
    fn builtin_ptr_to_int(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("ptr_to_int", 1, args.len()))?;
        Ok(Value::I64(ptr.as_ptr()? as i64))
    }

    /// `int_to_ptr(n) -> ptr<T>`: reconstruct a raw pointer from an integer
    /// handle (the inverse of `ptr_to_int`).
    fn builtin_int_to_ptr(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("int_to_ptr", 1, args.len()))?;
        Ok(Value::Ptr(handle.as_i64()? as usize))
    }

    fn builtin_read_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_file", 1, args.len()))?;
        let path = path.as_string()?;
        fs::read_to_string(&path)
            .map(|s| Value::String(s.into()))
            .map_err(|error| {
                RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
            })
    }

    fn builtin_write_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path, contents]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("write_file", 2, args.len()))?;
        let path = path.as_string()?;
        let contents = contents.as_string()?;
        fs::write(&path, contents)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to write `{path}`: {error}"))
            })
    }

    fn builtin_append_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path, contents]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("append_file", 2, args.len()))?;
        let path = path.as_string()?;
        let contents = contents.as_string()?;
        use std::io::Write;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(contents.as_bytes()))
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to append `{path}`: {error}"))
            })
    }

    fn builtin_file_exists(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("file_exists", 1, args.len()))?;
        Ok(Value::Bool(fs::metadata(path.as_string()?).is_ok()))
    }

    fn builtin_read_lines(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_lines", 1, args.len()))?;
        let path = path.as_string()?;
        let contents = fs::read_to_string(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        Ok(Value::Array(
            contents
                .lines()
                .map(|line| Value::String((line.to_string()).into()))
                .collect(),
        ))
    }

    fn builtin_read_bytes(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_bytes", 1, args.len()))?;
        let path = path.as_string()?;
        let bytes = fs::read(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        Ok(Value::Array(bytes.into_iter().map(Value::Byte).collect()))
    }

    fn builtin_write_bytes(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path, data]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("write_bytes", 2, args.len()))?;
        let path = path.as_string()?;
        let bytes = Self::value_to_bytes("write_bytes", data)?;
        fs::write(&path, bytes)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to write `{path}`: {error}"))
            })
    }

    /// Convert a `list<byte>` (`Value::Array` of `Value::Byte`) to raw bytes,
    /// erroring on a non-array or a non-byte element.
    fn value_to_bytes(name: &str, value: Value) -> Result<Vec<u8>, RuntimeError> {
        let Value::Array(values) = value else {
            return Err(RuntimeError::new(
                "L0418",
                format!("{name} expects a `list<byte>` value"),
            ));
        };
        values
            .into_iter()
            .map(|element| match element {
                Value::Byte(b) => Ok(b),
                other => Err(RuntimeError::new(
                    "L0418",
                    format!("{name} expects `list<byte>` but found `{other}`"),
                )),
            })
            .collect()
    }

    fn builtin_file_size(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("file_size", 1, args.len()))?;
        let path = path.as_string()?;
        let metadata = fs::metadata(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        Ok(Value::I64(metadata.len() as i64))
    }

    fn builtin_is_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("is_file", 1, args.len()))?;
        Ok(Value::Bool(
            fs::metadata(path.as_string()?)
                .map(|m| m.is_file())
                .unwrap_or(false),
        ))
    }

    fn builtin_is_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("is_dir", 1, args.len()))?;
        Ok(Value::Bool(
            fs::metadata(path.as_string()?)
                .map(|m| m.is_dir())
                .unwrap_or(false),
        ))
    }

    fn builtin_list_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_dir", 1, args.len()))?;
        let path = path.as_string()?;
        let entries = fs::read_dir(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
            })?;
            names.push(Value::String(
                (entry.file_name().to_string_lossy().to_string()).into(),
            ));
        }
        Ok(Value::Array((names).into()))
    }

    fn builtin_make_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("make_dir", 1, args.len()))?;
        let path = path.as_string()?;
        fs::create_dir_all(&path)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to create `{path}`: {error}"))
            })
    }

    fn builtin_remove_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("remove_file", 1, args.len()))?;
        let path = path.as_string()?;
        fs::remove_file(&path)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to remove `{path}`: {error}"))
            })
    }

    fn builtin_remove_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("remove_dir", 1, args.len()))?;
        let path = path.as_string()?;
        fs::remove_dir(&path)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to remove `{path}`: {error}"))
            })
    }

    fn builtin_sys_status(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [program, command_args]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sys_status", 2, args.len()))?;
        let program = program.as_string()?;
        let command_args = command_args.as_string_array()?;
        let output = Command::new(&program)
            .args(command_args)
            .output()
            .map_err(|error| {
                RuntimeError::resource("L0416", format!("failed to run `{program}`: {error}"))
            })?;
        Ok(Value::I64(output.status.code().unwrap_or(-1).into()))
    }

    fn builtin_sys_output(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [program, command_args]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sys_output", 2, args.len()))?;
        let program = program.as_string()?;
        let command_args = command_args.as_string_array()?;
        let output = Command::new(&program)
            .args(command_args)
            .output()
            .map_err(|error| {
                RuntimeError::resource("L0416", format!("failed to run `{program}`: {error}"))
            })?;
        Ok(Value::String(
            (String::from_utf8_lossy(&output.stdout).to_string()).into(),
        ))
    }

    fn builtin_print(
        &self,
        name: &'static str,
        args: Vec<Value>,
        newline: bool,
    ) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        let text = text.as_string()?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let result = if newline {
            writeln!(handle, "{text}")
        } else {
            write!(handle, "{text}")
        };
        result.map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    fn builtin_warn(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("warn", 1, args.len()))?;
        let text = text.as_string()?;
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        writeln!(handle, "{text}").map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stderr: {error}"))
        })?;
        Ok(Value::Void)
    }

    fn builtin_flush(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        if !args.is_empty() {
            return Err(Self::wrong_arity("flush", 0, args.len()));
        }
        std::io::stdout().flush().map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to flush stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    /// `mono_now() -> i64`: nanoseconds since a fixed per-process monotonic
    /// baseline. Non-decreasing within a run. Routes through the shared
    /// `monotonic_now_nanos` baseline so the IR interpreter, the bytecode VM
    /// (which runs on this interpreter), and the AST runtime agree.
    fn builtin_mono_now(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mono_now", 0, args.len()))?;
        Ok(Value::I64(monotonic_now_nanos()))
    }

    /// `wall_now() -> i64`: milliseconds since the Unix epoch (wall-clock time).
    fn builtin_wall_now(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("wall_now", 0, args.len()))?;
        Ok(Value::I64(wall_now_millis()))
    }

    /// `sleep_millis(ms i64) -> void`: sleep the current thread for `ms`
    /// milliseconds; a negative `ms` sleeps for zero (no error).
    fn builtin_sleep_millis(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ms]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sleep_millis", 1, args.len()))?;
        let ms = expect_i64("sleep_millis", ms)?;
        sleep_millis(ms);
        Ok(Value::Void)
    }

    /// `wasm_log(x i64) -> void`: the host log builtin. On the WASM backend it
    /// lowers to a `call` of the imported `env.log_i64`; on the interpreters it
    /// prints the value as a stdout line, kept at parity with the AST runtime so
    /// all backends observe the same side effect.
    fn builtin_wasm_log(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("wasm_log", 1, args.len()))?;
        let value = value.as_i64()?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{value}").map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    /// `console_log(s string) -> void`: the JS/DOM host console builtin. On the
    /// WASM backend it lowers to a `call` of the imported
    /// `env.console_log(ptr, len)`; on the interpreters it prints the string as a
    /// stdout line, kept at parity with the AST runtime so all backends observe
    /// the same side effect.
    fn builtin_console_log(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("console_log", 1, args.len()))?;
        let text = text.as_string()?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{text}").map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    /// `dom_set_text(id string, text string) -> void`: the DOM-write primitive. On
    /// the WASM backend it lowers to a `call` of the imported
    /// `env.dom_set_text(id_ptr, id_len, text_ptr, text_len)`; on the interpreters
    /// it prints the deterministic line `id=text`, kept at parity with the AST
    /// runtime so all backends observe the same side effect.
    fn builtin_dom_set_text(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [id, text]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("dom_set_text", 2, args.len()))?;
        let id = id.as_string()?;
        let text = text.as_string()?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{id}={text}").map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    /// `assert(cond bool) -> void`: raises the same catchable user-error (code
    /// `L0420`) a `throw` produces when `cond` is false; returns void otherwise.
    /// Kept at parity with the AST runtime's `builtin_assert`.
    fn builtin_assert(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("assert", 1, args.len()))?;
        if value.as_bool()? {
            Ok(Value::Void)
        } else {
            Err(RuntimeError::new("L0420", "assertion failed"))
        }
    }

    fn builtin_to_string(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_string", 1, args.len()))?;
        match value {
            Value::I64(_)
            | Value::Int { .. }
            | Value::F64(_)
            | Value::F32(_)
            | Value::Bool(_)
            | Value::String(_)
            | Value::Char(_)
            | Value::Byte(_) => Ok(Value::String((value.to_string()).into())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("to_string cannot convert `{other}`"),
            )),
        }
    }

    /// `char_code(c char) -> i64`: the char's Unicode scalar value.
    fn builtin_char_code(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("char_code", 1, args.len()))?;
        match value {
            Value::Char(c) => Ok(Value::I64(c as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("char_code expects a char but got `{other}`"),
            )),
        }
    }

    /// `char_from(i i64) -> char`: the char for a Unicode scalar value; a runtime
    /// error when `i` is not a valid Unicode scalar.
    fn builtin_char_from(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("char_from", 1, args.len()))?;
        let code = expect_i64("char_from", value)?;
        u32::try_from(code)
            .ok()
            .and_then(char::from_u32)
            .map(Value::Char)
            .ok_or_else(|| {
                RuntimeError::new(
                    "L0417",
                    format!("char_from got `{code}`, which is not a valid Unicode scalar value"),
                )
            })
    }

    /// `is_digit(c char) -> bool`: whether `c` is an ASCII digit (`0`-`9`).
    fn builtin_is_digit(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_digit", args, |c| c.is_ascii_digit())
    }

    /// `is_alpha(c char) -> bool`: whether `c` is an alphabetic character.
    fn builtin_is_alpha(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_alpha", args, |c| c.is_alphabetic())
    }

    /// `is_alnum(c char) -> bool`: whether `c` is alphabetic or numeric.
    fn builtin_is_alnum(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_alnum", args, |c| c.is_alphanumeric())
    }

    /// `is_whitespace(c char) -> bool`: whether `c` is a whitespace character.
    fn builtin_is_whitespace(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_whitespace", args, |c| c.is_whitespace())
    }

    /// `is_upper(c char) -> bool`: whether `c` is an uppercase character.
    fn builtin_is_upper(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_upper", args, |c| c.is_uppercase())
    }

    /// `is_lower(c char) -> bool`: whether `c` is a lowercase character.
    fn builtin_is_lower(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_lower", args, |c| c.is_lowercase())
    }

    /// Shared helper for the deterministic `char -> bool` classification
    /// predicates: unwrap a single `char` operand and apply `test`, reporting a
    /// runtime error (never a panic) on a non-char operand.
    fn char_predicate(
        name: &'static str,
        args: Vec<Value>,
        test: impl Fn(char) -> bool,
    ) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        match value {
            Value::Char(c) => Ok(Value::Bool(test(c))),
            other => Err(RuntimeError::new(
                "L0417",
                format!("{name} expects a char but got `{other}`"),
            )),
        }
    }

    /// `byte(i i64) -> byte`: an 8-bit unsigned value; a runtime error outside 0-255.
    fn builtin_byte(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("byte", 1, args.len()))?;
        let number = expect_i64("byte", value)?;
        u8::try_from(number).map(Value::Byte).map_err(|_| {
            RuntimeError::new(
                "L0417",
                format!("byte got `{number}`, which is outside the 0-255 range"),
            )
        })
    }

    /// `byte_val(b byte) -> i64`: the numeric value of a byte.
    fn builtin_byte_val(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("byte_val", 1, args.len()))?;
        match value {
            Value::Byte(b) => Ok(Value::I64(b as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("byte_val expects a byte but got `{other}`"),
            )),
        }
    }

    /// `to_<T>(x i64) -> T`: wrapping reinterpret of an `i64` into fixed-width
    /// integer `T`; shared by every `to_i8`/`to_i16`/…/`to_usize` conversion.
    fn builtin_to_int(
        name: &'static str,
        args: Vec<Value>,
        ty: IntKind,
    ) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        Ok(Value::int(expect_i64(name, value)?, ty))
    }

    /// `to_i64(x) -> i64`: widen a fixed-width integer into `i64` (identity on
    /// the already-normalized cell).
    fn builtin_to_i64(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_i64", 1, args.len()))?;
        match value {
            Value::Int { value, .. } => Ok(Value::I64(value)),
            other => Err(RuntimeError::new(
                "L0407",
                format!("to_i64 expects a fixed-width integer but got `{other}`"),
            )),
        }
    }

    /// `to_f32(x f64) -> f32`: round an `f64` to the nearest `f32`.
    fn builtin_to_f32(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_f32", 1, args.len()))?;
        Ok(Value::F32(value.as_f64()? as f32))
    }

    /// `to_f64(x f32) -> f64`: widen an `f32` to `f64` (exact).
    fn builtin_to_f64(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_f64", 1, args.len()))?;
        match value {
            Value::F32(value) => Ok(Value::F64(f64::from(value))),
            other => Err(RuntimeError::new(
                "L0421",
                format!("to_f64 expects an f32 but got `{other}`"),
            )),
        }
    }

    fn builtin_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("len", 1, args.len()))?;
        match value {
            Value::Array(values) => Ok(Value::I64(values.len() as i64)),
            Value::String(text) => Ok(Value::I64(text.chars().count() as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("len expects a string or array but got `{other}`"),
            )),
        }
    }

    /// `list_new() -> list<T>`: a fresh empty list, represented as an array.
    fn builtin_list_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_new", 0, args.len()))?;
        Ok(Value::Array((Vec::new()).into()))
    }

    /// `env(name string) -> option<string>`: `some(value)` when the environment
    /// variable is set, `none` otherwise.
    fn builtin_env(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [name]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("env", 1, args.len()))?;
        let name = expect_string("env", name)?;
        Ok(option_value(
            std::env::var(&name).ok().map(|s| Value::String(s.into())),
        ))
    }

    /// `os_random(len i64) -> result<list<byte>, string>`: `len`
    /// cryptographically-secure random bytes from the operating-system CSPRNG as
    /// `ok(list<byte>)`, or `err(message)` if the OS RNG fails. `len == 0`
    /// returns `ok([])`; `len < 0` returns `err("os_random length must be
    /// non-negative")`. Never a seeded/deterministic PRNG and never a panic.
    /// Routes through the shared [`os_random_bytes`] helper so the IR
    /// interpreter, the bytecode VM (which runs on it), and the AST runtime all
    /// agree on behavior.
    fn builtin_os_random(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [len]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("os_random", 1, args.len()))?;
        let len = expect_i64("os_random", len)?;
        Ok(result_value(match os_random_bytes(len) {
            Ok(bytes) => Ok(Value::Array(bytes.into_iter().map(Value::Byte).collect())),
            Err(message) => Err(Value::String((message).into())),
        }))
    }

    /// `args() -> list<string>`: the running program's CLI arguments (an empty
    /// list when none were passed), represented as an array of strings.
    fn builtin_args(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("args", 0, args.len()))?;
        Ok(Value::Array(
            self.program_args
                .iter()
                .cloned()
                .map(|s| Value::String(s.into()))
                .collect(),
        ))
    }

    /// `parallel_map(f fn(i64) -> i64, args list<i64>) -> list<i64>`: evaluate
    /// `f(arg)` for every element of `args` concurrently on separate OS threads,
    /// returning the results in the SAME order as `args`. Each thread builds a
    /// fresh sibling interpreter over the shared `&IrModule` (heaps are
    /// per-thread, so there is no shared mutable state and no locking). Output
    /// order follows input order, so results are fully deterministic.
    fn builtin_parallel_map(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [callee, elements]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("parallel_map", 2, args.len()))?;
        // `parallel_map` accepts either a named function value or a capturing
        // closure. A closure is self-contained (it carries its captured snapshot,
        // all `Send`) and the worker's fresh interpreter rebuilds the same
        // id-keyed body table from the shared module, so invoking it there is
        // sound and stays order-deterministic.
        let callable = match callee {
            Value::Func(name) => IrParallelCallable::Func((name).into()),
            Value::Closure(closure) => IrParallelCallable::Closure(*closure),
            other => {
                return Err(RuntimeError::new(
                    "L0417",
                    format!("parallel_map expects a function but got `{other}`"),
                ));
            }
        };
        let arg_values = expect_list("parallel_map", elements)?;

        let module = self.module;
        let module_arc = &self.module_arc;
        let callable = &callable;
        let results: Vec<Value> = std::thread::scope(|scope| {
            let handles: Vec<_> = arg_values
                .iter()
                .map(|value| {
                    let callable = callable.clone();
                    let value = value.clone();
                    let arc = Arc::clone(module_arc);
                    scope.spawn(move || {
                        let mut runtime = IrRuntime::new(module, arc)?;
                        match callable {
                            IrParallelCallable::Func(name) => {
                                runtime.call_function(&name, vec![value])
                            }
                            IrParallelCallable::Closure(closure) => {
                                runtime.invoke_closure(&closure, vec![value])
                            }
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|_| {
                        Err(RuntimeError::new(
                            "L0401",
                            "parallel_map worker thread panicked",
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        })?;

        Ok(Value::Array((results).into()))
    }

    /// `chan_new() -> Chan`: create an unbounded `i64` message-passing channel.
    fn builtin_chan_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("chan_new", 0, args.len()))?;
        Ok(new_chan())
    }

    /// `send(ch Chan, v i64) -> void`: enqueue `v` (never blocks; unbounded).
    fn builtin_send(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [chan, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("send", 2, args.len()))?;
        let chan = expect_chan("send", chan)?;
        let value = expect_i64("send", value)?;
        chan.sender
            .send(Value::I64(value))
            .map_err(|_| RuntimeError::new("L0401", "send on a channel with no live receiver"))?;
        Ok(Value::Void)
    }

    /// `recv(ch Chan) -> i64`: dequeue, blocking until a value is available.
    fn builtin_recv(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [chan]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("recv", 1, args.len()))?;
        let chan = expect_chan("recv", chan)?;
        let receiver = chan
            .receiver
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "recv on a poisoned channel"))?;
        receiver
            .recv()
            .map_err(|_| RuntimeError::new("L0401", "recv on a closed, empty channel"))
    }

    /// `try_recv(ch Chan) -> option<i64>`: non-blocking; `some(v)` or `none`.
    fn builtin_try_recv(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [chan]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("try_recv", 1, args.len()))?;
        let chan = expect_chan("try_recv", chan)?;
        let receiver = chan
            .receiver
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "try_recv on a poisoned channel"))?;
        Ok(option_value(receiver.try_recv().ok()))
    }

    /// `spawn(f fn(Chan, i64) -> void, ch Chan, v i64) -> Task`: run `f(ch, v)` on
    /// a detached OS thread that owns a share of the module (an `Arc<IrModule>`
    /// clone) and builds its own interpreter over `&*arc`, then returns a one-shot
    /// `Task` handle so the thread is `task_join`ed exactly once.
    fn builtin_spawn(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [callee, chan, value]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("spawn", 3, args.len()))?;
        let func_name = match callee {
            Value::Func(name) => name,
            other => {
                return Err(RuntimeError::new(
                    "L0417",
                    format!("spawn expects a function but got `{other}`"),
                ));
            }
        };
        let chan = expect_chan("spawn", chan)?;
        let value = expect_i64("spawn", value)?;
        let arc = Arc::clone(&self.module_arc);
        let handle = std::thread::spawn(move || {
            let mut runtime = IrRuntime::new(&arc, Arc::clone(&arc))?;
            runtime.call_function(&func_name, vec![Value::Chan(chan), Value::I64(value)])
        });
        Ok(Value::Task(Task {
            handle: Arc::new(std::sync::Mutex::new(Some(handle))),
        }))
    }

    /// `task_join(t Task) -> void`: wait for the spawned thread; a second
    /// `task_join` on an already-joined handle is a harmless no-op.
    fn builtin_task_join(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [task]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("task_join", 1, args.len()))?;
        let task = expect_task("task_join", task)?;
        join_task(&task)
    }

    /// `mutex_new(v i64) -> Mutex`: a shared mutex over one `i64`.
    fn builtin_mutex_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_new", 1, args.len()))?;
        let value = expect_i64("mutex_new", value)?;
        Ok(Value::Mutex(SharedMutex {
            cell: Arc::new(std::sync::Mutex::new(value)),
        }))
    }

    /// `mutex_get(m Mutex) -> i64`: lock, read, unlock.
    fn builtin_mutex_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [mutex]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_get", 1, args.len()))?;
        let mutex = expect_mutex("mutex_get", mutex)?;
        let guard = mutex
            .cell
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "mutex_get on a poisoned mutex"))?;
        Ok(Value::I64(*guard))
    }

    /// `mutex_set(m Mutex, v i64) -> void`: lock, write, unlock.
    fn builtin_mutex_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [mutex, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_set", 2, args.len()))?;
        let mutex = expect_mutex("mutex_set", mutex)?;
        let value = expect_i64("mutex_set", value)?;
        let mut guard = mutex
            .cell
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "mutex_set on a poisoned mutex"))?;
        *guard = value;
        Ok(Value::Void)
    }

    /// `mutex_add(m Mutex, delta i64) -> i64`: lock, `v += delta`, return the new
    /// value, unlock — an atomic read-modify-write so worker threads accumulate
    /// safely.
    fn builtin_mutex_add(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [mutex, delta]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_add", 2, args.len()))?;
        let mutex = expect_mutex("mutex_add", mutex)?;
        let delta = expect_i64("mutex_add", delta)?;
        let mut guard = mutex
            .cell
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "mutex_add on a poisoned mutex"))?;
        *guard = guard.wrapping_add(delta);
        Ok(Value::I64(*guard))
    }

    /// `atomic_new(v i64) -> atomic_i64`: allocate a fresh shared atomic cell
    /// initialized to `v`. Cloning the returned handle shares the same
    /// `Arc<AtomicI64>`, so several threads observe each other's updates.
    fn builtin_atomic_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_new", 1, args.len()))?;
        let value = expect_i64("atomic_new", value)?;
        Ok(Value::Atomic(SharedAtomic {
            cell: Arc::new(AtomicI64::new(value)),
        }))
    }

    /// `atomic_load(a atomic_i64) -> i64`: read the cell (SeqCst).
    fn builtin_atomic_load(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_load", 1, args.len()))?;
        let atomic = expect_atomic("atomic_load", atomic)?;
        Ok(Value::I64(atomic.cell.load(Ordering::SeqCst)))
    }

    /// `atomic_store(a atomic_i64, v i64) -> void`: write the cell (SeqCst).
    fn builtin_atomic_store(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_store", 2, args.len()))?;
        let atomic = expect_atomic("atomic_store", atomic)?;
        let value = expect_i64("atomic_store", value)?;
        atomic.cell.store(value, Ordering::SeqCst);
        Ok(Value::Void)
    }

    /// `atomic_swap(a atomic_i64, v i64) -> i64`: store `v`, return the previous
    /// value (SeqCst).
    fn builtin_atomic_swap(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_swap", 2, args.len()))?;
        let atomic = expect_atomic("atomic_swap", atomic)?;
        let value = expect_i64("atomic_swap", value)?;
        Ok(Value::I64(atomic.cell.swap(value, Ordering::SeqCst)))
    }

    /// `atomic_cas(a atomic_i64, expected i64, new i64) -> i64`: strong
    /// compare-and-swap. Returns the value that was in the cell (equal to
    /// `expected` on success). SeqCst on both success and failure.
    fn builtin_atomic_cas(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic, expected, new]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_cas", 3, args.len()))?;
        let atomic = expect_atomic("atomic_cas", atomic)?;
        let expected = expect_i64("atomic_cas", expected)?;
        let new = expect_i64("atomic_cas", new)?;
        let observed =
            match atomic
                .cell
                .compare_exchange(expected, new, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(prev) => prev,
                Err(current) => current,
            };
        Ok(Value::I64(observed))
    }

    /// `atomic_add(a atomic_i64, v i64) -> i64`: fetch-and-add, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_add(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_add", args)?;
        Ok(Value::I64(atomic.cell.fetch_add(value, Ordering::SeqCst)))
    }

    /// `atomic_sub(a atomic_i64, v i64) -> i64`: fetch-and-sub, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_sub(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_sub", args)?;
        Ok(Value::I64(atomic.cell.fetch_sub(value, Ordering::SeqCst)))
    }

    /// `atomic_and(a atomic_i64, v i64) -> i64`: fetch-and-and, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_and(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_and", args)?;
        Ok(Value::I64(atomic.cell.fetch_and(value, Ordering::SeqCst)))
    }

    /// `atomic_or(a atomic_i64, v i64) -> i64`: fetch-and-or, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_or(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_or", args)?;
        Ok(Value::I64(atomic.cell.fetch_or(value, Ordering::SeqCst)))
    }

    /// `atomic_xor(a atomic_i64, v i64) -> i64`: fetch-and-xor, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_xor(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_xor", args)?;
        Ok(Value::I64(atomic.cell.fetch_xor(value, Ordering::SeqCst)))
    }

    /// Shared argument-decoding for the `atomic_<op>(a atomic_i64, v i64)`
    /// fetch-and-op family: exactly two arguments, an atomic handle then an
    /// `i64` operand.
    fn atomic_binary_args(
        name: &str,
        args: Vec<Value>,
    ) -> Result<(SharedAtomic, i64), RuntimeError> {
        let [atomic, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 2, args.len()))?;
        let atomic = expect_atomic(name, atomic)?;
        let value = expect_i64(name, value)?;
        Ok((atomic, value))
    }

    /// Push a freshly opened socket resource into the handle table, returning its
    /// index wrapped as a `Value::Socket`.
    fn register_socket(&mut self, resource: SocketResource) -> Value {
        self.sockets.push(Some(resource));
        Value::Socket(self.sockets.len() - 1)
    }

    /// Resolve a socket handle argument to its live slot index, reporting a
    /// wrong-argument-type error for a non-socket value and a stale-handle error
    /// for a closed or invalid slot.
    fn socket_slot(&self, name: &str, value: &Value) -> Result<usize, RuntimeError> {
        let Value::Socket(handle) = value else {
            return Err(RuntimeError::new(
                "L0417",
                format!("{name} expects a Socket but got `{value}`"),
            ));
        };
        match self.sockets.get(*handle) {
            Some(Some(_)) => Ok(*handle),
            _ => Err(RuntimeError::new(
                "L0406",
                format!("{name} received a closed or invalid socket `{handle}`"),
            )),
        }
    }

    /// Push a freshly spawned child into the handle table, returning its index
    /// wrapped as a `Value::Process`. Mirrors `register_socket` and the AST
    /// interpreter's `register_process`.
    fn register_process(&mut self, resource: ProcessResource) -> Value {
        self.processes.push(Some(resource));
        Value::Process(self.processes.len() - 1)
    }

    /// Resolve a process handle argument to its live slot index. Mirrors
    /// `socket_slot` and the AST interpreter's `process_slot`.
    fn process_slot(&self, name: &str, value: &Value) -> Result<usize, RuntimeError> {
        let Value::Process(handle) = value else {
            return Err(RuntimeError::new(
                "L0417",
                format!("{name} expects a process but got `{value}`"),
            ));
        };
        match self.processes.get(*handle) {
            Some(Some(_)) => Ok(*handle),
            _ => Err(RuntimeError::new(
                "L0406",
                format!("{name} received a reaped or invalid process `{handle}`"),
            )),
        }
    }

    /// `proc_spawn(cmd string, args array<string>) -> result<process, string>`:
    /// mirrors the AST interpreter's `builtin_proc_spawn` exactly.
    fn builtin_proc_spawn(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [cmd, cmd_args]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_spawn", 2, args.len()))?;
        let cmd = expect_string("proc_spawn", cmd)?;
        let cmd_args = cmd_args.as_string_array()?;
        match Command::new(&cmd)
            .args(cmd_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => {
                let handle = self.register_process(ProcessResource { child });
                Ok(result_value(Ok(handle)))
            }
            Err(error) => Ok(result_value(Err(Value::String((error.to_string()).into())))),
        }
    }

    /// `proc_wait(p process) -> result<i64, string>`: mirrors the AST interpreter.
    fn builtin_proc_wait(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [proc]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_wait", 1, args.len()))?;
        let slot = self.process_slot("proc_wait", &proc)?;
        let Some(resource) = self.processes[slot].as_mut() else {
            return Ok(result_value(Err(Value::String(
                ("proc_wait requires a live process".to_string()).into(),
            ))));
        };
        match resource.child.wait() {
            Ok(status) => Ok(result_value(Ok(Value::I64(process_exit_code(&status))))),
            Err(error) => Ok(result_value(Err(Value::String((error.to_string()).into())))),
        }
    }

    /// `proc_stdout(p process) -> result<string, string>`: mirrors the AST
    /// interpreter; the pipe is taken on first read (second read is EOF).
    fn builtin_proc_stdout(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Read;
        let [proc]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_stdout", 1, args.len()))?;
        let slot = self.process_slot("proc_stdout", &proc)?;
        let Some(resource) = self.processes[slot].as_mut() else {
            return Ok(result_value(Err(Value::String(
                ("proc_stdout requires a live process".to_string()).into(),
            ))));
        };
        let mut buffer = String::new();
        match resource
            .child
            .stdout
            .take()
            .map(|mut pipe| pipe.read_to_string(&mut buffer))
        {
            None => Ok(result_value(Ok(Value::String((String::new()).into())))),
            Some(Ok(_)) => Ok(result_value(Ok(Value::String((buffer).into())))),
            Some(Err(error)) => Ok(result_value(Err(Value::String((error.to_string()).into())))),
        }
    }

    /// `proc_stderr(p process) -> result<string, string>`: mirrors the AST
    /// interpreter; the pipe is taken on first read (second read is EOF).
    fn builtin_proc_stderr(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Read;
        let [proc]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_stderr", 1, args.len()))?;
        let slot = self.process_slot("proc_stderr", &proc)?;
        let Some(resource) = self.processes[slot].as_mut() else {
            return Ok(result_value(Err(Value::String(
                ("proc_stderr requires a live process".to_string()).into(),
            ))));
        };
        let mut buffer = String::new();
        match resource
            .child
            .stderr
            .take()
            .map(|mut pipe| pipe.read_to_string(&mut buffer))
        {
            None => Ok(result_value(Ok(Value::String((String::new()).into())))),
            Some(Ok(_)) => Ok(result_value(Ok(Value::String((buffer).into())))),
            Some(Err(error)) => Ok(result_value(Err(Value::String((error.to_string()).into())))),
        }
    }

    /// `proc_kill(p process) -> result<i64, string>`: mirrors the AST interpreter.
    fn builtin_proc_kill(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [proc]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_kill", 1, args.len()))?;
        let slot = self.process_slot("proc_kill", &proc)?;
        let Some(resource) = self.processes[slot].as_mut() else {
            return Ok(result_value(Err(Value::String(
                ("proc_kill requires a live process".to_string()).into(),
            ))));
        };
        match resource.child.kill() {
            Ok(()) => Ok(result_value(Ok(Value::I64(0)))),
            Err(error) => Ok(result_value(Err(Value::String((error.to_string()).into())))),
        }
    }

    /// `tcp_connect(host string, port i64) -> result<Socket, string>`.
    fn builtin_tcp_connect(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [host, port]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_connect", 2, args.len()))?;
        let host = expect_string("tcp_connect", host)?;
        let port = expect_i64("tcp_connect", port)?;
        match TcpStream::connect((host.as_str(), port as u16)) {
            Ok(stream) => {
                let socket = self.register_socket(SocketResource::Stream(stream));
                Ok(result_value(Ok(socket)))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_listen(host string, port i64) -> result<Socket, string>`.
    fn builtin_tcp_listen(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [host, port]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_listen", 2, args.len()))?;
        let host = expect_string("tcp_listen", host)?;
        let port = expect_i64("tcp_listen", port)?;
        match TcpListener::bind((host.as_str(), port as u16)) {
            Ok(listener) => {
                let socket = self.register_socket(SocketResource::Listener(listener));
                Ok(result_value(Ok(socket)))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_accept(listener Socket) -> result<Socket, string>`: block for a
    /// connection and register the accepted stream as a new handle.
    fn builtin_tcp_accept(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [listener]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_accept", 1, args.len()))?;
        let slot = self.socket_slot("tcp_accept", &listener)?;
        let accepted = match &self.sockets[slot] {
            Some(SocketResource::Listener(listener)) => listener.accept(),
            _ => {
                return Ok(result_value(Err(Value::String(
                    ("tcp_accept requires a listening socket".to_string()).into(),
                ))));
            }
        };
        match accepted {
            Ok((stream, _addr)) => {
                let socket = self.register_socket(SocketResource::Stream(stream));
                Ok(result_value(Ok(socket)))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_accept_nb(listener Socket) -> result<option<Socket>, string>`:
    /// non-blocking accept. Returns `ok(some(client))` when a connection is
    /// pending, `ok(none)` when the listener would block (no pending connection),
    /// and `err(message)` on a real error. The listener must first be put into
    /// non-blocking mode with `set_nonblocking`.
    fn builtin_tcp_accept_nb(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [listener]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_accept_nb", 1, args.len()))?;
        let slot = self.socket_slot("tcp_accept_nb", &listener)?;
        let accepted = match &self.sockets[slot] {
            Some(SocketResource::Listener(listener)) => listener.accept(),
            _ => {
                return Ok(result_value(Err(Value::String(
                    ("tcp_accept_nb requires a listening socket".to_string()).into(),
                ))));
            }
        };
        match accepted {
            Ok((stream, _addr)) => {
                let socket = self.register_socket(SocketResource::Stream(stream));
                Ok(result_value(Ok(option_value(Some(socket)))))
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(result_value(Ok(option_value(None))))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_read(conn Socket) -> result<string, string>`: read up to 4096 bytes
    /// and return them as a lossy UTF-8 string (empty on clean EOF).
    fn builtin_tcp_read(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Read;
        let [conn]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_read", 1, args.len()))?;
        let slot = self.socket_slot("tcp_read", &conn)?;
        let mut buffer = [0u8; 4096];
        let read = match &mut self.sockets[slot] {
            Some(SocketResource::Stream(stream)) => stream.read(&mut buffer),
            _ => {
                return Ok(result_value(Err(Value::String(
                    ("tcp_read requires a connected stream socket".to_string()).into(),
                ))));
            }
        };
        match read {
            Ok(count) => Ok(result_value(Ok(Value::String(
                (String::from_utf8_lossy(&buffer[..count]).into_owned()).into(),
            )))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_read_nb(conn Socket, max i64) -> result<option<string>, string>`:
    /// non-blocking read of up to `max` bytes, returned as a lossy UTF-8 string.
    /// Returns `ok(some(data))` when bytes are available, `ok(some(""))` on a
    /// clean EOF (the peer closed the connection — matching blocking `tcp_read`),
    /// `ok(none)` when the stream would block (no data ready yet), and
    /// `err(message)` on a real error. `max` must be positive. The stream must
    /// first be put into non-blocking mode with `set_nonblocking`.
    fn builtin_tcp_read_nb(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Read;
        let [conn, max]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_read_nb", 2, args.len()))?;
        let slot = self.socket_slot("tcp_read_nb", &conn)?;
        let max = expect_i64("tcp_read_nb", max)?;
        if max <= 0 {
            return Ok(result_value(Err(Value::String(
                ("tcp_read_nb requires a positive `max` byte count".to_string()).into(),
            ))));
        }
        let mut buffer = vec![0u8; max as usize];
        let read = match &mut self.sockets[slot] {
            Some(SocketResource::Stream(stream)) => stream.read(&mut buffer),
            _ => {
                return Ok(result_value(Err(Value::String(
                    ("tcp_read_nb requires a connected stream socket".to_string()).into(),
                ))));
            }
        };
        match read {
            Ok(count) => Ok(result_value(Ok(option_value(Some(Value::String(
                (String::from_utf8_lossy(&buffer[..count]).into_owned()).into(),
            )))))),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(result_value(Ok(option_value(None))))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_write(conn Socket, data string) -> result<i64, string>`: write the
    /// string's bytes and return the number of bytes written.
    fn builtin_tcp_write(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [conn, data]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_write", 2, args.len()))?;
        let slot = self.socket_slot("tcp_write", &conn)?;
        let data = expect_string("tcp_write", data)?;
        let bytes = data.as_bytes();
        let written = match &mut self.sockets[slot] {
            Some(SocketResource::Stream(stream)) => {
                // Write the FULL buffer (short writes are possible) and flush.
                stream.write_all(bytes).and_then(|()| stream.flush())
            }
            _ => {
                return Ok(result_value(Err(Value::String(
                    ("tcp_write requires a connected stream socket".to_string()).into(),
                ))));
            }
        };
        match written {
            Ok(()) => Ok(result_value(Ok(Value::I64(bytes.len() as i64)))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_shutdown(conn Socket) -> void`: gracefully shut down the write half
    /// of the connection (`Shutdown::Write`), signaling EOF to the peer so any
    /// buffered response is delivered before the socket is dropped.
    fn builtin_tcp_shutdown(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::net::Shutdown;
        let [socket]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_shutdown", 1, args.len()))?;
        if let Value::Socket(handle) = socket {
            if let Some(Some(SocketResource::Stream(stream))) = self.sockets.get(handle) {
                let _ = stream.shutdown(Shutdown::Write);
            }
            Ok(Value::Void)
        } else {
            Err(RuntimeError::new(
                "L0417",
                format!("tcp_shutdown expects a Socket but got `{socket}`"),
            ))
        }
    }

    /// `tcp_close(conn Socket) -> void`: drop the handle, freeing its table slot.
    /// Closing an already-closed handle is a no-op.
    fn builtin_socket_close(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [socket]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_close", 1, args.len()))?;
        if let Value::Socket(handle) = socket {
            if let Some(slot) = self.sockets.get_mut(handle) {
                *slot = None;
            }
            Ok(Value::Void)
        } else {
            Err(RuntimeError::new(
                "L0417",
                format!("tcp_close expects a Socket but got `{socket}`"),
            ))
        }
    }

    /// `set_nonblocking(sock Socket, enabled bool) -> result<i64, string>`: put a
    /// socket (a listener, connected stream, or UDP socket) into or out of
    /// non-blocking mode via std's `set_nonblocking`. In non-blocking mode,
    /// accept/read/recv operations that would block instead surface as
    /// `ErrorKind::WouldBlock`, which the `*_nb` builtins report as `ok(none)`.
    /// Returns `ok(0)` on success or `err(message)` on failure.
    fn builtin_set_nonblocking(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [sock, enabled]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("set_nonblocking", 2, args.len()))?;
        let slot = self.socket_slot("set_nonblocking", &sock)?;
        let enabled = expect_bool("set_nonblocking", enabled)?;
        let outcome = match &self.sockets[slot] {
            Some(SocketResource::Listener(listener)) => listener.set_nonblocking(enabled),
            Some(SocketResource::Stream(stream)) => stream.set_nonblocking(enabled),
            Some(SocketResource::Udp(socket)) => socket.set_nonblocking(enabled),
            None => {
                return Ok(result_value(Err(Value::String(
                    ("set_nonblocking requires an open socket".to_string()).into(),
                ))));
            }
        };
        match outcome {
            Ok(()) => Ok(result_value(Ok(Value::I64(0)))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `udp_bind(host string, port i64) -> result<Socket, string>`.
    fn builtin_udp_bind(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [host, port]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("udp_bind", 2, args.len()))?;
        let host = expect_string("udp_bind", host)?;
        let port = expect_i64("udp_bind", port)?;
        match UdpSocket::bind((host.as_str(), port as u16)) {
            Ok(socket) => {
                let handle = self.register_socket(SocketResource::Udp(socket));
                Ok(result_value(Ok(handle)))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `udp_send_to(sock Socket, data string, host string, port i64)
    /// -> result<i64, string>`: send one datagram, returning the byte count.
    fn builtin_udp_send_to(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [sock, data, host, port]: [Value; 4] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("udp_send_to", 4, args.len()))?;
        let slot = self.socket_slot("udp_send_to", &sock)?;
        let data = expect_string("udp_send_to", data)?;
        let host = expect_string("udp_send_to", host)?;
        let port = expect_i64("udp_send_to", port)?;
        let sent = match &self.sockets[slot] {
            Some(SocketResource::Udp(socket)) => {
                socket.send_to(data.as_bytes(), (host.as_str(), port as u16))
            }
            _ => {
                return Ok(result_value(Err(Value::String(
                    ("udp_send_to requires a UDP socket".to_string()).into(),
                ))));
            }
        };
        match sent {
            Ok(count) => Ok(result_value(Ok(Value::I64(count as i64)))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `udp_recv(sock Socket) -> result<string, string>`: receive one datagram,
    /// dropping the sender address, and return it as a lossy UTF-8 string.
    fn builtin_udp_recv(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [sock]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("udp_recv", 1, args.len()))?;
        let slot = self.socket_slot("udp_recv", &sock)?;
        let mut buffer = [0u8; 4096];
        let received = match &self.sockets[slot] {
            Some(SocketResource::Udp(socket)) => socket.recv_from(&mut buffer),
            _ => {
                return Ok(result_value(Err(Value::String(
                    ("udp_recv requires a UDP socket".to_string()).into(),
                ))));
            }
        };
        match received {
            Ok((count, _addr)) => Ok(result_value(Ok(Value::String(
                (String::from_utf8_lossy(&buffer[..count]).into_owned()).into(),
            )))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `udp_recv_nb(sock Socket) -> result<option<string>, string>`: non-blocking
    /// receive of one datagram (sender address dropped), returned as a lossy
    /// UTF-8 string. Returns `ok(some(data))` when a datagram is ready,
    /// `ok(none)` when the socket would block (no datagram pending), and
    /// `err(message)` on a real error. The socket must first be put into
    /// non-blocking mode with `set_nonblocking`.
    fn builtin_udp_recv_nb(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [sock]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("udp_recv_nb", 1, args.len()))?;
        let slot = self.socket_slot("udp_recv_nb", &sock)?;
        let mut buffer = [0u8; 4096];
        let received = match &self.sockets[slot] {
            Some(SocketResource::Udp(socket)) => socket.recv_from(&mut buffer),
            _ => {
                return Ok(result_value(Err(Value::String(
                    ("udp_recv_nb requires a UDP socket".to_string()).into(),
                ))));
            }
        };
        match received {
            Ok((count, _addr)) => Ok(result_value(Ok(option_value(Some(Value::String(
                (String::from_utf8_lossy(&buffer[..count]).into_owned()).into(),
            )))))),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(result_value(Ok(option_value(None))))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `http_get(url string) -> result<string, string>`: perform an HTTP/1.1
    /// GET and return the response body on a 2xx/3xx response, or `err(message)`
    /// on a connection/parse/HTTP error.
    fn builtin_http_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [url]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("http_get", 1, args.len()))?;
        let url = expect_string("http_get", url)?;
        Ok(http_exchange("GET", &url, None))
    }

    /// `http_post(url string, body string) -> result<string, string>`: perform
    /// an HTTP/1.1 POST with a `text/plain` body and return the response body on
    /// a 2xx/3xx response, or `err(message)` on a connection/parse/HTTP error.
    fn builtin_http_post(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [url, body]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("http_post", 2, args.len()))?;
        let url = expect_string("http_post", url)?;
        let body = expect_string("http_post", body)?;
        Ok(http_exchange("POST", &url, Some(&body)))
    }

    /// `push(l, x) -> list<T>`: a new list with `x` appended.
    fn builtin_push(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("push", 2, args.len()))?;
        let mut values = expect_list("push", list)?;
        values.push(value);
        Ok(Value::Array((values).into()))
    }

    /// `array_fill(n, x) -> array<T>`: a new array of length `n`, every element
    /// `x` (matches the AST runtime; a negative length is `L0433`).
    fn builtin_array_fill(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [count, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("array_fill", 2, args.len()))?;
        let n = expect_i64("array_fill", count)?;
        if n < 0 {
            return Err(RuntimeError::new(
                "L0433",
                format!("array_fill length `{n}` is negative"),
            ));
        }
        Ok(Value::Array((vec![value; n as usize]).into()))
    }

    /// `get(l, i) -> T`: bounds-checked element read.
    fn builtin_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, index]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("get", 2, args.len()))?;
        let values = expect_list("get", list)?;
        let index = expect_i64("get", index)?;
        if index < 0 || index as usize >= values.len() {
            return Err(RuntimeError::new(
                "L0413",
                format!("list index `{index}` is out of bounds"),
            ));
        }
        Ok(values[index as usize].clone())
    }

    /// `set(l, i, x) -> list<T>`: a new list with index `i` replaced by `x`.
    fn builtin_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, index, value]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("set", 3, args.len()))?;
        let mut values = expect_list("set", list)?;
        let index = expect_i64("set", index)?;
        if index < 0 || index as usize >= values.len() {
            return Err(RuntimeError::new(
                "L0413",
                format!("list index `{index}` is out of bounds"),
            ));
        }
        values[index as usize] = value;
        Ok(Value::Array((values).into()))
    }

    /// `pop(l) -> list<T>`: a new list without the last element.
    fn builtin_pop(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("pop", 1, args.len()))?;
        let mut values = expect_list("pop", list)?;
        if values.pop().is_none() {
            return Err(RuntimeError::new("L0413", "cannot pop from an empty list"));
        }
        Ok(Value::Array((values).into()))
    }

    /// `list_index_of(l, x) -> i64`: index of the first element equal to `x`, or -1.
    fn builtin_list_index_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, target]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_index_of", 2, args.len()))?;
        let values = expect_list("list_index_of", list)?;
        let index = values
            .iter()
            .position(|value| *value == target)
            .map(|i| i as i64)
            .unwrap_or(-1);
        Ok(Value::I64(index))
    }

    /// `list_contains(l, x) -> bool`: whether any element equals `x`.
    fn builtin_list_contains(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, target]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_contains", 2, args.len()))?;
        let values = expect_list("list_contains", list)?;
        Ok(Value::Bool(values.contains(&target)))
    }

    /// `reverse(l) -> list<T>`: a new list with the elements reversed.
    fn builtin_reverse(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("reverse", 1, args.len()))?;
        let mut values = expect_list("reverse", list)?;
        values.reverse();
        Ok(Value::Array((values).into()))
    }

    /// `sort(l list<i64>) -> list<i64>`: a new list sorted ascending.
    fn builtin_sort(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sort", 1, args.len()))?;
        let values = expect_list("sort", list)?;
        sort_scalar_list("sort", values)
    }

    /// `sort_by(l list<T>, cmp fn(T, T) -> i64) -> list<T>`: return a new list
    /// sorted by the comparator (`cmp(a, b)` negative if `a` precedes `b`, zero
    /// if equal, positive if after). Uses a stable sort, so equal elements keep
    /// their input order. The comparator's error, if any, is propagated. Mirrors
    /// the AST runtime.
    fn builtin_sort_by(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, callee]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sort_by", 2, args.len()))?;
        let mut values = expect_list("sort_by", list)?;
        // A comparator error must abort the whole sort, so capture the first
        // error out of band; `sort_by` itself cannot propagate `Result`.
        let mut error: Option<RuntimeError> = None;
        values.sort_by(|a, b| {
            if error.is_some() {
                return std::cmp::Ordering::Equal;
            }
            match self.invoke_callable("sort_by", callee.clone(), vec![a.clone(), b.clone()]) {
                Ok(Value::I64(n)) => n.cmp(&0),
                Ok(other) => {
                    error = Some(RuntimeError::new(
                        "L0417",
                        format!("sort_by comparator must return i64 but returned `{other}`"),
                    ));
                    std::cmp::Ordering::Equal
                }
                Err(err) => {
                    error = Some(err);
                    std::cmp::Ordering::Equal
                }
            }
        });
        if let Some(err) = error {
            return Err(err);
        }
        Ok(Value::Array((values).into()))
    }

    /// `concat(a, b) -> list<T>`: a new list with `b`'s elements appended to `a`.
    fn builtin_concat(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [a, b]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("concat", 2, args.len()))?;
        let mut values = expect_list("concat", a)?;
        let mut rest = expect_list("concat", b)?;
        values.append(&mut rest);
        Ok(Value::Array((values).into()))
    }

    /// `slice(l, start, end) -> list<T>`: the half-open range `[start, end)`,
    /// with `start`/`end` clamped into `[0, len]` (so it is always total).
    fn builtin_slice(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, start, end]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("slice", 3, args.len()))?;
        let values = expect_list("slice", list)?;
        let start = expect_i64("slice", start)?;
        let end = expect_i64("slice", end)?;
        let len = values.len() as i64;
        let start = start.clamp(0, len) as usize;
        let end = end.clamp(0, len) as usize;
        if start >= end {
            return Ok(Value::Array((Vec::new()).into()));
        }
        Ok(Value::Array((values[start..end].to_vec()).into()))
    }

    /// Invoke a first-class function value (`Value::Func` name or a capturing
    /// `Value::Closure`) with `args`, reusing the same call/closure machinery as
    /// direct dispatch and `parallel_map`. Shared by the higher-order list
    /// builtins so closures capture correctly. Mirrors the AST runtime.
    fn invoke_callable(
        &mut self,
        builtin: &str,
        callee: Value,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        match callee {
            Value::Func(name) => self.call_function(&name, args),
            Value::Closure(closure) => self.invoke_closure(&closure, args),
            other => Err(RuntimeError::new(
                "L0417",
                format!("{builtin} expects a function but got `{other}`"),
            )),
        }
    }

    /// `list_map(l list<T>, f fn(T) -> U) -> list<U>`: apply `f` to each element
    /// in order, collecting the mapped values into a new list.
    fn builtin_list_map(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, callee]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_map", 2, args.len()))?;
        let values = expect_list("list_map", list)?;
        let mut mapped = Vec::with_capacity(values.len());
        for value in values {
            mapped.push(self.invoke_callable("list_map", callee.clone(), vec![value])?);
        }
        Ok(Value::Array((mapped).into()))
    }

    /// `list_filter(l list<T>, pred fn(T) -> bool) -> list<T>`: keep the elements
    /// for which `pred` returns `true`, preserving input order.
    fn builtin_list_filter(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, callee]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_filter", 2, args.len()))?;
        let values = expect_list("list_filter", list)?;
        let mut kept = Vec::new();
        for value in values {
            let keep = self.invoke_callable("list_filter", callee.clone(), vec![value.clone()])?;
            if keep.as_bool()? {
                kept.push(value);
            }
        }
        Ok(Value::Array((kept).into()))
    }

    /// `list_reduce(l list<T>, init U, f fn(U, T) -> U) -> U`: a left fold,
    /// threading the accumulator (starting at `init`) through `f(acc, element)`.
    fn builtin_list_reduce(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, init, callee]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_reduce", 3, args.len()))?;
        let values = expect_list("list_reduce", list)?;
        let mut acc = init;
        for value in values {
            acc = self.invoke_callable("list_reduce", callee.clone(), vec![acc, value])?;
        }
        Ok(acc)
    }

    /// `map_new() -> map<K, V>`: a fresh empty map.
    fn builtin_map_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_new", 0, args.len()))?;
        Ok(Value::Map(Box::default()))
    }

    /// `map_set(m, k, v) -> map<K, V>`: a new map with `k` mapped to `v`.
    /// Overwriting an existing key or appending a new one is O(1).
    fn builtin_map_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key, value]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_set", 3, args.len()))?;
        let mut entries = expect_map("map_set", map)?;
        entries.insert(key, value);
        Ok(Value::Map(Box::new(entries)))
    }

    /// `map_get(m, k) -> option<V>`: `some(v)` if present, else `none`. O(1).
    fn builtin_map_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_get", 2, args.len()))?;
        let entries = expect_map("map_get", map)?;
        let found = entries.get(&key).cloned();
        Ok(option_value(found))
    }

    /// `map_has(m, k) -> bool`. O(1).
    fn builtin_map_has(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_has", 2, args.len()))?;
        let entries = expect_map("map_has", map)?;
        Ok(Value::Bool(entries.contains_key(&key)))
    }

    /// `map_len(m) -> i64`.
    fn builtin_map_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_len", 1, args.len()))?;
        let entries = expect_map("map_len", map)?;
        Ok(Value::I64(entries.len() as i64))
    }

    /// `map_keys(m) -> list<K>`: the keys in insertion order.
    fn builtin_map_keys(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_keys", 1, args.len()))?;
        let entries = expect_map("map_keys", map)?;
        Ok(Value::Array(
            entries.into_entries().into_iter().map(|(k, _)| k).collect(),
        ))
    }

    /// `map_values(m) -> list<V>`: the values in insertion order.
    fn builtin_map_values(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_values", 1, args.len()))?;
        let entries = expect_map("map_values", map)?;
        Ok(Value::Array(
            entries.into_entries().into_iter().map(|(_, v)| v).collect(),
        ))
    }

    /// `map_del(m, k) -> map<K, V>`: a new map without key `k`.
    fn builtin_map_del(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_del", 2, args.len()))?;
        let mut entries = expect_map("map_del", map)?;
        entries.remove(&key);
        Ok(Value::Map(Box::new(entries)))
    }

    fn builtin_substring(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, start, end]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("substring", 3, args.len()))?;
        let text = expect_string("substring", text)?;
        let start = expect_i64("substring", start)?;
        let end = expect_i64("substring", end)?;
        let chars: Vec<char> = text.chars().collect();
        let count = chars.len() as i64;
        if start < 0 || end < 0 || start > end || end > count {
            return Err(RuntimeError::new(
                "L0413",
                format!(
                    "substring range [{start}, {end}) is out of bounds for a string of length {count}"
                ),
            ));
        }
        let slice: String = chars[start as usize..end as usize].iter().collect();
        Ok(Value::String((slice).into()))
    }

    fn builtin_find(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, needle]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("find", 2, args.len()))?;
        let text = expect_string("find", text)?;
        let needle = expect_string("find", needle)?;
        Ok(Value::I64(char_find(&text, &needle)))
    }

    fn builtin_contains(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, needle]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("contains", 2, args.len()))?;
        let text = expect_string("contains", text)?;
        let needle = expect_string("contains", needle)?;
        Ok(Value::Bool(text.contains(&needle)))
    }

    fn builtin_starts_with(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, prefix]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("starts_with", 2, args.len()))?;
        let text = expect_string("starts_with", text)?;
        let prefix = expect_string("starts_with", prefix)?;
        Ok(Value::Bool(text.starts_with(&prefix)))
    }

    fn builtin_ends_with(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, suffix]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("ends_with", 2, args.len()))?;
        let text = expect_string("ends_with", text)?;
        let suffix = expect_string("ends_with", suffix)?;
        Ok(Value::Bool(text.ends_with(&suffix)))
    }

    fn builtin_repeat(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, count]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("repeat", 2, args.len()))?;
        let text = expect_string("repeat", text)?;
        let count = expect_i64("repeat", count)?;
        let result = if count <= 0 {
            String::new()
        } else {
            text.repeat(count as usize)
        };
        Ok(Value::String((result).into()))
    }

    fn builtin_split(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, sep]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("split", 2, args.len()))?;
        let text = expect_string("split", text)?;
        let sep = expect_string("split", sep)?;
        if sep.is_empty() {
            return Err(RuntimeError::new(
                "L0417",
                "split requires a non-empty separator".to_string(),
            ));
        }
        let parts = text
            .split(sep.as_str())
            .map(|part| Value::String((part.to_string()).into()))
            .collect();
        Ok(Value::Array(parts))
    }

    fn builtin_words(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("words", 1, args.len()))?;
        let text = expect_string("words", text)?;
        let parts = text
            .split_whitespace()
            .map(|part| Value::String((part.to_string()).into()))
            .collect();
        Ok(Value::Array(parts))
    }

    fn builtin_count(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, sub]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("count", 2, args.len()))?;
        let text = expect_string("count", text)?;
        let sub = expect_string("count", sub)?;
        // An empty needle has no well-defined non-overlapping count; define it as 0.
        let n = if sub.is_empty() {
            0
        } else {
            text.matches(sub.as_str()).count() as i64
        };
        Ok(Value::I64(n))
    }

    fn builtin_join(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [parts, sep]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("join", 2, args.len()))?;
        let Value::Array(parts) = parts else {
            return Err(RuntimeError::new(
                "L0417",
                format!("join expects an array of strings but got `{parts}`"),
            ));
        };
        let sep = expect_string("join", sep)?;
        let mut pieces = Vec::with_capacity(parts.len());
        for part in parts {
            pieces.push(expect_string("join", part)?);
        }
        Ok(Value::String((pieces.join(sep.as_str())).into()))
    }

    fn builtin_trim(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("trim", 1, args.len()))?;
        let text = expect_string("trim", text)?;
        Ok(Value::String(
            (text
                .trim_matches(|c: char| c.is_ascii_whitespace())
                .to_string())
            .into(),
        ))
    }

    fn builtin_replace(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, from, to]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("replace", 3, args.len()))?;
        let text = expect_string("replace", text)?;
        let from = expect_string("replace", from)?;
        let to = expect_string("replace", to)?;
        if from.is_empty() {
            return Err(RuntimeError::new(
                "L0417",
                "replace requires a non-empty `from` pattern".to_string(),
            ));
        }
        Ok(Value::String(
            (text.replace(from.as_str(), to.as_str())).into(),
        ))
    }

    fn builtin_upper(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("upper", 1, args.len()))?;
        let text = expect_string("upper", text)?;
        Ok(Value::String((text.to_uppercase()).into()))
    }

    /// `chars(s) -> list<char>`: the characters of `s` in order.
    fn builtin_chars(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("chars", 1, args.len()))?;
        let text = expect_string("chars", text)?;
        Ok(Value::Array(text.chars().map(Value::Char).collect()))
    }

    /// `string_from_chars(cs) -> string`: concatenate a `list<char>` into a string.
    fn builtin_string_from_chars(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("string_from_chars", 1, args.len()))?;
        let values = expect_list("string_from_chars", list)?;
        let mut out = String::new();
        for value in values {
            match value {
                Value::Char(c) => out.push(c),
                other => {
                    return Err(RuntimeError::new(
                        "L0417",
                        format!("string_from_chars expects a list<char> but found `{other}`"),
                    ));
                }
            }
        }
        Ok(Value::String((out).into()))
    }

    fn builtin_lower(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("lower", 1, args.len()))?;
        let text = expect_string("lower", text)?;
        Ok(Value::String((text.to_lowercase()).into()))
    }

    /// `to_bytes(s string) -> list<byte>`: the UTF-8 encoding of `s` as a
    /// `list<byte>` (a `Value::Array` of `Value::Byte`, matching `read_bytes`).
    fn builtin_to_bytes(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_bytes", 1, args.len()))?;
        let text = expect_string("to_bytes", text)?;
        Ok(Value::Array(
            text.into_bytes().into_iter().map(Value::Byte).collect(),
        ))
    }

    /// `from_bytes(b list<byte>) -> result<string, string>`: decode `b` as UTF-8,
    /// returning `ok(s)` on success and `err(message)` (never a panic, never a
    /// lossy replacement) on invalid UTF-8.
    fn builtin_from_bytes(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [data]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("from_bytes", 1, args.len()))?;
        let bytes = Self::value_to_bytes("from_bytes", data)?;
        Ok(result_value(match String::from_utf8(bytes) {
            Ok(text) => Ok(Value::String((text).into())),
            Err(error) => Err(Value::String(format!("invalid utf-8: {error}").into())),
        }))
    }

    /// `byte_len(s string) -> i64`: the number of UTF-8 bytes in `s` (distinct
    /// from `len`, which counts characters for a string).
    fn builtin_byte_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("byte_len", 1, args.len()))?;
        let text = expect_string("byte_len", text)?;
        Ok(Value::I64(text.len() as i64))
    }

    /// `parse_i64(s string) -> result<i64, string>`: parse `s` as a base-10
    /// signed 64-bit integer via Rust `str::parse::<i64>()`, returning `ok(n)`
    /// on success and `err(message)` on any failure (empty, non-numeric, or out
    /// of range). Whitespace is not trimmed, so a padded string is an `err`. The
    /// error message is a fixed string so every backend matches byte-for-byte.
    fn builtin_parse_i64(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("parse_i64", 1, args.len()))?;
        let text = expect_string("parse_i64", text)?;
        Ok(result_value(match text.parse::<i64>() {
            Ok(value) => Ok(Value::I64(value)),
            Err(_) => Err(Value::String(
                format!("cannot parse `{text}` as i64").into(),
            )),
        }))
    }

    /// `parse_f64(s string) -> result<f64, string>`: parse `s` as an `f64` via
    /// Rust `str::parse::<f64>()`, returning `ok(x)` on success and
    /// `err(message)` on failure. The error message is a fixed string so every
    /// backend matches byte-for-byte.
    fn builtin_parse_f64(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("parse_f64", 1, args.len()))?;
        let text = expect_string("parse_f64", text)?;
        Ok(result_value(match text.parse::<f64>() {
            Ok(value) => Ok(Value::F64(value)),
            Err(_) => Err(Value::String(
                format!("cannot parse `{text}` as f64").into(),
            )),
        }))
    }

    fn builtin_abs(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("abs", 1, args.len()))?;
        match value {
            Value::I64(n) => Ok(Value::I64(n.abs())),
            Value::F64(n) => Ok(Value::F64(n.abs())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("abs expects an i64 or f64 but got `{other}`"),
            )),
        }
    }

    /// `clamp(x, lo, hi) -> T`: `x` limited to `[lo, hi]`; total (for `lo > hi`
    /// yields `lo`, for f64 NaN `x` returns `x`). Mirrors the AST interpreter.
    fn builtin_clamp(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [x, lo, hi]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("clamp", 3, args.len()))?;
        match (x, lo, hi) {
            (Value::I64(x), Value::I64(lo), Value::I64(hi)) => Ok(Value::I64(if x < lo {
                lo
            } else if x > hi {
                hi
            } else {
                x
            })),
            (Value::F64(x), Value::F64(lo), Value::F64(hi)) => Ok(Value::F64(if x < lo {
                lo
            } else if x > hi {
                hi
            } else {
                x
            })),
            (x, lo, hi) => Err(RuntimeError::new(
                "L0417",
                format!(
                    "clamp expects three matching i64 or f64 values but got `{x}`, `{lo}`, and `{hi}`"
                ),
            )),
        }
    }

    /// `sign(x) -> i64`: `-1`/`0`/`1`; f64 `NaN`/`-0.0` map to `0`.
    fn builtin_sign(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sign", 1, args.len()))?;
        match value {
            Value::I64(n) => Ok(Value::I64(n.signum())),
            Value::F64(n) => Ok(Value::I64(if n > 0.0 {
                1
            } else if n < 0.0 {
                -1
            } else {
                0
            })),
            other => Err(RuntimeError::new(
                "L0417",
                format!("sign expects an i64 or f64 but got `{other}`"),
            )),
        }
    }

    /// `gcd(a, b) -> i64`: non-negative greatest common divisor (total at
    /// `i64::MIN`; see `gcd_i64`).
    fn builtin_gcd(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [a, b]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("gcd", 2, args.len()))?;
        match (a, b) {
            (Value::I64(a), Value::I64(b)) => Ok(Value::I64(gcd_i64(a, b))),
            (a, b) => Err(RuntimeError::new(
                "L0417",
                format!("gcd expects two i64 values but got `{a}` and `{b}`"),
            )),
        }
    }

    /// `list_sum(l) -> T`: wrapping i64 / f64 sum; empty -> `0`/`0.0`.
    fn builtin_list_sum(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_sum", 1, args.len()))?;
        let values = expect_list("list_sum", list)?;
        list_sum_values("list_sum", values)
    }

    /// `list_min(l) -> option<T>`: `none` on empty, else `some(minimum)`.
    fn builtin_list_min(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_min", 1, args.len()))?;
        let values = expect_list("list_min", list)?;
        Ok(option_value(list_extreme("list_min", values, false)?))
    }

    /// `list_max(l) -> option<T>`: `none` on empty, else `some(maximum)`.
    fn builtin_list_max(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_max", 1, args.len()))?;
        let values = expect_list("list_max", list)?;
        Ok(option_value(list_extreme("list_max", values, true)?))
    }

    fn builtin_min(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [left, right]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("min", 2, args.len()))?;
        match (left, right) {
            (Value::I64(a), Value::I64(b)) => Ok(Value::I64(a.min(b))),
            (Value::F64(a), Value::F64(b)) => Ok(Value::F64(a.min(b))),
            (a, b) => Err(RuntimeError::new(
                "L0417",
                format!("min expects two matching i64 or f64 values but got `{a}` and `{b}`"),
            )),
        }
    }

    fn builtin_max(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [left, right]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("max", 2, args.len()))?;
        match (left, right) {
            (Value::I64(a), Value::I64(b)) => Ok(Value::I64(a.max(b))),
            (Value::F64(a), Value::F64(b)) => Ok(Value::F64(a.max(b))),
            (a, b) => Err(RuntimeError::new(
                "L0417",
                format!("max expects two matching i64 or f64 values but got `{a}` and `{b}`"),
            )),
        }
    }

    fn builtin_pow(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [base, exp]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("pow", 2, args.len()))?;
        match (base, exp) {
            (Value::I64(b), Value::I64(e)) => {
                if e < 0 {
                    return Err(RuntimeError::new(
                        "L0417",
                        format!("pow expects a non-negative integer exponent but got `{e}`"),
                    ));
                }
                Ok(Value::I64(b.pow(e as u32)))
            }
            (Value::F64(b), Value::F64(e)) => Ok(Value::F64(b.powf(e))),
            (b, e) => Err(RuntimeError::new(
                "L0417",
                format!("pow expects two matching i64 or f64 values but got `{b}` and `{e}`"),
            )),
        }
    }

    fn builtin_sqrt(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sqrt", 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(n.sqrt())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("sqrt expects an f64 but got `{other}`"),
            )),
        }
    }

    /// Shared implementation for the unary `f64 -> f64` math builtins, matching
    /// the AST runtime so every backend produces bit-identical results.
    fn builtin_unary_f64(
        name: &str,
        args: Vec<Value>,
        op: fn(f64) -> f64,
    ) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(op(n))),
            other => Err(RuntimeError::new(
                "L0417",
                format!("{name} expects an f64 but got `{other}`"),
            )),
        }
    }

    /// `atan2(y, x)`: the angle of the vector `(x, y)` in radians.
    fn builtin_atan2(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [y, x]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atan2", 2, args.len()))?;
        match (y, x) {
            (Value::F64(y), Value::F64(x)) => Ok(Value::F64(y.atan2(x))),
            (y, x) => Err(RuntimeError::new(
                "L0417",
                format!("atan2 expects two f64 values but got `{y}` and `{x}`"),
            )),
        }
    }

    /// `rotate_left(x, n)`: rotate the 64 bits of `x` left by `(n & 63)`
    /// positions, matching the AST runtime so every backend agrees.
    fn builtin_rotate_left(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [x, n]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rotate_left", 2, args.len()))?;
        match (x, n) {
            (Value::I64(x), Value::I64(n)) => {
                Ok(Value::I64(x.rotate_left(((n as u64) & 63) as u32)))
            }
            (x, n) => Err(RuntimeError::new(
                "L0417",
                format!("rotate_left expects two i64 values but got `{x}` and `{n}`"),
            )),
        }
    }

    /// `rotate_right(x, n)`: rotate the 64 bits of `x` right by `(n & 63)`
    /// positions, matching the AST runtime.
    fn builtin_rotate_right(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [x, n]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rotate_right", 2, args.len()))?;
        match (x, n) {
            (Value::I64(x), Value::I64(n)) => {
                Ok(Value::I64(x.rotate_right(((n as u64) & 63) as u32)))
            }
            (x, n) => Err(RuntimeError::new(
                "L0417",
                format!("rotate_right expects two i64 values but got `{x}` and `{n}`"),
            )),
        }
    }

    /// `count_ones(x)`: population count of the 64-bit value `x` (0..=64).
    fn builtin_count_ones(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("count_ones", 1, args.len()))?;
        match value {
            Value::I64(x) => Ok(Value::I64(x.count_ones() as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("count_ones expects an i64 but got `{other}`"),
            )),
        }
    }

    /// `leading_zeros(x)`: number of leading zero bits in `x` (0..=64).
    fn builtin_leading_zeros(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("leading_zeros", 1, args.len()))?;
        match value {
            Value::I64(x) => Ok(Value::I64(x.leading_zeros() as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("leading_zeros expects an i64 but got `{other}`"),
            )),
        }
    }

    /// `trailing_zeros(x)`: number of trailing zero bits in `x` (0..=64).
    fn builtin_trailing_zeros(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("trailing_zeros", 1, args.len()))?;
        match value {
            Value::I64(x) => Ok(Value::I64(x.trailing_zeros() as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("trailing_zeros expects an i64 but got `{other}`"),
            )),
        }
    }

    /// `reverse_bytes(x)`: reverse the byte order of the 64-bit value `x`.
    fn builtin_reverse_bytes(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("reverse_bytes", 1, args.len()))?;
        match value {
            Value::I64(x) => Ok(Value::I64(x.swap_bytes())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("reverse_bytes expects an i64 but got `{other}`"),
            )),
        }
    }

    fn builtin_floor(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("floor", 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(n.floor())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("floor expects an f64 but got `{other}`"),
            )),
        }
    }

    fn builtin_ceil(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("ceil", 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(n.ceil())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("ceil expects an f64 but got `{other}`"),
            )),
        }
    }

    fn builtin_round(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("round", 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(n.round())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("round expects an f64 but got `{other}`"),
            )),
        }
    }

    fn builtin_rc_new(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_new", 1, args.len()))?;
        self.heap.push(Some(value));
        let slot = self.heap.len() - 1;
        self.refcounts.insert(slot, 1);
        Ok(Value::Ptr(slot))
    }

    fn builtin_rc_clone(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_clone", 1, args.len()))?;
        let slot = handle.as_ptr()?;
        match self.refcounts.get_mut(&slot) {
            Some(count) => {
                *count += 1;
                Ok(Value::Ptr(slot))
            }
            None => Err(RuntimeError::new(
                "L0406",
                format!("invalid reference-counted handle `{slot}`"),
            )),
        }
    }

    fn builtin_rc_release(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_release", 1, args.len()))?;
        let slot = handle.as_ptr()?;
        match self.refcounts.get_mut(&slot) {
            Some(count) => {
                *count -= 1;
                if *count == 0 {
                    self.refcounts.remove(&slot);
                    if let Some(target) = self.heap.get_mut(slot) {
                        *target = None;
                    }
                }
                Ok(Value::Void)
            }
            None => Err(RuntimeError::new(
                "L0406",
                format!("invalid reference-counted handle `{slot}`"),
            )),
        }
    }

    fn builtin_rc_borrow(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_borrow", 1, args.len()))?;
        let slot = handle.as_ptr()?;
        if self.refcounts.contains_key(&slot) {
            Ok(Value::Ptr(slot))
        } else {
            Err(RuntimeError::new(
                "L0406",
                format!("invalid reference-counted handle `{slot}`"),
            ))
        }
    }

    fn builtin_ref_get(&self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        let slot = handle.as_ptr()?;
        self.heap
            .get(slot)
            .and_then(|value| value.clone())
            .ok_or_else(|| RuntimeError::new("L0406", format!("invalid pointer `{slot}`")))
    }

    fn wrong_arity(name: &str, expected: usize, actual: usize) -> RuntimeError {
        RuntimeError::new(
            "L0405",
            format!("function `{name}` expects {expected} arguments but got {actual}"),
        )
    }
}
