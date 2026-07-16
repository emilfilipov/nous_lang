//! The builtin implementations and socket/process resource helpers of the AST
//! interpreter's `impl Runtime`. Split out of lib.rs as a separate impl block.

use super::*;

impl<'a> Runtime<'a> {
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
    /// Routes through the shared [`os_random_bytes`] helper so every backend
    /// agrees on behavior.
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
    /// fresh sibling interpreter over the shared `&Program` (heaps are per-thread,
    /// so there is no shared mutable state and no locking). Output order follows
    /// input order, so results are fully deterministic regardless of scheduling.
    pub(crate) fn builtin_parallel_map(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [callee, elements]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("parallel_map", 2, args.len()))?;
        // `parallel_map` accepts either a named function value or a capturing
        // closure. A closure is self-contained (it carries its captured snapshot,
        // all `Send`) and the worker's fresh interpreter rebuilds the same
        // id-keyed body table from the shared program, so invoking it there is
        // sound and stays order-deterministic.
        let callable = match callee {
            Value::Func(name) => ParallelCallable::Func((name).into()),
            Value::Closure(closure) => ParallelCallable::Closure(*closure),
            other => {
                return Err(RuntimeError::new(
                    "L0417",
                    format!("parallel_map expects a function but got `{other}`"),
                ));
            }
        };
        let arg_values = expect_list("parallel_map", elements)?;

