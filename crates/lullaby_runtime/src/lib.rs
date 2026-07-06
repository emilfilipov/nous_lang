use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::process::Command;

use lullaby_diagnostics::{Span, TraceFrame};
use lullaby_parser::{AssignOp, BinaryOp, Expr, ExprKind, Function, Place, Program, Stmt, UnaryOp};

// `Eq` is intentionally omitted: `Value::F64` holds an `f64`, which is not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    I64(i64),
    F64(f64),
    Bool(bool),
    String(String),
    Array(Vec<Value>),
    Ptr(usize),
    Struct {
        name: String,
        fields: Vec<(String, Value)>,
    },
    Void,
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::I64(value) => write!(formatter, "{value}"),
            Self::F64(value) => write!(formatter, "{value}"),
            Self::Bool(value) => write!(formatter, "{value}"),
            Self::String(value) => write!(formatter, "{value}"),
            Self::Array(values) => {
                let values = values
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(formatter, "[{values}]")
            }
            Self::Ptr(slot) => write!(formatter, "ptr({slot})"),
            Self::Struct { name, fields } => {
                let rendered = fields
                    .iter()
                    .map(|(field, value)| format!("{field}: {value}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(formatter, "{name}({rendered})")
            }
            Self::Void => write!(formatter, "void"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    pub code: &'static str,
    pub category: ErrorCategory,
    pub message: String,
    pub span: Option<Span>,
    pub function: Option<String>,
    pub traceback: Vec<TraceFrame>,
}

impl RuntimeError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self::categorized(code, ErrorCategory::Runtime, message)
    }

    pub fn resource(code: &'static str, message: impl Into<String>) -> Self {
        Self::categorized(code, ErrorCategory::Resource, message)
    }

    pub fn categorized(
        code: &'static str,
        category: ErrorCategory,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            category,
            message: message.into(),
            span: None,
            function: None,
            traceback: Vec::new(),
        }
    }

    pub fn with_span(mut self, span: Span) -> Self {
        if self.span.is_none() {
            self.span = Some(span);
        }
        self
    }

    pub fn with_function(mut self, function: impl Into<String>) -> Self {
        if self.function.is_none() {
            self.function = Some(function.into());
        }
        self
    }

    pub fn with_traceback(mut self, traceback: Vec<TraceFrame>) -> Self {
        if self.traceback.is_empty() {
            self.traceback = traceback;
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    Runtime,
    Resource,
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime => write!(formatter, "runtime"),
            Self::Resource => write!(formatter, "resource"),
        }
    }
}

pub fn run_main(program: &Program) -> Result<Value, RuntimeError> {
    let mut runtime = Runtime::new(program)?;
    runtime.call_function("main", Vec::new())
}

struct Runtime<'a> {
    functions: HashMap<&'a str, &'a Function>,
    /// Declared struct types: name -> ordered field names, used to build struct
    /// values from positional construction arguments.
    structs: HashMap<&'a str, Vec<String>>,
    heap: Vec<Option<Value>>,
    /// Ownership counts for reference-counted (`rc<T>`) heap slots, keyed by
    /// slot index. Slots not present here are raw pointers / plain allocations.
    refcounts: HashMap<usize, usize>,
    call_stack: Vec<TraceFrame>,
}

