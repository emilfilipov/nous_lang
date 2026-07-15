//! The AST interpreter's actor scheduler (stage 1: `spawn` + `tell`; stage 2:
//! `ask` + `await` + `Future<R>`).
//!
//! Actors run on a single-threaded, cooperative, deterministic scheduler. `spawn`
//! constructs an actor (zero-initializing its private `state`, then running its
//! `init`) and returns a typed [`Value::ActorRef`] handle. `tell` enqueues a
//! fire-and-forget message on a global FIFO mailbox; `ask` enqueues a request
//! carrying a one-shot reply slot and hands back a `Future<R>`
//! ([`Value::ActorFuture`]). `await` on that future drives the mailbox until the
//! slot is filled by the target handler's reply.
//!
//! **Turn model — non-reentrant run-to-completion.** An actor is *busy* for the
//! whole span of a message turn (including any nested `await`s). The scheduler
//! only ever runs the first *deliverable* message — one whose target actor is not
//! busy — so a single actor never runs two turns at once and its `state` stays a
//! single-writer resource with no data races. When nothing is on the stack (the
//! graceful drain before `main` returns), every message is deliverable, so the
//! order is plain FIFO and fully deterministic.
//!
//! **Ordering & fulfillment.** `await f` repeatedly runs the next deliverable
//! message until `f`'s reply slot is filled, then takes it. Because dispatch is
//! FIFO over deliverable messages on one thread, the sequence of turns — and thus
//! every reply and side effect — is identical on every run.
//!
//! **Deadlock.** If an `await` can only be satisfied by re-entering a busy actor
//! (a request cycle, e.g. A asks B while B is awaiting a reply from A), no message
//! is deliverable and the awaited slot can never fill. That is reported as a clean,
//! deterministic runtime error (`L0356`) rather than hanging.
//!
//! This is the AST-interpreter runtime; the IR/bytecode backends reject an actor
//! program (`L0355`) and the native/WASM backends cleanly skip it, so actors run
//! only here.

use lullaby_parser::{ActorDecl, ActorHandler, StructField, TypeRef};

use super::*;

impl<'a> Runtime<'a> {
    /// `spawn NAME(args)`: allocate an actor instance with zero-initialized
    /// state, run its `init` (if any) with `args`, register it on the scheduler,
    /// and return its handle. Semantics has already checked the actor exists and
    /// the argument count/types match `init`, so the runtime guards here are
    /// defensive.
    pub(crate) fn spawn_actor(
        &mut self,
        actor_name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let decl: &'a ActorDecl = match self.actors.get(actor_name).copied() {
            Some(decl) => decl,
            None => {
                return Err(RuntimeError::new(
                    "L0401",
                    format!("`spawn` of unknown actor `{actor_name}`"),
                ));
            }
        };
        // Zero-initialize every state field before `init` runs. A well-formed
        // `init` overwrites the fields it needs; the zero is the value a handler
        // would see for any field the author leaves unset.
        let state: Vec<(String, Value)> = decl
            .state
            .iter()
            .map(|field| (field.name.clone(), self.zero_value(&field.ty)))
            .collect();
        let id = self.actor_instances.len();
        self.actor_instances.push(ActorInstance {
            actor_name: actor_name.to_string(),
            state,
        });