        let program = self.program;
        let program_arc = &self.program_arc;
        let callable = &callable;
        let results: Vec<Value> = std::thread::scope(|scope| {
            let handles: Vec<_> = arg_values
                .iter()
                .map(|value| {
                    let callable = callable.clone();
                    let value = value.clone();
                    let arc = Arc::clone(program_arc);
                    scope.spawn(move || {
                        let mut runtime = Runtime::new(program, arc)?;
                        match callable {
                            ParallelCallable::Func(name) => {
                                runtime.call_function(&name, vec![value])
                            }
                            ParallelCallable::Closure(closure) => {
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
        // A send fails only if every receiver has been dropped. Because a channel
        // shares its receiver behind an `Arc`, the sender-side handle keeps it
        // alive; report a clear runtime error rather than panicking otherwise.
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
    /// a detached OS thread that owns a share of the program (an `Arc<Program>`
    /// clone) and builds its own interpreter over `&*arc`, then returns a one-shot
    /// `Task` handle so the thread is `join`ed exactly once.
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
        // `spawn`'s fixed arg shape is `(Chan, i64)`; validate before detaching.
        let chan = expect_chan("spawn", chan)?;
        let value = expect_i64("spawn", value)?;
        // Hand the detached thread an owned share of the program so it can outlive
        // this call and build its own interpreter over `&*arc` independently.
        let arc = Arc::clone(&self.program_arc);
        let handle = std::thread::spawn(move || {
            let mut runtime = Runtime::new(&arc, Arc::clone(&arc))?;
            runtime.call_function(&func_name, vec![Value::Chan(chan), Value::I64(value)])
        });
        Ok(Value::Task(Task {
            handle: Arc::new(Mutex::new(Some(handle))),
        }))
    }

    /// `task_join(t Task) -> void`: wait for the spawned thread. A second
    /// `task_join` on an already-joined handle is a harmless no-op. (Named
    /// `task_join` rather than `join` because `join` is already the string-list
    /// joiner builtin.)
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
            cell: Arc::new(Mutex::new(value)),
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
    /// `expected` on success), matching C11's value-returning shape. SeqCst on
    /// both success and failure.
    pub(crate) fn builtin_atomic_cas(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic, expected, new]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_cas", 3, args.len()))?;
        let atomic = expect_atomic("atomic_cas", atomic)?;
        let expected = expect_i64("atomic_cas", expected)?;
        let new = expect_i64("atomic_cas", new)?;
        // `compare_exchange` returns `Ok(prev)` on success and `Err(current)` on
        // failure; both payloads carry the value that was observed in the cell.
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
    /// PREVIOUS value (SeqCst). Wrapping arithmetic, as `fetch_add` defines.
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
    /// wrapped as a `Value::Process`. Mirrors `register_socket`.
    pub(crate) fn register_process(&mut self, resource: ProcessResource) -> Value {
        self.processes.push(Some(resource));
        Value::Process(self.processes.len() - 1)
    }

    /// Resolve a process handle argument to its live slot index, reporting a
    /// wrong-argument-type error for a non-process value and a stale-handle error
    /// for a reaped/invalid slot. Mirrors `socket_slot`.
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
    /// spawn `cmd` with `args`, piping stdout/stderr so they can be read later.
    /// `ok(handle)` on success, `err(message)` if the process cannot be started
    /// (e.g. the command is not found). Never panics.
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

    /// `proc_wait(p process) -> result<i64, string>`: block until the child exits
    /// and return its exit code (`128 + signal` on Unix signal termination).
    /// `err` if the handle is already reaped/invalid or the wait fails.
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

    /// `proc_stdout(p process) -> result<string, string>`: read the child's
    /// captured stdout to end as a lossy UTF-8 string. The pipe is taken out of
    /// the `Child` on first read, so a second call returns an empty string.
    pub(crate) fn builtin_proc_stdout(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        self.proc_read_pipe("proc_stdout", args, PipeKind::Stdout)
    }

    /// `proc_stderr(p process) -> result<string, string>`: like `proc_stdout` but
    /// for the child's captured stderr.
    pub(crate) fn builtin_proc_stderr(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        self.proc_read_pipe("proc_stderr", args, PipeKind::Stderr)
    }

    /// Shared body of `proc_stdout`/`proc_stderr`: take the requested pipe out of
    /// the child and read it to end.
    pub(crate) fn proc_read_pipe(
        &mut self,
        name: &'static str,
        args: Vec<Value>,
        kind: PipeKind,
    ) -> Result<Value, RuntimeError> {
        use std::io::Read;
        let [proc]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        let slot = self.process_slot(name, &proc)?;
        let Some(resource) = self.processes[slot].as_mut() else {
            return Ok(result_value(Err(Value::String(
                format!("{name} requires a live process").into(),
            ))));
        };
        let mut buffer = String::new();
        let read = match kind {
            PipeKind::Stdout => resource
                .child
                .stdout
                .take()
                .map(|mut pipe| pipe.read_to_string(&mut buffer)),
            PipeKind::Stderr => resource
                .child
                .stderr
                .take()
                .map(|mut pipe| pipe.read_to_string(&mut buffer)),
        };
        match read {
            // Pipe already drained (or was never captured): report EOF.
            None => Ok(result_value(Ok(Value::String((String::new()).into())))),
            Some(Ok(_)) => Ok(result_value(Ok(Value::String((buffer).into())))),
            Some(Err(error)) => Ok(result_value(Err(Value::String((error.to_string()).into())))),
        }
    }

    /// `proc_kill(p process) -> result<i64, string>`: kill the child, returning
    /// `ok(0)` on success. Killing an already-exited child still succeeds.
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
    /// buffered response is delivered before the socket is dropped. Shutting down
    /// a non-stream or already-closed handle is a no-op.
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

    /// `tcp_close(conn Socket) -> void` / `udp_close`: drop the handle, freeing
    /// its table slot. Closing an already-closed handle is a no-op.
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

    /// Execute a user function (or trait impl method) with the given argument
    /// values, threading the traceback and translating loop-control escape.
    pub(crate) fn invoke_function(
        &mut self,
        function: &'a Function,
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

        // Borrow a reset environment from the pool (or make a fresh one) instead
        // of allocating per call; it is returned to the pool on the normal exit
        // path below.
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
        // Open a raw-pointer frame around the call. This is what lets an `addr_of`
        // pointer tell "my place is reachable right here" from "my place belongs to
        // some other frame's `env`, which I cannot see" — the callee's locals are a
        // different `Env` object, so a caller's place-backed pointer must refuse to
        // resolve while we are inside. Regions created by this frame die on the way
        // out. See `raw_pointer.rs`.
        let outer_frame = self.raw_ptrs.enter_frame();
        let result = self.eval_block(&function.body, &mut env);
        self.raw_ptrs.exit_frame(outer_frame);

        // Attach the traceback lazily. `with_traceback` records only the first
        // (innermost) stack — every later frame's attach is a no-op — so eagerly
        // cloning `call_stack` on every successful call, and on every frame an
        // error merely passes through, is pure waste. Clone it only when this
        // frame is the one first recording a traceback, while the frame is still
        // on the stack so it is included.
        //
        // A postfix `?` on a `none`/`err` unwinds to here as the `L0430`
        // sentinel, carrying the failure value in `pending_try_return`. Catch it
        // at this call boundary and turn it into a normal function return of that
        // value (this is the function-level early return `?` denotes). The slot
        // is always taken, so it never leaks into a later call.
        let control = match result {
            Err(error) if error.code == "L0430" => {
                self.call_stack.pop();
                let value = self.pending_try_return.take().ok_or_else(|| {
                    RuntimeError::new(
                        "L0430",
                        "missing `?` propagation value at function boundary",
                    )
                })?;
                return Ok(value);
            }
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
        // Normal exit: return the environment to the pool for the next call. The
        // early-return paths above simply drop theirs.
        self.env_pool.push(env);

        match control {
            Control::Return(value) | Control::Value(value) => Ok(value),
            Control::Break | Control::Continue => Err(RuntimeError::new(
                "L0410",
                "loop control escaped function body",
            )),
        }
    }

    /// Invoke a closure value: look its body up in the id-keyed closure table,
    /// push a fresh scope, bind the captured snapshot first and then the
    /// parameters (so parameters shadow captured names of the same identifier),
    /// evaluate the single-expression body, and return the produced value. The
    /// closure is self-contained (its captured values travel with it), so it runs
    /// against a fresh environment rather than the caller's.
    pub(crate) fn invoke_closure(
        &mut self,
        closure: &Closure,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        // Copy the body pointer and parameter names out of the table so the
        // borrow of `self.closures` ends before the `&mut self` evaluation.
        let (param_names, body) = match self.closures.get(&closure.id) {
            Some((names, body)) => (names.clone(), *body),
            None => {
                return Err(RuntimeError::new(
                    "L0402",
                    format!("closure #{} has no registered body", closure.id),
                ));
            }
        };
        if param_names.len() != args.len() {
            return Err(RuntimeError::new(
                "L0402",
                format!(
                    "closure expects {} arguments but got {}",
                    param_names.len(),
                    args.len()
                ),
            ));
        }

        let mut env = Env::default();
        // Captured bindings first, then parameters (parameters shadow captures).
        for (name, value) in &closure.captured {
            env.define(name.clone(), value.clone());
        }
        for (name, value) in param_names.iter().zip(args) {
            env.define(name.clone(), value);
        }
        // A closure body is its own frame: its locals live in this `Env`, so an
        // `addr_of` taken here must not stay resolvable once the closure returns.
        // See `raw_pointer.rs`.
        let outer_frame = self.raw_ptrs.enter_frame();
        let result = self.eval_expr(body, &mut env);
        self.raw_ptrs.exit_frame(outer_frame);
        result
    }

    pub(crate) fn eval_block(
        &mut self,
        statements: &[Stmt],
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

    pub(crate) fn eval_statement(
        &mut self,
        statement: &Stmt,
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        let span = statement_span(statement);
        let result = match statement {
            Stmt::Let { name, value, .. } => {
                // A `match` in value position is evaluated against the real
                // (mutable) environment — exactly like a statement `match` — so its
                // arm blocks may mutate outer bindings and any `return`/loop control
                // inside an arm propagates, keeping AST parity with the IR/bytecode
                // desugaring (which lowers it to real statements). Evaluating it via
                // `eval_expr` would clone the env and silently drop those effects.
                if let ExprKind::Match { scrutinee, arms } = &value.kind {
                    match self.eval_match(scrutinee, arms, env)? {
                        Control::Value(v) => {
                            env.define(name.clone(), v);
                            Ok(Control::Value(Value::Void))
                        }
                        diverted => Ok(diverted),
                    }
                } else {
                    // Move-on-functional-update fast path: `let x = f(x, …)`
                    // re-binding an existing innermost local consumes it by move,
                    // not clone.
                    let value = match self.try_move_functional_update(name, value, env, true)? {
                        Some(result) => result,
                        None => self.eval_expr(value, env)?,
                    };
                    env.define(name.clone(), value);
                    Ok(Control::Value(Value::Void))
                }
            }
            Stmt::Assign {
                name,
                path,
                op,
                value,
                ..
            } => {
                // A `match` RHS is evaluated on the real env (see the `let` case)
                // so its arm effects survive; a diverted control (`return`/loop)
                // from an arm propagates instead of assigning.
                let match_rhs = if let ExprKind::Match { scrutinee, arms } = &value.kind {
                    match self.eval_match(scrutinee, arms, env)? {
                        Control::Value(v) => Some(v),
                        diverted => return Ok(diverted),
                    }
                } else {
                    None
                };
                if path.is_empty() && matches!(op, AssignOp::Replace) {
                    // Whole-variable reassignment `x = RHS`: try the
                    // move-on-functional-update fast path (`x = f(x, …)`) before
                    // falling back to the ordinary clone path.
                    let new = match match_rhs {
                        Some(v) => v,
                        None => match self.try_move_functional_update(name, value, env, false)? {
                            Some(result) => result,
                            None => self.eval_expr(value, env)?,
                        },
                    };
                    env.assign(name, new)?;
                } else {
                    let rhs = match match_rhs {
                        Some(v) => v,
                        None => self.eval_expr(value, env)?,
                    };
                    if path.is_empty() {
                        let new = apply_compound(env.get(name)?, op, rhs)?;
                        env.assign(name, new)?;
                    } else {
                        // Mutate the element/field in place: borrow the container
                        // mutably instead of cloning the whole array/struct,
                        // mutating a copy, and writing it back (which is O(len) per
                        // write, O(len^2) in a write loop).
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
            Stmt::Return(expr) => {
                // `return match ...`: evaluate the match on the real env (see the
                // `let` case) so its arm effects are kept and its value becomes the
                // function's return value. An arm that itself `return`s also returns
                // that value; loop control escaping an arm propagates unchanged.
                if let Some(Expr {
                    kind: ExprKind::Match { scrutinee, arms },
                    ..
                }) = expr.as_ref()
                {
                    match self.eval_match(scrutinee, arms, env)? {
                        Control::Value(v) | Control::Return(v) => Ok(Control::Return(v)),
                        diverted => Ok(diverted),
                    }
                } else {
                    let value = expr
                        .as_ref()
                        .map(|expr| self.eval_expr(expr, env))
                        .unwrap_or(Ok(Value::Void))?;
                    Ok(Control::Return(value))
                }
            }
            Stmt::Break(_) => Ok(Control::Break),
            Stmt::Continue(_) => Ok(Control::Continue),
            // A `match` arrives wrapped in a `Stmt::Expr`; evaluate it here so its
            // arm blocks propagate control flow and produce a value like
            // `if`/`try`.
            Stmt::Expr(Expr {
                kind: ExprKind::Match { scrutinee, arms },
                ..
            }) => self.eval_match(scrutinee, arms, env),
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

                // Bind the loop variable once in an enclosing scope and update it
                // in place each iteration, rather than re-`define`ing it (which
                // clones the name `String` and reallocates a scope every pass). The
                // body still runs in its own fresh scope so its `let`s are cleared
                // between iterations. The final `pop_scope` always runs (even on an
                // error break) so the scope stack stays balanced for `try`/`catch`.
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
            Stmt::ForEach {
                name,
                iterable,
                body,
                ..
            } => {
                // Iterate `name` over each element of an array/list, or each char
                // of a string. (Semantics guarantees the collection type.)
                let elements: Vec<Value> = match self.eval_expr(iterable, env)? {
                    Value::Array(values) => (values).into(),
                    Value::String(text) => text.chars().map(Value::Char).collect(),
                    _ => {
                        return Err(RuntimeError::new(
                            "L0412",
                            "`for … in` target is not an array, list, or string",
                        ));
                    }
                };
                // Bind the loop variable once and update it in place each pass
                // (via `set_loop_var`) instead of re-`define`ing it (allocating a
                // scope and cloning the name every iteration). The body runs in its
                // own fresh scope so its `let`s clear between iterations, and the
                // final `pop_scope` always runs so the stack stays balanced for
                // `try`/`catch`. (Matches the range-`for` fast path.)
                env.push_scope();
                env.define(name.clone(), Value::Void);
                let mut outcome: Result<Control, RuntimeError> = Ok(Control::Value(Value::Void));
                for element in elements {
                    env.set_loop_var(name, element);
                    env.push_scope();
                    let result = self.eval_block(body, env);
                    env.pop_scope();
                    match result {
                        Ok(Control::Return(value)) => {
                            outcome = Ok(Control::Return(value));
                            break;
                        }
                        Ok(Control::Break) => break,
                        Ok(Control::Continue) | Ok(Control::Value(_)) => {}
                        Err(error) => {
                            outcome = Err(error);
                            break;
                        }
                    }
                }
                env.pop_scope();
                outcome
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
            // Inline assembly emits raw machine code and can only run after native
            // codegen + linking; the AST interpreter cannot execute it, so reject
            // it with `L0425` (like `extern`'s `L0423`) rather than no-op.
            Stmt::Asm { .. } => Err(asm_interpreter_error()),
            // A **static-buffer arena** (`region N in buf`, freestanding tier §5)
            // opens a real bump arena over the caller's buffer. Its whole state is
            // two env bindings — a cell cursor starting at zero and the backing
            // buffer's name — so the arena gets exactly the frame and block lifetime
            // the declaration has, for free. See `arena_cursor_key` for why these
            // keys cannot collide with a user binding.
            Stmt::Region(decl) if decl.backing.is_some() => {
                let backing = decl.backing.clone().unwrap_or_default();
                env.define(arena_cursor_key(&decl.name), Value::I64(0));
                env.define(arena_buffer_key(&decl.name), Value::String(backing.into()));
                Ok(Control::Value(Value::Void))
            }
            // A metadata region declaration is compile-time only; it has no runtime
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
                    env.define(catch_name.clone(), Value::String((error.message).into()));
                    let result = self.eval_block(catch_body, env);
                    env.pop_scope();
                    result
                }
                other => other,
            },
        };
        result.map_err(|error| self.annotate_error(error, span))
    }
}
