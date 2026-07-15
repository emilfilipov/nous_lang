//! The second half of the AST interpreter's `impl Runtime`: block/match/expr
//! evaluation and place resolution. Split out of lib.rs as a separate impl block;
//! sees the interpreter's types via `use super::*`.

use super::*;

impl<'a> Runtime<'a> {
    pub(crate) fn eval_scoped_block(
        &mut self,
        statements: &[Stmt],
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        env.push_scope();
        let result = self.eval_block(statements, env);
        env.pop_scope();
        result
    }

    /// Evaluate a `match`: select the arm whose variant matches the scrutinee's
    /// enum value (or the `_` wildcard), bind payload values to the arm's locals
    /// in a child scope, and evaluate the arm block. Exhaustiveness is enforced
    /// at compile time, so a valid program always selects an arm.
    pub(crate) fn eval_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
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
                MatchPattern::Wildcard => {
                    return self.eval_scoped_block(&arm.body, env);
                }
                MatchPattern::Variant { name, bindings } if name == &variant => {
                    env.push_scope();
                    for (binding, value) in bindings.iter().zip(payload.iter()) {
                        env.define(binding.clone(), value.clone());
                    }
                    let result = self.eval_block(&arm.body, env);
                    env.pop_scope();
                    return result;
                }
                MatchPattern::Variant { .. } => {}
            }
        }
        Err(RuntimeError::new(
            "L0384",
            format!("no match arm covered variant `{variant}`"),
        ))
    }

    /// Resolve a parser assignment path into concrete places, evaluating each
    /// array index expression against the current environment.
    pub(crate) fn resolve_places(
        &mut self,
        path: &[Place],
        env: &Env,
    ) -> Result<Vec<ResolvedPlace>, RuntimeError> {
        path.iter()
            .map(|place| match place {
                Place::Field(field) => Ok(ResolvedPlace::Field(field.clone())),
                Place::Index(expr) => {
                    Ok(ResolvedPlace::Index(self.eval_expr(expr, env)?.as_i64()?))
                }
            })
            .collect()
    }

    pub(crate) fn eval_expr(&mut self, expr: &Expr, env: &Env) -> Result<Value, RuntimeError> {
        let result = match &expr.kind {
            ExprKind::Field { target, field } => {
                // Fast path: borrow a bare-variable struct and clone only the field
                // read, instead of cloning the whole struct on every `s.field`.
                if let ExprKind::Variable(name) = &target.kind
                    && let Some(Value::Struct(s)) = env.get_ref(name)
                {
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
            ExprKind::Integer(value) => Ok(Value::I64(*value)),
            ExprKind::Float(value) => Ok(Value::F64(*value)),
            ExprKind::Bool(value) => Ok(Value::Bool(*value)),
            ExprKind::String(value) => Ok(Value::String((value.clone()).into())),
            ExprKind::Char(value) => Ok(Value::Char(*value)),
            ExprKind::Array(values) => values
                .iter()
                .map(|value| self.eval_expr(value, env))
                .collect::<Result<Vec<_>, _>>()
                .map(|v| Value::Array(v.into())),
            ExprKind::Variable(name) => match env.get(name) {
                Ok(value) => Ok(value),
                Err(error) => {
                    // A bare name that is not a local but is a known enum variant
                    // constructs a unit variant.
                    if let Some(enum_name) = self.variants.get(name.as_str()) {
                        Ok(Value::Enum(Box::new(EnumValue {
                            enum_name: enum_name.to_string(),
                            variant: name.clone(),
                            payload: Vec::new(),
                        })))
                    } else if self.functions.contains_key(name.as_str()) {
                        // A bare name that is a known top-level function evaluates
                        // to a first-class function value.
                        Ok(Value::Func((name.clone()).into()))
                    } else {
                        Err(error)
                    }
                }
            },
            ExprKind::Index { target, index } => {
                // Fast path: when the target is a bare variable, borrow the
                // container and clone only the element read, instead of cloning the
                // whole array/string on every `a[i]` (which is O(len) per access).
                if let ExprKind::Variable(name) = &target.kind {
                    let idx = self.eval_expr(index, env)?.as_i64()?;
                    if let Some(container) = env.get_ref(name) {
                        return index_into(container, idx);
                    }
                    // A bare name that is not a local (enum variant / function
                    // value) is not indexable; evaluate it and let `index_into`
                    // report the type error on the owned value.
                    let owned = self.eval_expr(target, env)?;
                    return index_into(&owned, idx);
                }
                // Computed target: preserve target-before-index evaluation order.
                let target = self.eval_expr(target, env)?;
                let index = self.eval_expr(index, env)?.as_i64()?;
                index_into(&target, index)
            }
            ExprKind::Unary { op, expr } => {
                let value = self.eval_expr(expr, env)?;
                match op {
                    UnaryOp::Not => Ok(Value::Bool(!value.as_bool()?)),
                    // Bitwise NOT (one's complement). On a fixed-width integer the
                    // complement is re-normalized to the width.
                    UnaryOp::BitNot => match value {
                        Value::Int { value, ty } => Ok(Value::int(!value, ty)),
                        other => Ok(Value::I64(!other.as_i64()?)),
                    },
                    // Arithmetic negation, preserving the operand's numeric type.
                    // Integer negation wraps; float negation flips the sign bit.
                    UnaryOp::Negate => match value {
                        Value::Int { value, ty } => Ok(Value::int(value.wrapping_neg(), ty)),
                        Value::F64(f) => Ok(Value::F64(-f)),
                        Value::F32(f) => Ok(Value::F32(-f)),
                        other => Ok(Value::I64(other.as_i64()?.wrapping_neg())),
                    },
                }
            }
            ExprKind::Binary { left, op, right } => {
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
            ExprKind::Call { name, args } => {
                let values = args
                    .iter()
                    .map(|arg| self.eval_expr(arg, env))
                    .collect::<Result<Vec<_>, _>>()?;
                // Resolve the call target with a single borrowing lookup. A call
                // name bound to a closure value invokes that closure (binding its
                // captured snapshot then the arguments); a name bound to a
                // function value dispatches through it; otherwise `name` is a plain
                // top-level function / builtin / constructor and dispatches by
                // name. Using `get_ref` keeps the common case — an ordinary
                // top-level call, where `name` is not a local at all — free of the
                // clone and the discarded "unknown variable" error a bare
                // `env.get` allocates on every such call.
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
            ExprKind::Await { expr } => {
                let value = self.eval_expr(expr, env)?;
                match value {
                    // An actor request-reply future: drive the deterministic
                    // mailbox until the awaited reply slot is filled (or a
                    // deadlock is detected). See `await_actor_future`.
                    Value::ActorFuture(slot) => self.await_actor_future(slot, expr.span),
                    // A thread-spawned `async fn` future: join its OS thread.
                    other => {
                        let future = expect_future("await", other)?;
                        await_future(&future)
                    }
                }
            }
            // Postfix `EXPR?` error propagation. Evaluate the operand to an
            // `option`/`result` enum value; on the success variant (`some`/`ok`)
            // the expression is the payload, and on the failure variant
            // (`none`/`err`) we raise a function-level early-return signal that
            // carries the whole enum value. `invoke_function` (the call boundary)
            // catches the `L0430` sentinel and returns the stashed value as the
            // enclosing function's result — mirroring how `throw`'s `L0420`
            // unwinds to the nearest `try`/`catch`, but unwinding to the function
            // boundary instead. Semantics has already verified the operand is an
            // `option`/`result` and the return type is compatible.
            ExprKind::Try(inner) => {
                let value = self.eval_expr(inner, env)?;
                let Value::Enum(e) = &value else {
                    return Err(RuntimeError::new(
                        "L0428",
                        "`?` operand did not evaluate to an option/result value",
                    )
                    .with_span(expr.span));
                };
                let (variant, payload) = (&e.variant, &e.payload);
                match variant.as_str() {
                    "ok" | "some" => payload.first().cloned().ok_or_else(|| {
                        RuntimeError::new("L0428", format!("`{variant}` payload missing for `?`"))
                    }),
                    "err" | "none" => {
                        // Stash the whole failure value and unwind to the nearest
                        // function boundary via the `L0430` early-return sentinel.
                        self.pending_try_return = Some(value.clone());
                        Err(RuntimeError::new(
                            "L0430",
                            "`?` propagated a failure value to the enclosing function",
                        ))
                    }
                    other => Err(RuntimeError::new(
                        "L0428",
                        format!("`?` operand has unexpected variant `{other}`"),
                    )),
                }
            }
            ExprKind::StructLiteral { name, fields } => {
                // Evaluate in source order, then reorder to the declared field
                // order so the constructed value matches positional construction.
                let mut evaluated = Vec::with_capacity(fields.len());
                for (field_name, value) in fields {
                    evaluated.push((field_name.clone(), self.eval_expr(value, env)?));
                }
                let order = self.structs.get(name.as_str()).ok_or_else(|| {
                    RuntimeError::new("L0372", format!("`{name}` is not a struct type"))
                })?;
                let ordered = order
                    .iter()
                    .map(|declared| {
                        evaluated
                            .iter()
                            .find(|(n, _)| n == declared)
                            .map(|(_, v)| v.clone())
                            .ok_or_else(|| {
                                RuntimeError::new(
                                    "L0372",
                                    format!("missing field `{declared}` for `{name}`"),
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                self.call_function(name, ordered)
            }
            // `match` normally arrives as a statement and is handled in
            // `eval_statement`; this path covers a `match` nested as a value and
            // evaluates its selected arm to a plain value.
            ExprKind::Match { scrutinee, arms } => {
                let mut child = env.clone();
                match self.eval_match(scrutinee, arms, &mut child)? {
                    Control::Value(value) | Control::Return(value) => Ok(value),
                    Control::Break | Control::Continue => Err(RuntimeError::new(
                        "L0410",
                        "loop control escaped a match arm",
                    )),
                }
            }
            // Evaluating a closure literal snapshots the current environment's
            // in-scope locals by value and yields a backend-neutral
            // `Value::Closure` carrying the literal's parse-order `id` plus that
            // snapshot. The body is not stored here — it lives in `self.closures`,
            // keyed by `id`, and is looked up at invocation time.
            ExprKind::Closure { id, .. } => Ok(Value::Closure(Box::new(Closure {
                id: *id,
                captured: env.snapshot_locals(),
            }))),
            // Inline conditional `THEN if COND else ELSE`: evaluate the
            // condition, then evaluate exactly the taken branch. Semantics has
            // already verified `cond` is `bool` and the branch types agree.
            ExprKind::Conditional {
                cond,
                then_branch,
                else_branch,
            } => {
                if self.eval_expr(cond, env)?.as_bool()? {
                    self.eval_expr(then_branch, env)
                } else {
                    self.eval_expr(else_branch, env)
                }
            }
            // Membership `VALUE in COLLECTION`. Reuse the same builtins the IR
            // desugar targets so every backend agrees: `contains` for a string
            // (a char value is coerced to a one-char string) and
            // `list_contains` for a `list<T>`.
            ExprKind::In { value, collection } => {
                let coll = self.eval_expr(collection, env)?;
                let val = self.eval_expr(value, env)?;
                match &coll {
                    Value::String(_) => {
                        let needle = Value::String((val.as_concat_string()?).into());
                        Self::builtin_contains(vec![coll, needle])
                    }
                    _ => Self::builtin_list_contains(vec![coll, val]),
                }
            }
            // String slice `target[start:end]`. Reuse the `substring` builtin so
            // the range, bounds check, and char-based indexing match the IR
            // desugar exactly; an omitted `start` is `0` and an omitted `end` is
            // the string length.
            ExprKind::Slice { target, start, end } => {
                let target = self.eval_expr(target, env)?;
                let length = target.as_string()?.chars().count() as i64;
                let start = match start {
                    Some(start) => self.eval_expr(start, env)?,
                    None => Value::I64(0),
                };
                let end = match end {
                    Some(end) => self.eval_expr(end, env)?,
                    None => Value::I64(length),
                };
                Self::builtin_substring(vec![target, start, end])
            }
            // `spawn NAME(args)`: construct the actor, run its `init` with the
            // evaluated arguments, register it on the scheduler, and yield the
            // handle. Delegated to the actor scheduler module.
            ExprKind::Spawn { actor, args } => {
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(self.eval_expr(arg, env)?);
                }
                self.spawn_actor(actor, values)
            }
            // `tell`/`ask TARGET.HANDLER(args)`: evaluate the target handle and
            // the arguments, then dispatch. `tell` enqueues a fire-and-forget
            // message and returns `void`; `ask` enqueues a request carrying a
            // reply slot and returns a `Future<R>` (`Value::ActorFuture`) that
            // `await` resolves by driving the mailbox until the reply arrives.
            ExprKind::Tell {
                target,
                handler,
                args,
                is_ask,
            } => {
                let target = self.eval_expr(target, env)?;
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(self.eval_expr(arg, env)?);
                }
                if *is_ask {
                    self.ask_actor(target, handler, values)
                } else {
                    self.tell_actor(target, handler, values)
                }
            }
        };
        result.map_err(|error| self.annotate_error(error, expr.span))
    }

    pub(crate) fn annotate_error(&self, error: RuntimeError, span: Span) -> RuntimeError {
        let error = error.with_span(span);
        match self.call_stack.last() {
            Some(frame) => error
                .with_function(frame.function.to_string())
                .with_traceback(self.build_traceback()),
            None => error,
        }
    }

    /// Materialize the active call stack as owned [`TraceFrame`]s for a
    /// `RuntimeError`. Called only on the error path, so the per-frame name
    /// clone stays off the hot call path (the live `call_stack` borrows each
    /// name from the program).
    pub(crate) fn build_traceback(&self) -> Vec<TraceFrame> {
        self.call_stack
            .iter()
            .map(|frame| TraceFrame {
                function: frame.function.to_string(),
                span: frame.span,
            })
            .collect()
    }

    pub(crate) fn eval_binary(
        &self,
        left: Value,
        op: BinaryOp,
        right: Value,
    ) -> Result<Value, RuntimeError> {
        // Float arithmetic/comparison when both operands are f64 (IEEE 754
        // semantics: division by zero yields infinity/NaN, not an error).
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
        // 32-bit float arithmetic/comparison, IEEE 754 like f64. Storing a native
        // f32 rounds each result to f32 precision automatically.
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
        // Fixed-width integer arithmetic/comparison. Both operands carry the same
        // width/signedness tag (the type checker forbids mixing widths); the
        // arithmetic result is wrap-normalized back into that width, and ordering
        // reduces to plain `i64` comparison of the normalized cells (signed-
        // correct for `i32`, unsigned-correct for `u32`).
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
                // Bitwise ops operate on the normalized cell and re-normalize;
                // shifts mask the amount to the width and honor signedness.
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
            // `+` concatenates when both operands are strings; otherwise it adds i64s.
            BinaryOp::Add if matches!((&left, &right), (Value::String(_), Value::String(_))) => {
                // Reuse the left operand's heap buffer: `String + &str` is a
                // `push_str`, amortized O(1) when capacity allows, so building a
                // string with `s = s + piece` in a loop stays O(n) overall
                // instead of reallocating a fresh buffer on every concat.
                Ok(Value::String(
                    (left.into_string()? + &right.as_string()?).into(),
                ))
            }
            // `string + char` (either order) concatenates the char as a
            // one-character string.
            BinaryOp::Add
                if matches!(
                    (&left, &right),
                    (Value::String(_), Value::Char(_)) | (Value::Char(_), Value::String(_))
                ) =>
            {
                Ok(Value::String(
                    (left.as_concat_string()? + &right.as_concat_string()?).into(),
                ))
            }
            // Plain `i64` `+`/`-`/`*` wrap on overflow (two's complement), matching
            // the native backend (`add`/`sub`/`imul` keep the low 64 bits) and the
            // `/`/`%` cases below. Using `wrapping_*` rather than `+`/`-`/`*` keeps
            // the result deterministic across debug/release (a plain `*` panics on
            // overflow only in debug). Explicit overflow handling is the
            // `checked_*`/`saturating_*`/`overflowing_*` builtins.
            BinaryOp::Add => Ok(Value::I64(left.as_i64()?.wrapping_add(right.as_i64()?))),
            BinaryOp::Subtract => Ok(Value::I64(left.as_i64()?.wrapping_sub(right.as_i64()?))),
            BinaryOp::Multiply => Ok(Value::I64(left.as_i64()?.wrapping_mul(right.as_i64()?))),
            BinaryOp::Divide => {
                let divisor = right.as_i64()?;
                if divisor == 0 {
                    Err(RuntimeError::new("L0404", "division by zero"))
                } else {
                    // Wrap the one signed-overflow case (`i64::MIN / -1`) to
                    // `i64::MIN` rather than panicking, matching `int_div` and the
                    // native backend's guarded `idiv`.
                    Ok(Value::I64(left.as_i64()?.wrapping_div(divisor)))
                }
            }
            BinaryOp::Remainder => {
                let divisor = right.as_i64()?;
                if divisor == 0 {
                    Err(RuntimeError::new("L0404", "remainder by zero"))
                } else {
                    // `i64::MIN % -1` is 0; `wrapping_rem` yields it without the
                    // overflow panic, matching the native backend's guarded `idiv`.
                    Ok(Value::I64(left.as_i64()?.wrapping_rem(divisor)))
                }
            }
            BinaryOp::Equal => Ok(Value::Bool(left == right)),
            BinaryOp::NotEqual => Ok(Value::Bool(left != right)),
            // String ordering is lexicographic by Unicode code point (Rust's
            // `str` Ord over UTF-8 bytes, which is code-point order).
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
            // Integer bitwise ops on two i64s. Shift amounts are masked to the
            // low 6 bits (`amount & 63`), so large/negative shifts are total and
            // deterministic (x86/Java `long` semantics) — never a runtime error.
            // `>>` is an arithmetic (sign-preserving) shift on the signed i64.
            BinaryOp::BitAnd => Ok(Value::I64(left.as_i64()? & right.as_i64()?)),
            BinaryOp::BitOr => Ok(Value::I64(left.as_i64()? | right.as_i64()?)),
            BinaryOp::BitXor => Ok(Value::I64(left.as_i64()? ^ right.as_i64()?)),
            BinaryOp::Shl => Ok(Value::I64(shift_left(left.as_i64()?, right.as_i64()?))),
            BinaryOp::Shr => Ok(Value::I64(shift_right(left.as_i64()?, right.as_i64()?))),
            BinaryOp::And | BinaryOp::Or => unreachable!("logical ops short-circuit in eval_expr"),
        }
    }

    pub(crate) fn builtin_alloc(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("alloc", 1, args.len()))?;
        self.heap.push(Some(value));
        Ok(Value::Ptr(self.heap.len() - 1))
    }

    pub(crate) fn builtin_load(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("load", 1, args.len()))?;
        let slot = ptr.as_ptr()?;
        self.heap
            .get(slot)
            .and_then(|value| value.clone())
            .ok_or_else(|| RuntimeError::new("L0406", format!("invalid pointer `{slot}`")))
    }

    pub(crate) fn builtin_store(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_dealloc(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    /// `size_of(x) -> i64`: the C-natural byte size of `x`'s type (a
    /// compile-time constant that depends only on the type).
    pub(crate) fn builtin_size_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_align_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_offset_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    /// `ptr_to_int(p) -> i64`: the integer address/handle of a raw pointer. On
    /// the interpreters a pointer is a heap-slot handle, so this returns that
    /// handle; it round-trips through `int_to_ptr`.
    pub(crate) fn builtin_ptr_to_int(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("ptr_to_int", 1, args.len()))?;
        Ok(Value::I64(ptr.as_ptr()? as i64))
    }

    /// `int_to_ptr(n) -> ptr<T>`: reconstruct a raw pointer from an integer
    /// handle (the inverse of `ptr_to_int`).
    pub(crate) fn builtin_int_to_ptr(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("int_to_ptr", 1, args.len()))?;
        Ok(Value::Ptr(handle.as_i64()? as usize))
    }

    pub(crate) fn builtin_read_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    /// `read_line() -> option<string>`: read one line from standard input with
    /// the trailing newline (`\n`, and a preceding `\r` on Windows CRLF input)
    /// stripped. `none` at end-of-input; a blank input line is `some("")`, so EOF
    /// and an empty line stay distinct. Reads through the shared, buffered global
    /// `Stdin`, so repeated calls consume consecutive lines without losing
    /// buffered bytes.
    pub(crate) fn builtin_read_line(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_line", 0, args.len()))?;
        read_stdin_line()
    }

    /// `read_all() -> string`: read the whole of standard input to EOF into a
    /// single `string`. Empty string when stdin is empty or already closed.
    pub(crate) fn builtin_read_all(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_all", 0, args.len()))?;
        read_stdin_all()
    }

    pub(crate) fn builtin_write_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_append_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_file_exists(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("file_exists", 1, args.len()))?;
        Ok(Value::Bool(fs::metadata(path.as_string()?).is_ok()))
    }

    pub(crate) fn builtin_read_lines(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_read_bytes(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_bytes", 1, args.len()))?;
        let path = path.as_string()?;
        let bytes = fs::read(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        Ok(Value::Array(bytes.into_iter().map(Value::Byte).collect()))
    }

    pub(crate) fn builtin_write_bytes(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn value_to_bytes(name: &str, value: Value) -> Result<Vec<u8>, RuntimeError> {
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

    pub(crate) fn builtin_file_size(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("file_size", 1, args.len()))?;
        let path = path.as_string()?;
        let metadata = fs::metadata(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        Ok(Value::I64(metadata.len() as i64))
    }

    pub(crate) fn builtin_is_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("is_file", 1, args.len()))?;
        Ok(Value::Bool(
            fs::metadata(path.as_string()?)
                .map(|m| m.is_file())
                .unwrap_or(false),
        ))
    }

    pub(crate) fn builtin_is_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("is_dir", 1, args.len()))?;
        Ok(Value::Bool(
            fs::metadata(path.as_string()?)
                .map(|m| m.is_dir())
                .unwrap_or(false),
        ))
    }

    pub(crate) fn builtin_list_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_make_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_remove_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_remove_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_sys_status(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_sys_output(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_print(
        &self,
        name: &'static str,
        args: Vec<Value>,
        newline: bool,
    ) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        let text = text.as_string()?;
        #[cfg(target_arch = "wasm32")]
        {
            // wasm has no real stdout — capture program output into a thread-local
            // buffer the wasm entry point drains (the browser playground reads it).
            let _ = name;
            WASM_STDOUT.with(|buf| {
                let mut b = buf.borrow_mut();
                b.push_str(&text);
                if newline {
                    b.push('\n');
                }
            });
            Ok(Value::Void)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::io::Write;
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
    }

    pub(crate) fn builtin_warn(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_flush(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    /// baseline. Non-decreasing within a run. Shares the baseline with every
    /// other backend through [`monotonic_now_nanos`].
    pub(crate) fn builtin_mono_now(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mono_now", 0, args.len()))?;
        Ok(Value::I64(monotonic_now_nanos()))
    }

    /// `wall_now() -> i64`: milliseconds since the Unix epoch (wall-clock time).
    pub(crate) fn builtin_wall_now(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("wall_now", 0, args.len()))?;
        Ok(Value::I64(wall_now_millis()))
    }

    /// `sleep_millis(ms i64) -> void`: sleep the current thread for `ms`
    /// milliseconds; a negative `ms` sleeps for zero (no error).
    pub(crate) fn builtin_sleep_millis(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ms]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sleep_millis", 1, args.len()))?;
        let ms = expect_i64("sleep_millis", ms)?;
        sleep_millis(ms);
        Ok(Value::Void)
    }

    /// `wasm_log(x i64) -> void`: the host log builtin. On the WASM backend it
    /// lowers to a `call` of the imported `env.log_i64`; on the interpreters it
    /// prints the value as a stdout line so all backends observe the same side
    /// effect and the parity harness stays green.
    pub(crate) fn builtin_wasm_log(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("wasm_log", 1, args.len()))?;
        let value = expect_i64("wasm_log", value)?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{value}").map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    /// `console_log(s string) -> void`: the JS/DOM host console builtin. On the
    /// WASM backend it lowers to a `call` of the imported
    /// `env.console_log(ptr, len)` (a browser host implements it as
    /// `console.log`); on the interpreters it prints the string as a stdout line
    /// so all backends observe the same side effect and the parity harness stays
    /// green.
    pub(crate) fn builtin_console_log(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    /// `env.dom_set_text(id_ptr, id_len, text_ptr, text_len)` (a browser host
    /// implements it as `document.getElementById(id).textContent = text`); on the
    /// interpreters it prints the deterministic line `id=text` so all backends
    /// observe the same side effect and the parity harness stays green.
    pub(crate) fn builtin_dom_set_text(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    /// `assert(cond bool) -> void`: raises a catchable user-error (the same code
    /// `L0420` a `throw` produces, so `try`/`catch` recovers it) with the message
    /// `assertion failed` when `cond` is false; returns void when true.
    pub(crate) fn builtin_assert(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("assert", 1, args.len()))?;
        if value.as_bool()? {
            Ok(Value::Void)
        } else {
            Err(RuntimeError::new("L0420", "assertion failed"))
        }
    }

    pub(crate) fn builtin_to_string(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_char_code(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_char_from(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_is_digit(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_digit", args, |c| c.is_ascii_digit())
    }

    /// `is_alpha(c char) -> bool`: whether `c` is an alphabetic character.
    pub(crate) fn builtin_is_alpha(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_alpha", args, |c| c.is_alphabetic())
    }

    /// `is_alnum(c char) -> bool`: whether `c` is alphabetic or numeric.
    pub(crate) fn builtin_is_alnum(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_alnum", args, |c| c.is_alphanumeric())
    }

    /// `is_whitespace(c char) -> bool`: whether `c` is a whitespace character.
    pub(crate) fn builtin_is_whitespace(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_whitespace", args, |c| c.is_whitespace())
    }

    /// `is_upper(c char) -> bool`: whether `c` is an uppercase character.
    pub(crate) fn builtin_is_upper(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_upper", args, |c| c.is_uppercase())
    }

    /// `is_lower(c char) -> bool`: whether `c` is a lowercase character.
    pub(crate) fn builtin_is_lower(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_lower", args, |c| c.is_lowercase())
    }

    /// Shared helper for the deterministic `char -> bool` classification
    /// predicates: unwrap a single `char` operand and apply `test`, reporting a
    /// runtime error (never a panic) on a non-char operand.
    pub(crate) fn char_predicate(
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
    pub(crate) fn builtin_byte(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    /// `to_<T>(x i64) -> T`: reinterpret an `i64` into fixed-width integer `T`,
    /// truncating to `T`'s width (the wrapping conversion). Shared by every
    /// `to_i8`/`to_i16`/`to_i32`/`to_u16`/`to_u32`/`to_u64`/`to_isize`/`to_usize`.
    pub(crate) fn builtin_to_int(
        name: &'static str,
        args: Vec<Value>,
        ty: IntKind,
    ) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        Ok(Value::int(expect_i64(name, value)?, ty))
    }

    /// `to_i64(x) -> i64`: widen a fixed-width integer into `i64`. The cell is
    /// already normalized (signed kinds sign-extended, unsigned zero-extended),
    /// so widening is the identity on the stored value.
    pub(crate) fn builtin_to_i64(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_to_f32(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_f32", 1, args.len()))?;
        Ok(Value::F32(value.as_f64()? as f32))
    }

    /// `to_f64(x f32) -> f64`: widen an `f32` to `f64` (exact).
    pub(crate) fn builtin_to_f64(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    /// `byte_val(b byte) -> i64`: the numeric value of a byte.
    pub(crate) fn builtin_byte_val(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_list_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_new", 0, args.len()))?;
        Ok(Value::Array((Vec::new()).into()))
    }

    /// `array_fill(n, x) -> array<T>`: a new array of length `n` with every
    /// element equal to `x`. `array_fill(n, 0)` is the natural runtime-sized
    /// zeroed buffer (DP tables, matrices, BFS visited sets); a negative length
    /// is a runtime error.
    pub(crate) fn builtin_array_fill(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    /// `push(l, x) -> list<T>`: a new list with `x` appended.
    pub(crate) fn builtin_push(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("push", 2, args.len()))?;
        let mut values = expect_list("push", list)?;
        values.push(value);
        Ok(Value::Array((values).into()))
    }

    /// `get(l, i) -> T`: bounds-checked element read.
    pub(crate) fn builtin_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_pop(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_list_index_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_list_contains(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, target]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_contains", 2, args.len()))?;
        let values = expect_list("list_contains", list)?;
        Ok(Value::Bool(values.contains(&target)))
    }

    /// `reverse(l) -> list<T>`: a new list with the elements reversed.
    pub(crate) fn builtin_reverse(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("reverse", 1, args.len()))?;
        let mut values = expect_list("reverse", list)?;
        values.reverse();
        Ok(Value::Array((values).into()))
    }

    /// `sort(l list<i64>) -> list<i64>`: a new list with the elements sorted
    /// ascending.
    pub(crate) fn builtin_sort(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sort", 1, args.len()))?;
        let values = expect_list("sort", list)?;
        sort_scalar_list("sort", values)
    }

    /// `sort_by(l list<T>, cmp fn(T, T) -> i64) -> list<T>`: return a new list
    /// sorted by the comparator (`cmp(a, b)` negative if `a` precedes `b`, zero
    /// if equal, positive if after). Uses a stable sort, so equal elements keep
    /// their input order. The comparator's error, if any, is propagated.
    pub(crate) fn builtin_sort_by(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_concat(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_slice(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    /// `Value::Closure`) with `args`, reusing the same invocation path that
    /// `parallel_map`/direct call dispatch use. Shared by the higher-order list
    /// builtins so closures capture correctly and named functions resolve
    /// through the normal call machinery.
    pub(crate) fn invoke_callable(
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
    pub(crate) fn builtin_list_map(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_list_filter(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_list_reduce(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_map_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_new", 0, args.len()))?;
        Ok(Value::Map(Box::default()))
    }

    /// `map_set(m, k, v) -> map<K, V>`: a new map with `k` mapped to `v`.
    /// Overwriting an existing key or appending a new one is O(1).
    pub(crate) fn builtin_map_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key, value]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_set", 3, args.len()))?;
        let mut entries = expect_map("map_set", map)?;
        entries.insert(key, value);
        Ok(Value::Map(Box::new(entries)))
    }

    /// `map_get(m, k) -> option<V>`: `some(v)` if present, else `none`. O(1).
    pub(crate) fn builtin_map_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_get", 2, args.len()))?;
        let entries = expect_map("map_get", map)?;
        let found = entries.get(&key).cloned();
        Ok(option_value(found))
    }

    /// `map_has(m, k) -> bool`. O(1).
    pub(crate) fn builtin_map_has(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_has", 2, args.len()))?;
        let entries = expect_map("map_has", map)?;
        Ok(Value::Bool(entries.contains_key(&key)))
    }

    /// `map_len(m) -> i64`.
    pub(crate) fn builtin_map_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_len", 1, args.len()))?;
        let entries = expect_map("map_len", map)?;
        Ok(Value::I64(entries.len() as i64))
    }

    /// `map_keys(m) -> list<K>`: the keys in insertion order.
    pub(crate) fn builtin_map_keys(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_keys", 1, args.len()))?;
        let entries = expect_map("map_keys", map)?;
        Ok(Value::Array(
            entries.into_entries().into_iter().map(|(k, _)| k).collect(),
        ))
    }

    /// `map_values(m) -> list<V>`: the values in insertion order.
    pub(crate) fn builtin_map_values(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_values", 1, args.len()))?;
        let entries = expect_map("map_values", map)?;
        Ok(Value::Array(
            entries.into_entries().into_iter().map(|(_, v)| v).collect(),
        ))
    }

    /// `map_del(m, k) -> map<K, V>`: a new map without key `k`.
    pub(crate) fn builtin_map_del(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_del", 2, args.len()))?;
        let mut entries = expect_map("map_del", map)?;
        entries.remove(&key);
        Ok(Value::Map(Box::new(entries)))
    }

    pub(crate) fn builtin_substring(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_find(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, needle]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("find", 2, args.len()))?;
        let text = expect_string("find", text)?;
        let needle = expect_string("find", needle)?;
        Ok(Value::I64(char_find(&text, &needle)))
    }

    pub(crate) fn builtin_contains(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, needle]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("contains", 2, args.len()))?;
        let text = expect_string("contains", text)?;
        let needle = expect_string("contains", needle)?;
        Ok(Value::Bool(text.contains(&needle)))
    }

    pub(crate) fn builtin_starts_with(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, prefix]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("starts_with", 2, args.len()))?;
        let text = expect_string("starts_with", text)?;
        let prefix = expect_string("starts_with", prefix)?;
        Ok(Value::Bool(text.starts_with(&prefix)))
    }

    pub(crate) fn builtin_ends_with(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, suffix]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("ends_with", 2, args.len()))?;
        let text = expect_string("ends_with", text)?;
        let suffix = expect_string("ends_with", suffix)?;
        Ok(Value::Bool(text.ends_with(&suffix)))
    }

    pub(crate) fn builtin_repeat(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_split(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_words(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_count(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_join(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_trim(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_replace(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_upper(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("upper", 1, args.len()))?;
        let text = expect_string("upper", text)?;
        Ok(Value::String((text.to_uppercase()).into()))
    }

    /// `chars(s) -> list<char>`: the characters of `s` in order.
    pub(crate) fn builtin_chars(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("chars", 1, args.len()))?;
        let text = expect_string("chars", text)?;
        Ok(Value::Array(text.chars().map(Value::Char).collect()))
    }

    /// `string_from_chars(cs) -> string`: concatenate a `list<char>` into a string.
    pub(crate) fn builtin_string_from_chars(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_lower(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("lower", 1, args.len()))?;
        let text = expect_string("lower", text)?;
        Ok(Value::String((text.to_lowercase()).into()))
    }

    /// `to_bytes(s string) -> list<byte>`: the UTF-8 encoding of `s` as a
    /// `list<byte>` (a `Value::Array` of `Value::Byte`, matching `read_bytes`).
    pub(crate) fn builtin_to_bytes(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_from_bytes(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_byte_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_parse_i64(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_parse_f64(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_abs(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("abs", 1, args.len()))?;
        match value {
            // Wrapping abs so `abs(i64::MIN)` yields `i64::MIN` (matching release
            // `i64::abs` and the native/WASM backends) instead of panicking on
            // overflow in a debug build — deterministic across build modes and
            // bit-identical across all backends.
            Value::I64(n) => Ok(Value::I64(n.wrapping_abs())),
            Value::F64(n) => Ok(Value::F64(n.abs())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("abs expects an i64 or f64 but got `{other}`"),
            )),
        }
    }

    /// `clamp(x, lo, hi) -> T`: `x` limited to `[lo, hi]`. Total on every input:
    /// for `lo > hi` it yields `lo`, and for an f64 NaN `x` it returns `x`
    /// unchanged.
    pub(crate) fn builtin_clamp(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    /// `sign(x) -> i64`: `-1`/`0`/`1` for negative/zero/positive. For f64, `NaN`
    /// and `-0.0` both map to `0`. Always returns `i64`.
    pub(crate) fn builtin_sign(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    /// `gcd(a, b) -> i64`: non-negative greatest common divisor of the absolute
    /// values (see `gcd_i64`, which is total even at `i64::MIN`).
    pub(crate) fn builtin_gcd(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    /// `list_sum(l) -> T`: the sum of a `list<i64>` (wrapping, matching `+`) or a
    /// `list<f64>`. An empty list yields `0`/`0.0`.
    pub(crate) fn builtin_list_sum(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_sum", 1, args.len()))?;
        let values = expect_list("list_sum", list)?;
        list_sum_values("list_sum", values)
    }

    /// `list_min(l) -> option<T>`: `none` on empty, else `some(minimum)`.
    pub(crate) fn builtin_list_min(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_min", 1, args.len()))?;
        let values = expect_list("list_min", list)?;
        Ok(option_value(list_extreme("list_min", values, false)?))
    }

    /// `list_max(l) -> option<T>`: `none` on empty, else `some(maximum)`.
    pub(crate) fn builtin_list_max(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_max", 1, args.len()))?;
        let values = expect_list("list_max", list)?;
        Ok(option_value(list_extreme("list_max", values, true)?))
    }

    pub(crate) fn builtin_min(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_max(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_pow(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_sqrt(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_floor(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_ceil(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_round(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    /// Shared implementation for the unary `f64 -> f64` math builtins
    /// (`sin`/`cos`/`tan`/`atan`/`exp`/`ln`/`log10`). Undefined inputs follow
    /// the platform `f64` semantics (`NaN`/`inf`), identically on every backend.
    pub(crate) fn builtin_unary_f64(
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
    pub(crate) fn builtin_atan2(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    /// positions. The mask makes it total for any `n` (large or negative).
    pub(crate) fn builtin_rotate_left(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    /// positions. Total for any `n` (the mask handles large/negative values).
    pub(crate) fn builtin_rotate_right(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_count_ones(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_leading_zeros(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_trailing_zeros(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_reverse_bytes(args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_rc_new(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_new", 1, args.len()))?;
        self.heap.push(Some(value));
        let slot = self.heap.len() - 1;
        self.refcounts.insert(slot, 1);
        Ok(Value::Ptr(slot))
    }

    pub(crate) fn builtin_rc_clone(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_rc_release(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

    pub(crate) fn builtin_rc_borrow(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_borrow", 1, args.len()))?;
        let slot = handle.as_ptr()?;
        if self.refcounts.contains_key(&slot) {
            // A borrow is a non-owning view of the same live slot.
            Ok(Value::Ptr(slot))
        } else {
            Err(RuntimeError::new(
                "L0406",
                format!("invalid reference-counted handle `{slot}`"),
            ))
        }
    }

    /// Shared dereference for `rc_get`, `ref_get`, and (unsafe) `ptr_read`.
    pub(crate) fn builtin_ref_get(
        &self,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        let slot = handle.as_ptr()?;
        self.heap
            .get(slot)
            .and_then(|value| value.clone())
            .ok_or_else(|| RuntimeError::new("L0406", format!("invalid pointer `{slot}`")))
    }

    pub(crate) fn wrong_arity(name: &str, expected: usize, actual: usize) -> RuntimeError {
        RuntimeError::new(
            "L0405",
            format!("function `{name}` expects {expected} arguments but got {actual}"),
        )
    }
}
