//! The AST interpreter's lexical environment. Split out of `interpreter.rs` (which
//! this change would otherwise push past the file-size cap) as a cohesive module,
//! mirroring the IR/bytecode tiers' `ir_env.rs` one-to-one — `Env` is independent of
//! the evaluator that drives it.
//!
//! A behavior-preserving code move: `Scope`/`Env` and every method are unchanged.
//! `Env` carries the monotonic scope ids and the process-unique env id that let the
//! place-backed raw-pointer model name a binding by `RootSlot` rather than by name,
//! and that let the **env shelf** find an ancestor frame's environment by id. See
//! `crate::raw_pointer` for why both matter.

use crate::*;
use std::collections::HashMap;

/// A lexical environment: a stack of scopes, each an insertion-ordered
/// association list of `(name, value)`. Function-call and block scopes are
/// small (a handful of bindings), so a linear-scan `Vec` beats a `HashMap`
/// here — it avoids a per-scope bucket allocation and per-access string
/// hashing, and its contiguous layout is cache-friendly. `define` keeps at
/// most one binding per name per scope (replacing in place, exactly like the
/// previous `HashMap::insert`), so resolution never has to disambiguate
/// duplicates within a scope; shadowing across scopes is innermost-first.
///
/// Each scope carries a monotonic `id`. Ids are **never reused** — not across
/// `push_scope`/`pop_scope`, and not across a pooled `reset` — which is what lets a
/// raw pointer name a binding by [`RootSlot`] (`(scope id, entry index)`) instead of
/// by name: resolution either finds the exact binding whose address was taken, or
/// finds nothing because its scope is gone. Resolving by name would silently follow
/// a nested shadowing `let`. See `raw_pointer.rs`.
#[derive(Debug, Clone)]
pub(crate) struct Scope {
    id: u64,
    entries: Vec<(String, Value)>,
}

#[derive(Debug, Clone)]
pub(crate) struct Env {
    /// Process-unique id of this `Env` object, so a `RootSlot` from a *different*
    /// `Env` (another call's, a closure's, an actor turn's) can never resolve here
    /// even if its scope id and entry happen to coincide. See `RootSlot`.
    id: u64,
    scopes: Vec<Scope>,
    next_scope_id: u64,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            id: crate::raw_pointer::next_env_id(),
            scopes: vec![Scope {
                id: 0,
                entries: Vec::new(),
            }],
            next_scope_id: 1,
        }
    }
}

impl Env {
    /// This environment's process-unique id. The env shelf looks a frame up by the
    /// [`RootSlot::env`] an `addr_of` recorded, which is what lets a callee reach its
    /// caller's locals. Ids are unique among *live* environments: a pooled `Env` is
    /// only reused after its frame returned, so at any instant no two live frames
    /// share one. Mirrors the IR/bytecode `Env`.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// A placeholder environment that owns nothing: no allocation, no scopes, and no
    /// id bump (which would take the process-global atomic on every call).
    ///
    /// Left behind in a caller's `&mut Env` slot while its real environment sits on
    /// the env shelf for the duration of a call. It is never read: the swap back
    /// happens before the caller resumes. Its id is `0`, which
    /// [`crate::raw_pointer::next_env_id`] never hands out, so even a stray
    /// [`RootSlot`] cannot resolve against it.
    pub(crate) fn hollow() -> Self {
        Self {
            id: 0,
            scopes: Vec::new(),
            next_scope_id: 0,
        }
    }

    fn fresh_scope(&mut self) -> Scope {
        let id = self.next_scope_id;
        self.next_scope_id += 1;
        Scope {
            id,
            entries: Vec::new(),
        }
    }

