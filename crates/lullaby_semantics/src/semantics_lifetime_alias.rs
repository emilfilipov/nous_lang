//! Freed-resource tracking for the `L0350` lifetime check, including **direct
//! aliases** of a freed binding.
//!
//! Kept out of `lib.rs` (already over the size cap) as a cohesive unit: `lib.rs`'s
//! `walk_lifetimes` drives the traversal and this module owns the state.
//!
//! # The hole this closes
//!
//! `L0350` tracked freed *names* only, so a copy escaped it entirely:
//!
//! ```text
//! let p = alloc(8)
//! let q = p          # q and p are the same box
//! dealloc(p)
//! ptr_read(q)        # used to type-check; fails at RUN time with L0406
//! ```
//!
//! That is why native `dealloc` skips cleanly today rather than lowering to
//! `rc_free`: with this hole, `rc_free` would turn a *detected* interpreter error
//! into a silent read of free-list memory, and a double free through an alias would
//! push one block onto the free list twice, making it cyclic. See
//! `native_object_heapbox.rs`.
//!
//! # Scope — what this is, and what it is NOT
//!
//! This is **not** alias analysis. It tracks exactly one shape: a **direct
//! binding-to-binding copy** (`let q = p` / `q = p`), where the initializer is
//! literally a variable reference. That is the obvious case, it is sound to close,
//! and it is closed. Aliases created any other way are still **not** tracked, and
//! the checker does not pretend otherwise:
//!
//! * through a call — `let q = identity(p)`, or a box returned by a helper;
//! * through an aggregate — storing into a struct field, array element, or a `list`;
//! * across a function boundary — passing the box to a callee that frees it;
//! * through the raw-pointer surface — `ptr_cast`/`int_to_ptr` of a box's address.
//!
//! Those remain runtime-detected (`L0406`) on the interpreters. Closing them needs
//! real interprocedural alias analysis, which is out of scope here.
//!
//! Aliasing is treated as **symmetric and transitive** over copies, because a copy
//! makes two names denote one resource: `let q = p; let r = q` puts `p`/`q`/`r` in
//! one group, and freeing *any* of them kills *all* of them. Re-binding a name
//! (`q = alloc(1)`) detaches just that name from its group and revives it.

use std::collections::{HashMap, HashSet};

use lullaby_parser::{Expr, ExprKind};

/// The set of bindings whose resource has been freed, plus the copy-alias groups
/// that make a free reach more than the name it was written on.
///
/// Cloned per nested block by `walk_lifetimes`, mirroring the existing
/// straight-line treatment of branches and loops (a free inside a branch does not
/// escape it — conservative, and deliberately so: it avoids false positives on a
/// conditional free).
#[derive(Clone, Default)]
pub(crate) struct FreedTracker {
    /// name -> the binding whose `dealloc`/`rc_release` actually killed it. For a
    /// directly-freed name that is the name itself; for an alias it is the name the
    /// free was written on, which is what the diagnostic should point the user at.
    freed: HashMap<String, String>,
    /// name -> the other names that denote the same resource (symmetric).
    aliases: HashMap<String, HashSet<String>>,
}

impl FreedTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record `let dest = value` / `dest = value`. A direct variable initializer
    /// makes `dest` an alias of that binding; anything else merely rebinds `dest`.
    /// Either way `dest` is detached from any group it was in and revived, so
    /// reallocating after a free is clean.
    pub(crate) fn record_binding(&mut self, dest: &str, value: &Expr) {
        self.detach(dest);
        let ExprKind::Variable(source) = &value.kind else {
            return;
        };
        if source == dest {
            return;
        }
        // `dest` joins `source`'s group: every existing member, plus `source`.
        let mut group = self.aliases.get(source).cloned().unwrap_or_default();
        group.insert(source.clone());
        group.remove(dest);
        for member in &group {
            self.aliases
                .entry(member.clone())
                .or_default()
                .insert(dest.to_string());
        }
        self.aliases.insert(dest.to_string(), group);
        // A copy of a freed binding is itself dead on arrival: `let q = p` after
        // `dealloc(p)` gives `q` a dangling resource. The *use* of `p` in the
        // initializer is reported separately by the caller.
        if let Some(origin) = self.freed.get(source).cloned() {
            self.freed.insert(dest.to_string(), origin);
        }
    }

    /// Record `dealloc(target)` / `rc_release(target)`, killing `target` and every
    /// name that aliases it. Returns the origin binding when `target` was **already**
    /// freed (a double free), for the caller to report.
    pub(crate) fn record_free(&mut self, target: &str) -> Option<String> {
        let already = self.freed.get(target).cloned();
        self.freed.insert(target.to_string(), target.to_string());
        if let Some(group) = self.aliases.get(target).cloned() {
            for member in group {
                self.freed.insert(member, target.to_string());
            }
        }
        already
    }

    /// The binding whose free killed `name`, or `None` when `name` is live. When the
    /// origin differs from `name`, `name` is an alias and the free was written on the
    /// origin.
    pub(crate) fn freed_origin(&self, name: &str) -> Option<&str> {
        self.freed.get(name).map(String::as_str)
    }

    /// Detach `name` from its alias group and revive it — a rebind gives the name a
    /// fresh resource, so it neither aliases its old group nor stays freed.
    fn detach(&mut self, name: &str) {
        self.freed.remove(name);
        let Some(group) = self.aliases.remove(name) else {
            return;
        };
        for member in group {
            if let Some(members) = self.aliases.get_mut(&member) {
                members.remove(name);
            }
        }
    }
}

/// The `L0350` message for using `name` after the resource was freed, naming the
/// binding the free was actually written on when `name` only aliases it.
///
/// The alias wording is deliberately **direction-neutral** ("shares its resource
/// with"). Copying is symmetric here: in `let q = p; let r = q; dealloc(r)` the free
/// is written on `r`, but a later use of `p` is equally dead — and `p` was not
/// copied *from* `r`, so any phrasing that implies a direction would be a lie in one
/// of the two cases.
pub(crate) fn use_after_free_message(name: &str, origin: &str) -> String {
    if name == origin {
        format!("`{name}` is used after it was freed")
    } else {
        format!(
            "`{name}` is used after `{origin}`, which shares its resource, was freed; copying a \
             pointer copies the address, not the resource, so freeing either kills both"
        )
    }
}

/// The `L0350` message for freeing `name` when the resource is already dead, naming
/// the binding the earlier free was written on when `name` only aliases it.
pub(crate) fn double_free_message(name: &str, origin: &str) -> String {
    if name == origin {
        format!("`{name}` is used after it was already freed")
    } else {
        format!(
            "`{name}` frees the resource it shares with `{origin}`, which was already freed; \
             copying a pointer copies the address, not the resource, so this frees it twice"
        )
    }
}