impl<'a> Runtime<'a> {
    fn new(program: &'a Program) -> Result<Self, RuntimeError> {
        let functions = program
            .functions
            .iter()
            .map(|function| (function.name.as_str(), function))
            .collect::<HashMap<_, _>>();

        if !functions.contains_key("main") {
            return Err(RuntimeError::new("L0400", "missing `main` function"));
        }

        let structs = program
            .structs
            .iter()
            .map(|declaration| {
                (
                    declaration.name.as_str(),
                    declaration
                        .fields
                        .iter()
                        .map(|field| field.name.clone())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();

        Ok(Self {
            functions,
            structs,
            heap: Vec::new(),
            refcounts: HashMap::new(),
            call_stack: Vec::new(),
        })
    }

    fn call_function(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        if let Some(field_names) = self.structs.get(name) {
            return Ok(Value::Struct {
                name: name.to_string(),
                fields: field_names.iter().cloned().zip(args).collect(),
            });
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
            "sys_status" => self.builtin_sys_status(args),
            "sys_output" => self.builtin_sys_output(args),
            "print" => self.builtin_print("print", args, false),
            "println" => self.builtin_print("println", args, true),
            "warn" => self.builtin_warn(args),
            "flush" => self.builtin_flush(args),
            "to_string" => Self::builtin_to_string(args),
            "len" => Self::builtin_len(args),
            "rc_new" => self.builtin_rc_new(args),
            "rc_clone" => self.builtin_rc_clone(args),
            "rc_release" => self.builtin_rc_release(args),
            "rc_get" | "ref_get" | "ptr_read" => self.builtin_ref_get(name, args),
            "rc_borrow" => self.builtin_rc_borrow(args),
            "ptr_write" => self.builtin_store(args),
            _ => {
                let function = *self.functions.get(name).ok_or_else(|| {
                    RuntimeError::new("L0401", format!("unknown function `{name}`"))
                })?;

                if function.params.len() != args.len() {
                    return Err(RuntimeError::new(
                        "L0402",
                        format!(
                            "function `{name}` expects {} arguments but got {}",
                            function.params.len(),
                            args.len()
                        ),
                    ));
                }

                let mut env = Env::default();
                for (param, value) in function.params.iter().zip(args) {
                    env.define(param.name.clone(), value);
                }

                self.call_stack.push(TraceFrame {
                    function: function.name.clone(),
                    span: Some(function.span),
                });
                let result = self.eval_block(&function.body, &mut env);
                let traceback = self.call_stack.clone();
                self.call_stack.pop();

                match result.map_err(|error| error.with_traceback(traceback))? {
                    Control::Return(value) | Control::Value(value) => Ok(value),
                    Control::Break | Control::Continue => Err(RuntimeError::new(
                        "L0410",
                        "loop control escaped function body",
                    )),
                }
            }
        }
    }

    fn eval_block(&mut self, statements: &[Stmt], env: &mut Env) -> Result<Control, RuntimeError> {
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

    fn eval_statement(&mut self, statement: &Stmt, env: &mut Env) -> Result<Control, RuntimeError> {
        let span = statement_span(statement);
        let result = match statement {
            Stmt::Let { name, value, .. } => {
                let value = self.eval_expr(value, env)?;
                env.define(name.clone(), value);
                Ok(Control::Value(Value::Void))
            }
            Stmt::Assign {
                name,
                path,
                op,
                value,
                ..
            } => {
                let rhs = self.eval_expr(value, env)?;
                if path.is_empty() {
                    let new = match op {
                        AssignOp::Replace => rhs,
                        _ => apply_compound(env.get(name)?, op, rhs)?,
                    };
                    env.assign(name, new)?;
                } else {
                    let resolved = self.resolve_places(path, env)?;
                    let mut root = env.get(name)?;
                    let new = match op {
                        AssignOp::Replace => rhs,
                        _ => apply_compound(get_place(&root, &resolved)?, op, rhs)?,
                    };
                    set_place(&mut root, &resolved, new)?;
                    env.assign(name, root)?;
                }
                Ok(Control::Value(Value::Void))
            }
            Stmt::Return(expr) => {
                let value = expr
                    .as_ref()
                    .map(|expr| self.eval_expr(expr, env))
                    .unwrap_or(Ok(Value::Void))?;
                Ok(Control::Return(value))
            }
            Stmt::Break(_) => Ok(Control::Break),
            Stmt::Continue(_) => Ok(Control::Continue),
            Stmt::Expr(expr) => self.eval_expr(expr, env).map(Control::Value),
            Stmt::If {
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
            Stmt::While {
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
            Stmt::For {
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

                while if step > 0 {
                    current <= end
                } else {
                    current >= end
                } {
                    env.push_scope();
                    env.define(name.clone(), Value::I64(current));
                    let result = self.eval_block(body, env);
                    env.pop_scope();

                    match result? {
                        Control::Return(value) => return Ok(Control::Return(value)),
                        Control::Break => break,
                        Control::Continue | Control::Value(_) => {}
                    }

                    current += step;
                }
                Ok(Control::Value(Value::Void))
            }
            Stmt::Loop { body, .. } => {
                loop {
                    match self.eval_scoped_block(body, env)? {
                        Control::Return(value) => return Ok(Control::Return(value)),
                        Control::Break => break,
                        Control::Continue | Control::Value(_) => {}
                    }
                }
                Ok(Control::Value(Value::Void))
            }
            // `unsafe` is a transparent gate: its body runs in the enclosing
            // scope, matching IR lowering, which inlines the body.
            Stmt::Unsafe { body, .. } => self.eval_block(body, env),
            // A region declaration is compile-time metadata; it has no runtime
            // effect in the current analysis-only region model.
            Stmt::Region(_) => Ok(Control::Value(Value::Void)),
            Stmt::Throw { value, .. } => {
                let message = self.eval_expr(value, env)?.as_string()?;
                Err(RuntimeError::new("L0420", message))
            }
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                ..
            } => match self.eval_scoped_block(body, env) {
                // Only user-thrown errors are recoverable; system errors propagate.
                Err(error) if error.code == "L0420" => {
                    env.push_scope();
                    env.define(catch_name.clone(), Value::String(error.message.clone()));
                    let result = self.eval_block(catch_body, env);
                    env.pop_scope();
                    result
                }
                other => other,
            },
        };
        result.map_err(|error| self.annotate_error(error, span))
    }

    fn eval_scoped_block(
        &mut self,
        statements: &[Stmt],
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        env.push_scope();
        let result = self.eval_block(statements, env);
        env.pop_scope();
        result
    }

    /// Resolve a parser assignment path into concrete places, evaluating each
    /// array index expression against the current environment.
    fn resolve_places(
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

    fn eval_expr(&mut self, expr: &Expr, env: &Env) -> Result<Value, RuntimeError> {
        let result = match &expr.kind {
            ExprKind::Field { target, field } => {
                let target = self.eval_expr(target, env)?;
                match target {
                    Value::Struct { fields, .. } => fields
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
            ExprKind::String(value) => Ok(Value::String(value.clone())),
            ExprKind::Array(values) => values
                .iter()
                .map(|value| self.eval_expr(value, env))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            ExprKind::Variable(name) => env.get(name),
            ExprKind::Index { target, index } => {
                let target = self.eval_expr(target, env)?;
                let index = self.eval_expr(index, env)?.as_i64()?;
                let Value::Array(values) = target else {
                    return Err(RuntimeError::new("L0412", "index target is not an array"));
                };
                if index < 0 {
                    return Err(RuntimeError::new(
                        "L0413",
                        format!("array index `{index}` is out of bounds"),
                    ));
                }
                values.get(index as usize).cloned().ok_or_else(|| {
                    RuntimeError::new("L0413", format!("array index `{index}` is out of bounds"))
                })
            }
            ExprKind::Unary { op, expr } => {
                let value = self.eval_expr(expr, env)?;
                match op {
                    UnaryOp::Not => Ok(Value::Bool(!value.as_bool()?)),
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
                self.call_function(name, values)
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
        };
        result.map_err(|error| self.annotate_error(error, expr.span))
    }

    fn annotate_error(&self, error: RuntimeError, span: Span) -> RuntimeError {
        let error = error.with_span(span);
        match self.call_stack.last() {
            Some(frame) => error
                .with_function(frame.function.clone())
                .with_traceback(self.call_stack.clone()),
            None => error,
        }
    }

    fn eval_binary(&self, left: Value, op: BinaryOp, right: Value) -> Result<Value, RuntimeError> {
        // Float arithmetic/comparison when both operands are f64 (IEEE 754
        // semantics: division by zero yields infinity/NaN, not an error).
        if let (Value::F64(l), Value::F64(r)) = (&left, &right) {
            let (l, r) = (*l, *r);
            return Ok(match op {
                BinaryOp::Add => Value::F64(l + r),
                BinaryOp::Subtract => Value::F64(l - r),
                BinaryOp::Multiply => Value::F64(l * r),
                BinaryOp::Divide => Value::F64(l / r),
                BinaryOp::Equal => Value::Bool(l == r),
                BinaryOp::NotEqual => Value::Bool(l != r),
                BinaryOp::Less => Value::Bool(l < r),
                BinaryOp::LessEqual => Value::Bool(l <= r),
                BinaryOp::Greater => Value::Bool(l > r),
                BinaryOp::GreaterEqual => Value::Bool(l >= r),
                BinaryOp::And | BinaryOp::Or => {
                    unreachable!("logical ops short-circuit in eval_expr")
                }
            });
        }
        match op {
            // `+` concatenates when both operands are strings; otherwise it adds i64s.
            BinaryOp::Add if matches!((&left, &right), (Value::String(_), Value::String(_))) => {
                Ok(Value::String(left.as_string()? + &right.as_string()?))
            }
            BinaryOp::Add => Ok(Value::I64(left.as_i64()? + right.as_i64()?)),
            BinaryOp::Subtract => Ok(Value::I64(left.as_i64()? - right.as_i64()?)),
            BinaryOp::Multiply => Ok(Value::I64(left.as_i64()? * right.as_i64()?)),
            BinaryOp::Divide => {
                let divisor = right.as_i64()?;
                if divisor == 0 {
                    Err(RuntimeError::new("L0404", "division by zero"))
                } else {
                    Ok(Value::I64(left.as_i64()? / divisor))
                }
            }
            BinaryOp::Equal => Ok(Value::Bool(left == right)),
            BinaryOp::NotEqual => Ok(Value::Bool(left != right)),
            BinaryOp::Less => Ok(Value::Bool(left.as_i64()? < right.as_i64()?)),
            BinaryOp::LessEqual => Ok(Value::Bool(left.as_i64()? <= right.as_i64()?)),
            BinaryOp::Greater => Ok(Value::Bool(left.as_i64()? > right.as_i64()?)),
            BinaryOp::GreaterEqual => Ok(Value::Bool(left.as_i64()? >= right.as_i64()?)),
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

    fn builtin_read_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_file", 1, args.len()))?;
        let path = path.as_string()?;
        fs::read_to_string(&path)
            .map(Value::String)
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
            String::from_utf8_lossy(&output.stdout).to_string(),
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

    fn builtin_to_string(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_string", 1, args.len()))?;
        match value {
            Value::I64(_) | Value::F64(_) | Value::Bool(_) | Value::String(_) => {
                Ok(Value::String(value.to_string()))
            }
            other => Err(RuntimeError::new(
                "L0417",
                format!("to_string cannot convert `{other}`"),
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

enum Control {
    Return(Value),
    Break,
    Continue,
    Value(Value),
}

/// Apply a compound assignment operator (`+=` etc.) to `current` and `rhs`,
/// supporting i64 and f64.
pub fn apply_compound(current: Value, op: &AssignOp, rhs: Value) -> Result<Value, RuntimeError> {
    if let (Value::F64(a), Value::F64(b)) = (&current, &rhs) {
        let (a, b) = (*a, *b);
        return Ok(Value::F64(match op {
            AssignOp::Add => a + b,
            AssignOp::Subtract => a - b,
            AssignOp::Multiply => a * b,
            AssignOp::Divide => a / b,
            AssignOp::Replace => b,
        }));
    }
    let a = current.as_i64()?;
    let b = rhs.as_i64()?;
    Ok(Value::I64(match op {
        AssignOp::Add => a + b,
        AssignOp::Subtract => a - b,
        AssignOp::Multiply => a * b,
        AssignOp::Divide => {
            if b == 0 {
                return Err(RuntimeError::new("L0404", "division by zero"));
            }
            a / b
        }
        AssignOp::Replace => b,
    }))
}

/// One hop of a resolved assignment target: a struct field name or an
/// already-evaluated array index. Index expressions are evaluated by each
/// backend's own evaluator before mutation, so these helpers stay shared.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedPlace {
    Field(String),
    Index(i64),
}

fn place_get<'a>(current: &'a Value, place: &ResolvedPlace) -> Result<&'a Value, RuntimeError> {
    match place {
        ResolvedPlace::Field(field) => {
            let Value::Struct { fields, .. } = current else {
                return Err(RuntimeError::new(
                    "L0371",
                    format!("cannot access field `{field}` on non-struct value"),
                ));
            };
            fields
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, value)| value)
                .ok_or_else(|| RuntimeError::new("L0371", format!("no field `{field}`")))
        }
        ResolvedPlace::Index(index) => {
            let Value::Array(values) = current else {
                return Err(RuntimeError::new("L0412", "index target is not an array"));
            };
            if *index < 0 {
                return Err(RuntimeError::new(
                    "L0413",
                    format!("array index `{index}` is out of bounds"),
                ));
            }
            values.get(*index as usize).ok_or_else(|| {
                RuntimeError::new("L0413", format!("array index `{index}` is out of bounds"))
            })
        }
    }
}

fn place_get_mut<'a>(
    current: &'a mut Value,
    place: &ResolvedPlace,
) -> Result<&'a mut Value, RuntimeError> {
    match place {
        ResolvedPlace::Field(field) => {
            let Value::Struct { fields, .. } = current else {
                return Err(RuntimeError::new(
                    "L0371",
                    format!("cannot access field `{field}` on non-struct value"),
                ));
            };
            fields
                .iter_mut()
                .find(|(name, _)| name == field)
                .map(|(_, value)| value)
                .ok_or_else(|| RuntimeError::new("L0371", format!("no field `{field}`")))
        }
        ResolvedPlace::Index(index) => {
            let Value::Array(values) = current else {
                return Err(RuntimeError::new("L0412", "index target is not an array"));
            };
            if *index < 0 {
                return Err(RuntimeError::new(
                    "L0413",
                    format!("array index `{index}` is out of bounds"),
                ));
            }
            let index = *index as usize;
            let len = values.len();
            values.get_mut(index).ok_or_else(|| {
                RuntimeError::new(
                    "L0413",
                    format!("array index `{index}` is out of bounds (len {len})"),
                )
            })
        }
    }
}

/// Read the value at a resolved place path (for compound assignment).
pub fn get_place(value: &Value, path: &[ResolvedPlace]) -> Result<Value, RuntimeError> {
    let mut current = value;
    for place in path {
        current = place_get(current, place)?;
    }
    Ok(current.clone())
}

/// Set the value at a resolved place path in place.
pub fn set_place(root: &mut Value, path: &[ResolvedPlace], new: Value) -> Result<(), RuntimeError> {
    let mut current = root;
    for (index, place) in path.iter().enumerate() {
        let slot = place_get_mut(current, place)?;
        if index + 1 == path.len() {
            *slot = new;
            return Ok(());
        }
        current = slot;
    }
    Ok(())
}

fn statement_span(statement: &Stmt) -> Span {
    match statement {
        Stmt::Let { span, .. }
        | Stmt::Assign { span, .. }
        | Stmt::Break(span)
        | Stmt::Continue(span)
        | Stmt::If { span, .. }
        | Stmt::While { span, .. }
        | Stmt::For { span, .. }
        | Stmt::Loop { span, .. }
        | Stmt::Unsafe { span, .. }
        | Stmt::Throw { span, .. }
        | Stmt::Try { span, .. } => *span,
        Stmt::Region(decl) => decl.span,
        Stmt::Return(Some(expr)) | Stmt::Expr(expr) => expr.span,
        Stmt::Return(None) => Span::new(1, 1),
    }
}

#[derive(Debug, Clone)]
struct Env {
    scopes: Vec<HashMap<String, Value>>,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            scopes: vec![HashMap::new()],
        }
    }
}

impl Env {
    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: String, value: Value) {
        self.scopes
            .last_mut()
            .expect("env always has a scope")
            .insert(name, value);
    }

    fn assign(&mut self, name: &str, value: Value) -> Result<(), RuntimeError> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                *slot = value;
                return Ok(());
            }
        }
        Err(RuntimeError::new(
            "L0403",
            format!("unknown variable `{name}`"),
        ))
    }

    fn get(&self, name: &str) -> Result<Value, RuntimeError> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
            .cloned()
            .ok_or_else(|| RuntimeError::new("L0403", format!("unknown variable `{name}`")))
    }
}

impl Value {
    pub fn as_i64(&self) -> Result<i64, RuntimeError> {
        match self {
            Self::I64(value) => Ok(*value),
            _ => Err(RuntimeError::new("L0407", "expected i64 value")),
        }
    }

    pub fn as_f64(&self) -> Result<f64, RuntimeError> {
        match self {
            Self::F64(value) => Ok(*value),
            _ => Err(RuntimeError::new("L0421", "expected f64 value")),
        }
    }

    pub fn as_bool(&self) -> Result<bool, RuntimeError> {
        match self {
            Self::Bool(value) => Ok(*value),
            _ => Err(RuntimeError::new("L0408", "expected bool value")),
        }
    }

    pub fn as_ptr(&self) -> Result<usize, RuntimeError> {
        match self {
            Self::Ptr(value) => Ok(*value),
            _ => Err(RuntimeError::new("L0409", "expected pointer value")),
        }
    }

    pub fn as_string(&self) -> Result<String, RuntimeError> {
        match self {
            Self::String(value) => Ok(value.clone()),
            _ => Err(RuntimeError::new("L0417", "expected string value")),
        }
    }

    pub fn as_string_array(&self) -> Result<Vec<String>, RuntimeError> {
        match self {
            Self::Array(values) => values
                .iter()
                .map(Value::as_string)
                .collect::<Result<Vec<_>, _>>(),
            _ => Err(RuntimeError::new("L0418", "expected array<string> value")),
        }
    }
}

#[cfg(test)]
mod tests {
    use lullaby_lexer::lex;
    use lullaby_parser::parse;
    use lullaby_semantics::validate;

    use super::*;

    fn run_source(source: &str) -> Result<Value, RuntimeError> {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        validate(&program).expect("semantic");
        run_main(&program)
    }

    #[test]
    fn runs_function_calls_and_arithmetic() {
        let source = "fn add x i64 y i64 -> i64\n    x + y\n\nfn main -> i64\n    let value i64 = add(40, 2)\n    value\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn runs_memory_builtins() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn runs_store_builtin() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn runs_if_expression_result() {
        let source = "fn main -> i64\n    if true\n        42\n    else\n        0\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn runs_while_loop_with_assignment() {
        let source = "fn main -> i64\n    let x i64 = 0\n    while x < 3\n        x += 1\n    x\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(3));
    }

    #[test]
    fn runs_loop_break_and_continue() {
        let source = "fn main -> i64\n    let x i64 = 0\n    loop\n        x += 1\n        if x < 3\n            continue\n        break\n    x\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(3));
    }

    #[test]
    fn runs_logical_expressions() {
        let source = "fn main -> bool\n    not false and true or false\n";
        assert_eq!(run_source(source).expect("run"), Value::Bool(true));
    }

    #[test]
    fn short_circuits_logical_expressions() {
        let source = "fn main -> bool\n    false and (1 / 0 == 0) or true\n";
        assert_eq!(run_source(source).expect("run"), Value::Bool(true));
    }

    #[test]
    fn runs_for_loop() {
        let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    total\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(6));
    }

    #[test]
    fn mutates_array_elements_and_reports_len() {
        let source = "fn main -> i64\n    let xs array<i64> = [1, 2, 3]\n    xs[0] = 10\n    xs[len(xs) - 1] += 4\n    xs[0] + xs[2]\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(17));
    }

    #[test]
    fn array_element_assignment_bounds_checked() {
        let source = "fn main -> i64\n    let xs array<i64> = [1]\n    xs[3] = 9\n    xs[0]\n";
        let error = run_source(source).expect_err("out of bounds");
        assert_eq!(error.code, "L0413");
    }

    #[test]
    fn runs_for_loop_with_step() {
        let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 5 by 2\n        total += i\n    total\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(9));
    }

    #[test]
    fn runs_descending_for_loop() {
        let source = "fn main -> i64\n    let total i64 = 0\n    for i from 3 to 1 by -1\n        total += i\n    total\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(6));
    }

    #[test]
    fn runs_array_literal_and_index() {
        let source = "fn main -> i64\n    let values array<i64> = [2, 4, 6]\n    values[2]\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(6));
    }

    #[test]
    fn rejects_array_index_out_of_bounds() {
        let source = "fn main -> i64\n    let values array<i64> = [1, 2]\n    values[3]\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0413");
    }