    pub(crate) fn push_scope(&mut self) {
        let scope = self.fresh_scope();
        self.scopes.push(scope);
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Reset to a single empty scope so a pooled environment can be reused for the
    /// next call. Each scope's backing `Vec` keeps its capacity, so a repeated
    /// call re-binds its locals without reallocating; clearing every entry means
    /// no stale binding can leak into the reused environment.
    ///
    /// The surviving scope takes a **fresh id**: a pooled env is a different frame's
    /// storage, and reusing the id would let a raw pointer from the previous
    /// occupant resolve against the new one's bindings.
    pub(crate) fn reset(&mut self) {
        self.scopes.truncate(1);
        let id = self.next_scope_id;
        self.next_scope_id += 1;
        match self.scopes.first_mut() {
            Some(first) => {
                first.entries.clear();
                first.id = id;
            }
            None => self.scopes.push(Scope {
                id,
                entries: Vec::new(),
            }),
        }
    }

    /// Locate the nearest binding of `name` as a stable [`RootSlot`], resolving
    /// innermost-first exactly like [`Env::get`]. This is the `addr_of` half of the
    /// place-backed raw-pointer model: it pins the binding at address-taking time so
    /// later reads/writes through the pointer cannot drift to a different one.
    pub(crate) fn locate(&self, name: &str) -> Option<RootSlot> {
        for scope in self.scopes.iter().rev() {
            for (entry, (existing, _)) in scope.entries.iter().enumerate() {
                if existing == name {
                    return Some(RootSlot {
                        env: self.id,
                        scope: scope.id,
                        entry,
                    });
                }
            }
        }
        None
    }

    /// Borrow the binding a [`RootSlot`] names, or `None` if its scope has since
    /// been popped (the place is dead — a raw pointer to it is dangling).
    pub(crate) fn at(&self, slot: &RootSlot) -> Option<&Value> {
        if slot.env != self.id {
            return None;
        }
        self.scopes
            .iter()
            .find(|scope| scope.id == slot.scope)
            .and_then(|scope| scope.entries.get(slot.entry))
            .map(|(_, value)| value)
    }

    /// Mutable counterpart of [`Env::at`] — the write half of a place-backed
    /// `ptr_write`, which is what makes an `addr_of` pointer genuinely alias.
    pub(crate) fn at_mut(&mut self, slot: &RootSlot) -> Option<&mut Value> {
        if slot.env != self.id {
            return None;
        }
        self.scopes
            .iter_mut()
            .find(|scope| scope.id == slot.scope)
            .and_then(|scope| scope.entries.get_mut(slot.entry))
            .map(|(_, value)| value)
    }

    pub(crate) fn define(&mut self, name: String, value: Value) {
        let scope = &mut self
            .scopes
            .last_mut()
            .expect("env always has a scope")
            .entries;
        // `let` may redefine a name already bound in this scope; replace that
        // binding in place so there is exactly one entry per name per scope
        // (matching the previous `HashMap::insert` semantics).
        for (existing, slot) in scope.iter_mut() {
            if *existing == name {
                *slot = value;
                return;
            }
        }
        scope.push((name, value));
    }

    /// Update the loop variable's binding in the innermost scope in place. The
    /// range-`for` lowering calls this each iteration with the loop-variable scope
    /// innermost (the body scope has been popped), so it never allocates or clones
    /// the name — the hot-path replacement for a per-iteration `define`.
    pub(crate) fn set_loop_var(&mut self, name: &str, value: Value) {
        let scope = &mut self
            .scopes
            .last_mut()
            .expect("env always has a scope")
            .entries;
        for (existing, slot) in scope.iter_mut() {
            if existing == name {
                *slot = value;
                return;
            }
        }
        scope.push((name.to_string(), value));
    }

    /// Borrow the nearest binding of `name` mutably, for in-place mutation of an
    /// element/field (`a[i] = v`, `s.field = v`) without cloning the whole
    /// container and writing it back. Resolves nearest-first like `assign`.
    pub(crate) fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        for scope in self.scopes.iter_mut().rev() {
            for (existing, slot) in scope.entries.iter_mut() {
                if existing == name {
                    return Some(slot);
                }
            }
        }
        None
    }

