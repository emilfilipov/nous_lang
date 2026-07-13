//! The second half of the semantic checker's `impl Checker` (call checking,
//! argument-type inference, and the builtin-signature dispatch). Split out of
//! lib.rs as a separate impl block; sees the checker's types via `use super::*`.

use super::*;

impl<'a> Checker<'a> {
    /// call expression (from a `let` annotation, a `return`, or an enclosing
    /// call's parameter type). It is used for argument-position inference: a
    /// collection-growing builtin (`push`/`set`/`pop`/`map_set`/`map_del`) whose
    /// result type equals the container type propagates `expected` into its
    /// container argument, so a nested `list_new()`/`map_new()` there infers its
    /// element/key/value types; the resolved element/key/value type is then
    /// propagated into the value arguments so a nested `none`/`ok`/`err` infers
    /// too. User-function calls propagate each concrete parameter type similarly.
    pub(crate) fn check_call(
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
    pub(crate) fn check_trait_method_call(
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
    pub(crate) fn type_param_has_bound(
        &self,
        function: &Function,
        type_name: &str,
        trait_name: &str,
    ) -> bool {
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
    pub(crate) fn check_generic_call(
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
    pub(crate) fn resolve_field_path(
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

    pub(crate) fn check_struct_construction(
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
    pub(crate) fn check_enum_construction(
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
    pub(crate) fn match_variants(&self, ty: &TypeRef) -> Option<(String, Vec<EnumVariant>)> {
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

    pub(crate) fn check_match(
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
    pub(crate) fn check_struct_literal(
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
    pub(crate) fn expect_reference(
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
    pub(crate) fn expect_list_arg(
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
    pub(crate) fn expect_fn_arg(
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
    pub(crate) fn expect_map_arg(
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
    pub(crate) fn type_has_layout(&self, ty: &TypeRef) -> bool {
        self.type_layout_ok(ty, &mut Vec::new())
    }

    pub(crate) fn type_layout_ok(&self, ty: &TypeRef, stack: &mut Vec<String>) -> bool {
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

    pub(crate) fn expect_raw_pointer(
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
    pub(crate) fn check_asm(&mut self, bytes: &[i64], span: Span, function: &Function) {
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
    pub(crate) fn require_unsafe(
        &mut self,
        name: &str,
        span: Span,
        function: &Function,
    ) -> Option<()> {
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

    pub(crate) fn expect_arg_count(
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

    pub(crate) fn expect_arg_type(
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
    pub(crate) fn expect_fs_arg_count(
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
    pub(crate) fn expect_fs_arg_type(
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
    pub(crate) fn expect_socket_arg_count(
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
    pub(crate) fn expect_concurrency_arity(
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
    pub(crate) fn expect_concurrency_arg(
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
    pub(crate) fn check_ordering_arg(
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
    pub(crate) fn expect_socket_arg_type(
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
    pub(crate) fn expect_http_arg_count(
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
    pub(crate) fn expect_http_arg_type(
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
    pub(crate) fn expect_scalar_builtin_arg(
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
    pub(crate) fn expect_process_arg_count(
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
    pub(crate) fn expect_process_arg(
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
    pub(crate) fn expect_string_builtin_arg(
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