    #[test]
    fn rejects_zero_for_step() {
        let source = "fn main -> i64\n    for i from 1 to 3 by 0\n        i\n    0\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0411");
    }

    #[test]
    fn keeps_let_bindings_block_scoped() {
        let source = "fn main -> i64\n    let x i64 = 1\n    if true\n        let x i64 = 2\n        x\n    x\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(1));
    }

    #[test]
    fn rejects_double_dealloc() {
        // The free is inside a branch, so the conservative compile-time
        // lifetime analysis does not track it out; the runtime L0406 guard
        // still catches the double free.
        let source = "fn main -> void\n    let ptr ptr_i64 = alloc(1)\n    if true\n        dealloc(ptr)\n    dealloc(ptr)\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0406");
    }

    #[test]
    fn rejects_store_after_dealloc() {
        let source = "fn main -> void\n    let ptr ptr_i64 = alloc(1)\n    if true\n        dealloc(ptr)\n    store(ptr, 2)\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0406");
    }

    #[test]
    fn runs_file_io_builtins() {
        let path = std::env::temp_dir()
            .join(format!("lullaby-runtime-{}.txt", std::process::id()))
            .to_string_lossy()
            .replace('\\', "/");
        let source = format!(
            "fn main -> string\n    write_file(\"{path}\", \"alpha\")\n    append_file(\"{path}\", \" beta\")\n    read_file(\"{path}\")\n"
        );
        assert_eq!(
            run_source(&source).expect("run"),
            Value::String("alpha beta".to_string())
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reports_missing_file_as_resource_error() {
        let path = std::env::temp_dir()
            .join(format!("lullaby-missing-{}.txt", std::process::id()))
            .to_string_lossy()
            .replace('\\', "/");
        let source = format!("fn main -> string\n    read_file(\"{path}\")\n");
        let error = run_source(&source).expect_err("runtime error");
        assert_eq!(error.code, "L0414");
        assert_eq!(error.category, ErrorCategory::Resource);
    }

    #[test]
    fn runs_safe_system_status_builtin() {
        let source = "fn main -> i64\n    sys_status(\"rustc\", [\"--version\"])\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(0));
    }

    #[test]
    fn runs_reference_counted_values() {
        let source = "fn main -> i64\n    let handle rc<i64> = rc_new(41)\n    let shared rc<i64> = rc_clone(handle)\n    let view ref<i64> = rc_borrow(handle)\n    let a i64 = rc_get(handle)\n    let b i64 = ref_get(view)\n    rc_release(shared)\n    rc_release(handle)\n    a + b - 40\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn rejects_use_after_rc_release() {
        // Release inside a branch escapes the conservative compile-time
        // analysis; the runtime guard still reports the dangling handle.
        let source = "fn main -> i64\n    let handle rc<i64> = rc_new(1)\n    if true\n        rc_release(handle)\n    rc_get(handle)\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0406");
    }

    #[test]
    fn runs_unsafe_raw_pointer_read() {
        let source = "fn main -> i64\n    let p ptr_i64 = alloc(42)\n    let v i64 = 0\n    unsafe\n        v = ptr_read(p)\n    dealloc(p)\n    v\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(42));
    }

    #[test]
    fn try_catch_yields_a_value_from_either_arm() {
        let caught = "fn main -> string\n    try\n        throw \"boom\"\n    catch message\n        \"caught: \" + message\n";
        assert_eq!(
            run_source(caught).expect("run"),
            Value::String("caught: boom".to_string())
        );
        let ok = "fn main -> i64\n    try\n        42\n    catch message\n        0\n";
        assert_eq!(run_source(ok).expect("run"), Value::I64(42));
    }

    #[test]
    fn catches_thrown_error_and_recovers() {
        let source = "fn main -> i64\n    let result i64 = 0\n    try\n        throw \"boom\"\n    catch message\n        result = 7\n    result\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(7));
    }

    #[test]
    fn propagates_uncaught_throw() {
        let source = "fn main -> i64\n    throw \"unhandled\"\n";
        let error = run_source(source).expect_err("runtime error");
        assert_eq!(error.code, "L0420");
        assert_eq!(error.message, "unhandled");
    }

    #[test]
    fn catch_binds_thrown_message_across_call_boundary() {
        let source = "fn risky -> i64\n    throw \"from risky\"\n\nfn main -> string\n    let captured string = \"\"\n    try\n        let value i64 = risky()\n    catch message\n        captured = message\n    captured\n";
        assert_eq!(
            run_source(source).expect("run"),
            Value::String("from risky".to_string())
        );
    }

    #[test]
    fn mutates_struct_fields() {
        let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(1, 2)\n    p.x = 10\n    p.y += 5\n    p.x + p.y\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(17));
    }

    #[test]
    fn constructs_and_reads_struct_fields() {
        let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(3, 4)\n    p.x * p.x + p.y * p.y\n";
        assert_eq!(run_source(source).expect("run"), Value::I64(25));
    }

    #[test]
    fn passes_structs_through_functions() {
        let source = "struct Player\n    name string\n    score i64\n\nfn label hero Player -> string\n    hero.name + \":\" + to_string(hero.score)\n\nfn main -> string\n    label(Player(\"Ada\", 100))\n";
        assert_eq!(
            run_source(source).expect("run"),
            Value::String("Ada:100".to_string())
        );
    }

    #[test]
    fn evaluates_f64_arithmetic() {
        let source = "fn main -> f64\n    let x f64 = 3.5\n    x + 1.5\n";
        assert_eq!(run_source(source).expect("run"), Value::F64(5.0));
    }

    #[test]
    fn compares_and_stringifies_f64() {
        let source = "fn main -> string\n    let x f64 = 2.5\n    to_string(x < 3.0) + \" \" + to_string(x * 2.0)\n";
        assert_eq!(
            run_source(source).expect("run"),
            Value::String("true 5".to_string())
        );
    }

    #[test]
    fn concatenates_strings_and_converts_values() {
        let source = "fn main -> string\n    let n i64 = 40 + 2\n    \"answer: \" + to_string(n) + \" ok=\" + to_string(n == 42)\n";
        assert_eq!(
            run_source(source).expect("run"),
            Value::String("answer: 42 ok=true".to_string())
        );
    }

    #[test]
    fn runs_standard_stream_builtins() {
        let source = "fn main -> void\n    println(\"hello\")\n    print(\"a\")\n    warn(\"w\")\n    flush()\n";
        assert_eq!(run_source(source).expect("run"), Value::Void);
    }

    #[test]
    fn runs_safe_system_output_builtin() {
        let source = "fn main -> bool\n    let output string = sys_output(\"rustc\", [\"--version\"])\n    output == \"\" == false\n";
        assert_eq!(run_source(source).expect("run"), Value::Bool(true));
    }
}