        match &decl.init {
            Some(init) => {
                let param_names: Vec<String> =
                    init.params.iter().map(|param| param.name.clone()).collect();
                self.run_actor_turn(id, &decl.state, &param_names, &init.body, args)?;
            }
            None if args.is_empty() => {}
            None => {
                return Err(RuntimeError::new(
                    "L0402",
                    format!(
                        "actor `{actor_name}` declares no `init` but `spawn` was given {} argument(s)",
                        args.len()
                    ),
                ));
            }
        }
        Ok(Value::ActorRef(id))
    }

    /// `tell TARGET.HANDLER(args)`: enqueue a fire-and-forget message on the
    /// target actor's mailbox and return `void`. The message is processed later,
    /// during the graceful drain before `main` returns.
    pub(crate) fn tell_actor(
        &mut self,
        target: Value,
        handler: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let Value::ActorRef(actor_id) = target else {
            return Err(RuntimeError::new(
                "L0401",
                format!("`tell` target is not an actor handle: `{target}`"),
            ));
        };
        if actor_id >= self.actor_instances.len() {
            return Err(RuntimeError::new(
                "L0401",
                format!("`tell` to unknown actor handle `{actor_id}`"),
            ));
        }
        self.actor_mailbox.push_back(ActorMessage {
            actor_id,
            handler: handler.to_string(),
            args,
            reply_slot: None,
        });
        Ok(Value::Void)
    }

    /// `ask TARGET.HANDLER(args)`: enqueue a request-reply message on the target
    /// actor's mailbox and return a `Future<R>` ([`Value::ActorFuture`]) for the
    /// reply. A fresh one-shot reply slot is allocated; the handler's turn writes
    /// its reply value into it, and `await` on the returned future drives the
    /// scheduler until the slot is filled.
    pub(crate) fn ask_actor(
        &mut self,
        target: Value,
        handler: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let Value::ActorRef(actor_id) = target else {
            return Err(RuntimeError::new(
                "L0401",
                format!("`ask` target is not an actor handle: `{target}`"),
            ));
        };
        if actor_id >= self.actor_instances.len() {
            return Err(RuntimeError::new(
                "L0401",
                format!("`ask` to unknown actor handle `{actor_id}`"),
            ));
        }
        let slot = self.actor_reply_slots.len();
        self.actor_reply_slots.push(None);
        self.actor_mailbox.push_back(ActorMessage {
            actor_id,
            handler: handler.to_string(),
            args,
            reply_slot: Some(slot),
        });
        Ok(Value::ActorFuture(slot))
    }

    /// `await` on an actor request-reply future: drive the mailbox until the
    /// awaited reply slot is filled, then take and return the reply value. Each
    /// iteration runs the next *deliverable* message (one whose target actor is not
    /// mid-turn). If no message is deliverable and the slot is still empty, the
    /// reply can never be produced — a deterministic deadlock reported as `L0356`
    /// rather than a hang.
    pub(crate) fn await_actor_future(
        &mut self,
        slot: usize,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        loop {
            if let Some(reply) = self.actor_reply_slots.get_mut(slot).and_then(Option::take) {
                return Ok(reply);
            }
            if !self.run_one_deliverable()? {
                return Err(RuntimeError::new(
                    "L0356",
                    "`await` can never complete: the awaited actor reply cannot be produced because no queued message is deliverable (a request cycle re-enters an actor that is already running) — this is a deterministic deadlock",
                )
                .with_span(span));
            }
        }
    }

    /// Drain the actor mailbox: run every outstanding message to completion, one
    /// at a time, until nothing is deliverable. At the top-level graceful drain no
    /// actor is busy, so every message is deliverable and the queue empties fully
    /// in FIFO order. A handler may itself `tell`/`ask`/`spawn` during its turn,
    /// appending to the queue; the loop continues until it is empty.
    pub(crate) fn drain_actors(&mut self) -> Result<(), RuntimeError> {
        while self.run_one_deliverable()? {}
        Ok(())
    }

    /// Run the first *deliverable* mailbox message — the earliest queued message
    /// whose target actor is not currently running a turn — to completion. Returns
    /// `true` if one ran, `false` if none is deliverable (the queue is empty, or
    /// every pending message targets a busy actor). Skipping a message bound for a
    /// busy actor preserves per-target FIFO (that actor's messages stay in order,
    /// merely delayed) while upholding the non-reentrant run-to-completion
    /// guarantee.
    fn run_one_deliverable(&mut self) -> Result<bool, RuntimeError> {
        let Some(index) = self
            .actor_mailbox
            .iter()
            .position(|message| !self.busy_actors.contains(&message.actor_id))
        else {
            return Ok(false);
        };
        // `index` was just computed from the live queue, so the removal resolves.
        let message = self
            .actor_mailbox
            .remove(index)
            .expect("deliverable index is valid");
        self.dispatch_message(message)?;
        Ok(true)
    }

    /// Run one mailbox message on its target actor: locate the handler on the
    /// actor's declaration, run its body as a turn over the actor's state, and —
    /// for an `ask` request — write the turn's reply value into its reply slot.
    fn dispatch_message(&mut self, message: ActorMessage) -> Result<(), RuntimeError> {
        let actor_name = self.actor_instances[message.actor_id].actor_name.clone();
        let decl: &'a ActorDecl = match self.actors.get(actor_name.as_str()).copied() {
            Some(decl) => decl,
            None => {
                return Err(RuntimeError::new(
                    "L0401",
                    format!("actor instance references unknown actor `{actor_name}`"),
                ));
            }
        };
        let handler: &'a ActorHandler = decl
            .handlers
            .iter()
            .find(|handler| handler.name == message.handler)
            .ok_or_else(|| {
                RuntimeError::new(
                    "L0401",
                    format!("actor `{actor_name}` has no handler `{}`", message.handler),
                )
            })?;
        let param_names: Vec<String> = handler
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect();
        let reply = self.run_actor_turn(
            message.actor_id,
            &decl.state,
            &param_names,
            &handler.body,
            message.args,
        )?;
        // For an `ask` request, the handler's turn value is the reply: publish it
        // into the reply slot so an `await` on the corresponding future resolves.
        if let Some(slot) = message.reply_slot {
            self.actor_reply_slots[slot] = Some(reply);
        }
        Ok(())
    }

    /// Run one actor turn and return its value (the handler/`init` block's final
    /// value — the reply value for an `ask` handler, `void` otherwise). Marks the
    /// actor busy for the whole turn (including nested `await`s) so no second turn
    /// for the same actor can start, upholding the non-reentrant run-to-completion
    /// guarantee; the busy mark is cleared on every exit path.
    fn run_actor_turn(
        &mut self,
        id: usize,
        state_fields: &[StructField],
        param_names: &[String],
        body: &'a [Stmt],
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        self.busy_actors.insert(id);
        let result = self.run_actor_turn_inner(id, state_fields, param_names, body, args);
        self.busy_actors.remove(&id);
        result
    }

    /// The body of a single actor turn (see [`Runtime::run_actor_turn`], which
    /// wraps this with the busy guard): take the instance's state out, bind it
    /// (plus the handler/`init` parameters) into a fresh environment, evaluate the
    /// body to completion, read the (possibly mutated) state fields back into the
    /// instance, and return the block's value. Taking the state out for the turn is
    /// safe because an actor runs at most one turn at a time (enforced by the busy
    /// set), so nothing else can observe the instance mid-turn.
    fn run_actor_turn_inner(
        &mut self,
        id: usize,
        state_fields: &[StructField],
        param_names: &[String],
        body: &'a [Stmt],
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        if param_names.len() != args.len() {
            return Err(RuntimeError::new(
                "L0402",
                format!(
                    "actor handler expects {} argument(s) but got {}",
                    param_names.len(),
                    args.len()
                ),
            ));
        }
        let current = std::mem::take(&mut self.actor_instances[id].state);
        let mut env = Env::default();
        for (name, value) in current {
            env.define(name, value);
        }
        // Parameters shadow a state field of the same name for the turn's scope.
        for (name, value) in param_names.iter().zip(args) {
            env.define(name.clone(), value);
        }

        let control = self.eval_block(body, &mut env)?;
        let reply = match control {
            Control::Value(value) | Control::Return(value) => value,
            Control::Break | Control::Continue => {
                return Err(RuntimeError::new(
                    "L0410",
                    "loop control escaped an actor handler body",
                ));
            }
        };

        // Read the state fields back out of the environment. Every field was
        // bound at turn start, so each read resolves.
        let mut new_state = Vec::with_capacity(state_fields.len());
        for field in state_fields {
            new_state.push((field.name.clone(), env.get(&field.name)?));
        }
        self.actor_instances[id].state = new_state;
        Ok(reply)
    }

    /// A type-appropriate zero value for an actor `state` field, used to
    /// initialize the field before `init` runs. Scalars get their numeric/empty
    /// zero, `string`/`char`/`byte` their empty/NUL/`0`, growable/`array`
    /// collections an empty container, `map` an empty map, `option<T>` `none`,
    /// and a struct a recursively zero-initialized value. Any other type (an
    /// `enum`, `result`, reference/pointer handle, `Actor<T>`, or function value)
    /// has no natural zero, so it defaults to `void`; a well-formed `init` sets
    /// such a field before any handler reads it.
    fn zero_value(&self, ty: &TypeRef) -> Value {
        match ty.name.as_str() {
            "i64" => return Value::I64(0),
            "f64" => return Value::F64(0.0),
            "f32" => return Value::F32(0.0),
            "bool" => return Value::Bool(false),
            "string" => return Value::String(String::new().into()),
            "char" => return Value::Char('\0'),
            "byte" => return Value::Byte(0),
            "i8" => return Value::int(0, IntKind::I8),
            "i16" => return Value::int(0, IntKind::I16),
            "i32" => return Value::int(0, IntKind::I32),
            "u8" => return Value::int(0, IntKind::U8),
            "u16" => return Value::int(0, IntKind::U16),
            "u32" => return Value::int(0, IntKind::U32),
            "u64" => return Value::int(0, IntKind::U64),
            "isize" => return Value::int(0, IntKind::Isize),
            "usize" => return Value::int(0, IntKind::Usize),
            _ => {}
        }
        if ty.list_element().is_some() || ty.array_element().is_some() {
            return Value::Array(Vec::new().into());
        }
        if ty.map_args().is_some() {
            return Value::Map(Box::default());
        }
        if ty.option_element().is_some() {
            return option_value(None);
        }
        // A non-generic user struct: build a value with each field zeroed.
        if let Some(decl) = self
            .program
            .structs
            .iter()
            .find(|decl| decl.name == ty.name && decl.type_params.is_empty())
        {
            let fields = decl
                .fields
                .iter()
                .map(|field| (field.name.clone(), self.zero_value(&field.ty)))
                .collect();
            return Value::Struct(Box::new(StructValue {
                name: decl.name.clone(),
                fields,
            }));
        }
        Value::Void
    }
}
