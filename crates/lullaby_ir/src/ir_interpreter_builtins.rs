//! Builtin implementations of the IR tree-walking interpreter's impl IrRuntime,
//! split out as a second impl block. Sees the interpreter's types via use super::*.

use super::*;

impl<'a> IrRuntime<'a> {
    /// `assert(cond bool) -> void`: raises the same catchable user-error (code
    /// `L0420`) a `throw` produces when `cond` is false; returns void otherwise.
    /// Kept at parity with the AST runtime's `builtin_assert`.
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

    /// `to_<T>(x i64) -> T`: wrapping reinterpret of an `i64` into fixed-width
    /// integer `T`; shared by every `to_i8`/`to_i16`/…/`to_usize` conversion.
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

    /// `to_i64(x) -> i64`: widen a fixed-width integer into `i64` (identity on
    /// the already-normalized cell).
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

    /// `env(name string) -> option<string>`: `some(value)` when the environment
    /// variable is set, `none` otherwise.
    pub(crate) fn builtin_env(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_os_random(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_args(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_parallel_map(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_chan_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("chan_new", 0, args.len()))?;
        Ok(new_chan())
    }

    /// `send(ch Chan, v i64) -> void`: enqueue `v` (never blocks; unbounded).
    pub(crate) fn builtin_send(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_recv(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_try_recv(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_spawn(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_task_join(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [task]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("task_join", 1, args.len()))?;
        let task = expect_task("task_join", task)?;
        join_task(&task)
    }

    /// `mutex_new(v i64) -> Mutex`: a shared mutex over one `i64`.
    pub(crate) fn builtin_mutex_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_new", 1, args.len()))?;
        let value = expect_i64("mutex_new", value)?;
        Ok(Value::Mutex(SharedMutex {
            cell: Arc::new(std::sync::Mutex::new(value)),
        }))
    }

    /// `mutex_get(m Mutex) -> i64`: lock, read, unlock.
    pub(crate) fn builtin_mutex_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_mutex_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_mutex_add(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_atomic_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_new", 1, args.len()))?;
        let value = expect_i64("atomic_new", value)?;
        Ok(Value::Atomic(SharedAtomic {
            cell: Arc::new(AtomicI64::new(value)),
        }))
    }

    /// `atomic_load(a atomic_i64) -> i64`: read the cell (SeqCst).
    pub(crate) fn builtin_atomic_load(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_load", 1, args.len()))?;
        let atomic = expect_atomic("atomic_load", atomic)?;
        Ok(Value::I64(atomic.cell.load(Ordering::SeqCst)))
    }

    /// `atomic_store(a atomic_i64, v i64) -> void`: write the cell (SeqCst).
    pub(crate) fn builtin_atomic_store(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_atomic_swap(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_atomic_cas(args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_atomic_add(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_add", args)?;
        Ok(Value::I64(atomic.cell.fetch_add(value, Ordering::SeqCst)))
    }

    /// `atomic_sub(a atomic_i64, v i64) -> i64`: fetch-and-sub, returning the
    /// PREVIOUS value (SeqCst).
    pub(crate) fn builtin_atomic_sub(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_sub", args)?;
        Ok(Value::I64(atomic.cell.fetch_sub(value, Ordering::SeqCst)))
    }

    /// `atomic_and(a atomic_i64, v i64) -> i64`: fetch-and-and, returning the
    /// PREVIOUS value (SeqCst).
    pub(crate) fn builtin_atomic_and(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_and", args)?;
        Ok(Value::I64(atomic.cell.fetch_and(value, Ordering::SeqCst)))
    }

    /// `atomic_or(a atomic_i64, v i64) -> i64`: fetch-and-or, returning the
    /// PREVIOUS value (SeqCst).
    pub(crate) fn builtin_atomic_or(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_or", args)?;
        Ok(Value::I64(atomic.cell.fetch_or(value, Ordering::SeqCst)))
    }

    /// `atomic_xor(a atomic_i64, v i64) -> i64`: fetch-and-xor, returning the
    /// PREVIOUS value (SeqCst).
    pub(crate) fn builtin_atomic_xor(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_xor", args)?;
        Ok(Value::I64(atomic.cell.fetch_xor(value, Ordering::SeqCst)))
    }

    /// Shared argument-decoding for the `atomic_<op>(a atomic_i64, v i64)`
    /// fetch-and-op family: exactly two arguments, an atomic handle then an
    /// `i64` operand.
    pub(crate) fn atomic_binary_args(
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
    pub(crate) fn register_socket(&mut self, resource: SocketResource) -> Value {
        self.sockets.push(Some(resource));
        Value::Socket(self.sockets.len() - 1)
    }

    /// Resolve a socket handle argument to its live slot index, reporting a
    /// wrong-argument-type error for a non-socket value and a stale-handle error
    /// for a closed or invalid slot.
    pub(crate) fn socket_slot(&self, name: &str, value: &Value) -> Result<usize, RuntimeError> {
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
    pub(crate) fn register_process(&mut self, resource: ProcessResource) -> Value {
        self.processes.push(Some(resource));
        Value::Process(self.processes.len() - 1)
    }

    /// Resolve a process handle argument to its live slot index. Mirrors
    /// `socket_slot` and the AST interpreter's `process_slot`.
    pub(crate) fn process_slot(&self, name: &str, value: &Value) -> Result<usize, RuntimeError> {
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
    pub(crate) fn builtin_proc_spawn(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_proc_wait(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_proc_stdout(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_proc_stderr(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_proc_kill(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_tcp_connect(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_tcp_listen(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_tcp_accept(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_tcp_accept_nb(
        &mut self,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_tcp_read(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_tcp_read_nb(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_tcp_write(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_tcp_shutdown(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_socket_close(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_set_nonblocking(
        &mut self,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_udp_bind(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_udp_send_to(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_udp_recv(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_udp_recv_nb(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
    pub(crate) fn builtin_http_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [url]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("http_get", 1, args.len()))?;
        let url = expect_string("http_get", url)?;
        Ok(http_exchange("GET", &url, None))
    }

    /// `http_post(url string, body string) -> result<string, string>`: perform
    /// an HTTP/1.1 POST with a `text/plain` body and return the response body on
    /// a 2xx/3xx response, or `err(message)` on a connection/parse/HTTP error.
    pub(crate) fn builtin_http_post(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [url, body]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("http_post", 2, args.len()))?;
        let url = expect_string("http_post", url)?;
        let body = expect_string("http_post", body)?;
        Ok(http_exchange("POST", &url, Some(&body)))
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

    /// `array_fill(n, x) -> array<T>`: a new array of length `n`, every element
    /// `x` (matches the AST runtime; a negative length is `L0433`).
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

    /// `sort(l list<i64>) -> list<i64>`: a new list sorted ascending.
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
    /// their input order. The comparator's error, if any, is propagated. Mirrors
    /// the AST runtime.
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
    /// `Value::Closure`) with `args`, reusing the same call/closure machinery as
    /// direct dispatch and `parallel_map`. Shared by the higher-order list
    /// builtins so closures capture correctly. Mirrors the AST runtime.
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

    /// `sign(x) -> i64`: `-1`/`0`/`1`; f64 `NaN`/`-0.0` map to `0`.
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

    /// `gcd(a, b) -> i64`: non-negative greatest common divisor (total at
    /// `i64::MIN`; see `gcd_i64`).
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

    /// `list_sum(l) -> T`: wrapping i64 / f64 sum; empty -> `0`/`0.0`.
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

    /// Shared implementation for the unary `f64 -> f64` math builtins, matching
    /// the AST runtime so every backend produces bit-identical results.
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
    /// positions, matching the AST runtime so every backend agrees.
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
    /// positions, matching the AST runtime.
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
            Ok(Value::Ptr(slot))
        } else {
            Err(RuntimeError::new(
                "L0406",
                format!("invalid reference-counted handle `{slot}`"),
            ))
        }
    }

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