    pub(crate) fn assign(&mut self, name: &str, value: Value) -> Result<(), RuntimeError> {
        for scope in self.scopes.iter_mut().rev() {
            for (existing, slot) in scope.entries.iter_mut() {
                if existing == name {
                    *slot = value;
                    return Ok(());
                }
            }
        }
        Err(RuntimeError::new(
            "L0403",
            format!("unknown variable `{name}`"),
        ))
    }

    pub(crate) fn get(&self, name: &str) -> Result<Value, RuntimeError> {
        self.get_ref(name)
            .cloned()
            .ok_or_else(|| RuntimeError::new("L0403", format!("unknown variable `{name}`")))
    }

    /// Borrow a binding's value without cloning it, resolving innermost-first
    /// exactly like [`Env::get`]. Used to classify a call target (closure/func
    /// value vs. builtin) on the move-on-functional-update fast path without
    /// paying for a clone.
    pub(crate) fn get_ref(&self, name: &str) -> Option<&Value> {
        for scope in self.scopes.iter().rev() {
            for (existing, value) in scope.entries.iter() {
                if existing == name {
                    return Some(value);
                }
            }
        }
        None
    }

    /// True when `name` is bound in the innermost (current) scope. A `let x =
    /// f(x, …)` re-binding only moves when the consumed binding lives here,
    /// because `let` shadows (defines into the innermost scope) rather than
    /// overwriting an outer binding — moving from an outer scope would corrupt it.
    pub(crate) fn innermost_has(&self, name: &str) -> bool {
        self.scopes
            .last()
            .is_some_and(|scope| scope.entries.iter().any(|(n, _)| n == name))
    }

    /// True when `name` is bound in any scope (a normal local). A plain `x =
    /// f(x, …)` reassignment moves from — and writes back to — the *nearest*
    /// binding, and both [`Env::get`] and [`Env::assign`] resolve nearest-first to
    /// that same slot, so the move is safe at any scope depth (e.g. `x` declared
    /// outside a loop, reassigned inside it).
    pub(crate) fn is_bound(&self, name: &str) -> bool {
        self.get_ref(name).is_some()
    }

    /// Move the value out of the nearest scope binding `name`, leaving a cheap
    /// [`Value::Void`] placeholder in the same slot (no clone), and return the old
    /// value. Nearest-first, matching [`Env::get`]/[`Env::assign`] resolution, so
    /// the caller's write-back overwrites this exact slot. The placeholder is
    /// never observable: on the fast path all other work is already done and the
    /// result is written back immediately, and the gating builtin cannot raise a
    /// catchable error mid-call.
    pub(crate) fn move_out_nearest(&mut self, name: &str) -> Option<Value> {
        for scope in self.scopes.iter_mut().rev() {
            for (existing, slot) in scope.entries.iter_mut() {
                if existing == name {
                    return Some(std::mem::replace(slot, Value::Void));
                }
            }
        }
        None
    }

    /// Snapshot every in-scope local by value: one `(name, value.clone())` per
    /// visible binding, with an inner scope's binding shadowing an outer one of
    /// the same name. This is the frame-capture-by-value used when a closure
    /// literal is evaluated. The order is deterministic (outer-to-inner insertion
    /// with later scopes overwriting), which is all closure invocation needs.
    pub(crate) fn snapshot_locals(&self) -> Vec<(String, Value)> {
        let mut flattened: HashMap<&str, &Value> = HashMap::new();
        // Iterate outermost-to-innermost so an inner scope overwrites an outer
        // binding of the same name.
        for scope in &self.scopes {
            for (name, value) in &scope.entries {
                flattened.insert(name.as_str(), value);
            }
        }
        let mut captured: Vec<(String, Value)> = flattened
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect();
        // Sort by name for a stable, reproducible capture order.
        captured.sort_by(|(a, _), (b, _)| a.cmp(b));
        captured
    }
}
