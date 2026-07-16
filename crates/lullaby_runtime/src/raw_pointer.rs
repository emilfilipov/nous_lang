//! Freestanding-tier raw-pointer *addressing* model for the interpreters — the
//! shared byte-addressed backing that makes `addr_of` / `ptr_offset` / `ptr_cast`
//! behave identically on the AST, IR, and bytecode interpreters (freestanding
//! tier stage 2, `documents/freestanding_tier_design.md` §2.2).
//!
//! # Two address spaces
//!
//! The delivered raw-pointer builtins (`ptr_read`/`ptr_write`/`int_to_ptr`/
//! `ptr_to_int`/`volatile_*`) model an `alloc`-derived `ptr<T>` as an abstract
//! *heap-slot handle*: a single slot holds one `Value`, and `ptr_to_int` returns the
//! slot index. That model has no notion of *adjacent* addresses, so it cannot
//! express pointer arithmetic (`ptr_offset`) or the address-of an array element that
//! a kernel walks. This module adds a second, *byte-addressed* address space that
//! lives **above** [`RAW_POINTER_BASE`] so it never collides with a small heap-slot
//! handle. The `alloc`/`int_to_ptr` heap path is untouched by everything here.
//!
//! # The place-backed model (stage 3)
//!
//! An `addr_of` does **not** copy the addressed value. A [`RawRegion`] is pure
//! *addressing metadata*: a byte `base`, an element `stride` (the C-natural
//! `size_of` of the element), a cell count, and — the point — a **stable coordinate
//! of the place the address refers to**:
//!
//! - the `frame` that owns the root binding (a monotonic interpreter frame id),
//! - a [`RootSlot`] locating the root binding inside that frame's `Env` by
//!   *scope id and entry index* rather than by name, and
//! - the [`ResolvedPlace`] `path` from that root down to the addressed place.
//!
//! Reads and writes therefore go **straight to the original storage**, so an
//! `addr_of` pointer genuinely aliases: `ptr_write(addr_of(x), 5)` makes `x == 5`,
//! and `ptr_read(addr_of(x))` after an independent `x = 99` observes `99`. This is
//! what a real `lea`-based native `addr_of` does, and it retires the stage-2
//! `L0459` store refusal (which existed only because the old region snapshotted the
//! place *by value* and so could not alias).
//!
//! Addressing itself stays **region-backed**: `ptr_offset(p, n)` advances the byte
//! address by `n * stride` and a read/write maps the byte address back to
//! `cell (addr - base) / stride`, which is then appended to the root path as an
//! index. That keeps the observable **size law**
//! `ptr_to_int(ptr_offset(p, 1)) - ptr_to_int(p) == size_of(T)` exact, exactly as it
//! is in real native addressing, while the *storage* is the live place. Hence the
//! hybrid: **place-backed for storage, region-backed for adjacency.**
//!
//! # Why resolution is by scope id, not by name
//!
//! Resolving the root by *name* at access time would silently follow a shadowing
//! re-`let` of that name in a nested scope, addressing a different binding than the
//! one whose address was taken. [`RootSlot`] pins the exact `(scope id, entry
//! index)` the `addr_of` resolved, and an `Env` scope id is never reused, so
//! resolution either finds the original binding or finds nothing at all.
//!
//! # Cross-frame addressing: the env shelf (stage 4)
//!
//! A pointer that leaves the frame that took it — `poke(addr_of(x))`, the
//! out-parameter idiom — used to be refused, because each frame's `Env` lived on the
//! Rust stack and a callee could not reach its caller's. That refusal was honest but
//! wrong-headed: the shapes it rejected are **valid C, not undefined behaviour**
//! (C11 6.2.4p6 ties an automatic object's lifetime to *its block*, and a call does
//! not end the caller's block), and native — where `addr_of` is a real `lea` —
//! compiles them.
//!
//! Stage 4 makes them resolve, via an **env shelf**: an explicit, interpreter-owned
//! stack of the *ancestor* frames' `Env`s. At a call boundary the caller's `Env` is
//! swapped out of its `&mut Env` slot and pushed onto the shelf for the dynamic
//! extent of the call, then swapped back. So a callee reaches its caller's locals by
//! looking the owning `Env` up **by [`RootSlot::env`] id** — the current frame's
//! `&mut Env`, or a shelf entry.
//!
//! Why this shape and not the two obvious alternatives:
//!
//! - **`Rc<RefCell<Env>>`** is cheaper to write but puts a runtime borrow check on
//!   *every variable access* in every program — a hot-path tax paid by code that
//!   never touches a pointer.
//! - **One `Vec<Env>` indexed by frame id**, with the current frame living in it too,
//!   puts a bounds-checked index on every variable access for the same reason.
//!
//! Keeping the **current** frame as a plain `&mut Env` and shelving only *ancestors*
//! avoids both: the hot path is byte-for-byte what it was. The shelf is touched only
//! at a call boundary, and only once the program has actually taken an address —
//! [`RawPointerMemory::shelf_needed`] is false until the first `addr_of`, so a
//! program that never takes one pays a single predictable branch per call and nothing
//! else. This is sound by construction rather than by luck: with no live region, no
//! address can resolve to a place at all, so not shelving is unobservable.
//!
//! # Lifetime: dangling is diagnosed, never guessed
//!
//! A place-backed address is only meaningful while its place exists. Two things end
//! that, and both are **detected** rather than approximated:
//!
//! - **The scope died.** The block holding the root binding was popped, so the
//!   binding is gone. [`RootSlot`] pins a scope id that is never reused, so `Env::at`
//!   finds nothing and the access is refused.
//! - **The frame returned.** [`RawPointerMemory::exit_frame`] drops that frame's
//!   regions, so a pointer that outlived its frame resolves to
//!   [`RawResolve::Dangling`].
//!
//! Both are refused with [`L0459`](../../../documents/diagnostic_registry.md), and
//! both are **genuine program errors**: returning `&local`, or using a pointer to a
//! block that has ended, is undefined behaviour in C, so refusing them forbids no
//! defined program. `L0459` now means exactly that and nothing else.
//!
//! A pointer handed to another **thread** (`spawn`/async/`parallel_map`) is a separate
//! case and not `L0459`: each thread builds its own interpreter with its own
//! [`RawPointerMemory`], so the address names no region there and is refused as
//! unmapped ([`unmapped_raw`], `L0406`).
//!
//! # Honest refusal is the invariant
//!
//! There are exactly two outcomes for a deref: the owning `Env` is found and the
//! access goes to the **real, live storage**, or it is not found and the access is a
//! hard error ([`unreachable_frame`], the fail-closed guard). There is no path that
//! reads a copy — so even a *missed* shelving site could only ever cost a loud
//! refusal, never a wrong answer. This matters because the
//! predecessor of this model *did* read a by-value snapshot, which silently returned
//! the pre-`addr_of` value for `addr_of(x); x = 99; peek(p)` — a wrong answer dressed
//! as a right one. A stale read is the failure mode this model exists to prevent, and
//! no amount of convenience is worth reintroducing it.
//!
//! Note this is not the `volatile_*` situation: `volatile_load`/`volatile_store` are
//! *semantically correct* on the interpreters (only the optimization barrier is
//! unmodelled, and that is unobservable single-threaded).

use crate::{ResolvedPlace, RuntimeError, Value};

/// `L0459` **fail-closed guard**: a live region resolved to a place, but the `Env` that
/// owns it was not reachable — neither the current frame's nor anything on the shelf.
///
/// This is not expected to fire in delivered code, and it is deliberately **not**
/// `unreachable!()` or an `unwrap`. It is the branch that makes the model's central
/// guarantee true by construction rather than by audit: a deref either finds the owning
/// `Env` and touches the **real, live storage**, or it is a **hard error**. There is no
/// third path that reads a copy. If a dispatch site were ever added that runs user code
/// without shelving the calling frame (see `with_env_shelved`), the cost is a loud,
/// specific refusal here — never a silent stale read, which is the failure mode this
/// whole model exists to prevent.
///
/// Note what this is *not*: a pointer handed to **another thread** (`spawn`, an
/// `async fn`, `parallel_map`) does not reach this at all. Each thread builds its own
/// interpreter with its own [`RawPointerMemory`], so the address names no region there
/// and is refused as unmapped ([`unmapped_raw`], `L0406`) — verified, not assumed.
pub fn unreachable_frame(name: &str) -> RuntimeError {
    RuntimeError::new(
        "L0459",
        format!(
            "this `addr_of` pointer refers to `{name}`, whose place is live but whose \
             environment this execution path cannot reach, so it cannot be dereferenced \
             here. Passing an address into a called function works — the callee reads \
             and writes the caller's place for real — so reaching this is unexpected and \
             is refused rather than answered from storage that cannot be verified. If \
             you can reproduce it, please report it: it indicates an interpreter \
             dispatch path that does not shelve its calling frame"
        ),
    )
}

/// `L0459` for a raw address whose place is gone: either its block was popped, or the
/// whole frame that owned it has returned. Reading or writing it would be a dangling
/// access — **undefined behaviour in C**, and a wrong answer here.
///
/// This is a genuine program error, and after the stage-4 env shelf it is essentially
/// what `L0459` means: passing an address into a callee resolves for real now, so what
/// is left refusing is code whose place has actually ceased to exist — a returned
/// `addr_of(local)`, or a pointer to an inner-block or loop-body local used after that
/// block ended.
pub fn dangling_place(name: &str) -> RuntimeError {
    RuntimeError::new(
        "L0459",
        format!(
            "this `addr_of` pointer refers to `{name}`, whose block or function has \
             already ended, so the place no longer exists. Reading or writing it would \
             be a dangling access (undefined behaviour in C — the storage may since have \
             been reused). Keep the pointer inside the block that owns the place, return \
             the value rather than its address, or use an `alloc`-backed `ptr<T>`, which \
             has no frame lifetime"
        ),
    )
}

/// `L0406` for a raw-space address that names no `addr_of` region at all — a bare
/// `int_to_ptr` value (an MMIO register, a fixed physical address) that merely happens
/// to land above [`RAW_POINTER_BASE`]. This is *not* an `addr_of` pointer, so the
/// diagnostic must not blame one.
pub fn unmapped_raw(addr: usize) -> RuntimeError {
    RuntimeError::new(
        "L0406",
        format!(
            "invalid pointer `{addr}`: this address is not backed by any `addr_of` \
             place or `alloc` allocation. The interpreters model memory abstractly and \
             have no real address space, so an address conjured with `int_to_ptr` (an \
             MMIO register, a fixed physical address) cannot be dereferenced — run it \
             on a native freestanding target instead"
        ),
    )
}

/// Base of the interpreters' freestanding raw-pointer byte-address space. Kept far
/// above any plausible heap-slot index (`Vec` indices are small) so an
/// `addr_of`-derived byte address is never confused with an `alloc`/`int_to_ptr`
/// heap-slot handle. `1 << 44` leaves 44 bits of low address space for handles and
/// ample room above for many regions.
pub const RAW_POINTER_BASE: usize = 1usize << 44;

/// A stable, **globally unique** coordinate of a root binding.
///
/// - `env` identifies the `Env` object itself; every `Env` takes a fresh id from a
///   process-global counter at construction.
/// - `scope` is that `Env`'s scope id, allocated monotonically and never reused —
///   not across `push_scope`/`pop_scope`, nor across a pooled `reset`.
/// - `entry` indexes the scope's insertion-ordered binding list. Entries are only
///   appended or replaced in place, never removed or reordered.
///
/// So the triple names the same binding for as long as its scope is alive, and names
/// *nothing* once the scope is popped. Resolving by name instead would silently
/// follow a nested shadowing `let` and address a different binding.
///
/// `env` is what makes this safe by *construction* rather than by convention. Each
/// call gets its own `Env`, and scope ids are only unique within one `Env`; without
/// `env`, a `RootSlot` left over from a closure or actor turn could collide with an
/// unrelated binding sitting at the same scope id and entry in a *different* `Env` —
/// a silent wrong answer. Including `env` means a foreign slot simply fails to
/// resolve, independently of the frame bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootSlot {
    pub env: u64,
    pub scope: u64,
    pub entry: usize,
}

/// Source of process-unique `Env` ids (see [`RootSlot`]). One atomic bump per `Env`
/// *construction* — not per scope — and the interpreters pool their environments, so
/// this is off the hot path. Sibling interpreters on other threads (`parallel_map`)
/// share the counter, so their ids stay disjoint too.
static NEXT_ENV_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Allocate a process-unique id for a freshly constructed `Env`.
pub fn next_env_id() -> u64 {
    NEXT_ENV_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// What a region's cells are, relative to the root path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawShape {
    /// Cells are the elements of the `array` at `path`; cell `k` is `path + [Index(k)]`.
    /// Produced by `addr_of(a[i])` and by whole-array decay `addr_of(a)`.
    ArrayElements,
    /// Exactly one cell: the value at `path` itself. Produced by `addr_of(x)` /
    /// `addr_of(s.f)`.
    Scalar,
}

/// One contiguous, byte-addressed region naming a live place. Cell `k` occupies
/// bytes `[base + k*stride, base + (k+1)*stride)`. The region owns **no value** —
/// only the coordinates needed to reach the real storage.
struct RawRegion {
    base: usize,
    stride: usize,
    len: usize,
    /// The interpreter frame whose `Env` holds the root binding.
    frame: u64,
    root: RootSlot,
    /// The root binding's source name. Diagnostics only — never used to resolve.
    name: String,
    /// The path from the root binding down to the addressed place (for
    /// [`RawShape::ArrayElements`], down to the *array*; the cell index is appended
    /// at access time).
    path: Vec<ResolvedPlace>,
    shape: RawShape,
}

impl RawRegion {
    /// The exclusive end byte address of the region.
    fn end(&self) -> usize {
        self.base + self.stride.saturating_mul(self.len)
    }

    fn contains(&self, addr: usize) -> bool {
        self.stride != 0 && addr >= self.base && addr < self.end()
    }

    /// The cell index a byte address maps to, if it lies within the region. Floors
    /// to the containing cell (a raw reinterpret of an unaligned interior address).
    fn cell_index(&self, addr: usize) -> Option<usize> {
        self.contains(addr)
            .then(|| (addr - self.base) / self.stride)
    }

    /// The full root-relative path of the cell a byte address maps to.
    fn path_to(&self, addr: usize) -> Option<Vec<ResolvedPlace>> {
        let cell = self.cell_index(addr)?;
        let mut path = self.path.clone();
        if self.shape == RawShape::ArrayElements {
            path.push(ResolvedPlace::Index(cell as i64));
        }
        Some(path)
    }
}

/// The outcome of mapping a raw byte address back to a place.
#[derive(Debug, Clone, PartialEq)]
pub enum RawResolve {
    /// A live region: read/write `path` under the root binding at `root`. The frame
    /// that created the region is still on the call stack (a returned frame's regions
    /// are dropped by [`RawPointerMemory::exit_frame`]), so the place is alive —
    /// whether it belongs to the current frame or an ancestor.
    ///
    /// The caller locates the owning `Env` by [`RootSlot::env`] — the current frame's
    /// `&mut Env` or an entry on the env shelf — and still has to confirm the *scope*
    /// is alive (`Env::at`), which fails once the block holding the root binding was
    /// popped.
    Place {
        root: RootSlot,
        path: Vec<ResolvedPlace>,
        name: String,
    },
    /// The address belonged to a region whose frame has since **returned**, so the
    /// place it named no longer exists at all. A pointer that outlived its frame.
    Dangling { name: String },
    /// No region covers this address: it is a bare `int_to_ptr` value that merely
    /// happens to land in raw space (e.g. an MMIO address), not an `addr_of` result.
    Unmapped,
}

/// A region whose frame has returned, kept only so a surviving pointer to it can be
/// diagnosed accurately ("`x`'s frame returned") instead of as an anonymous bad
/// address. Holds no root — the place is genuinely gone.
struct RawTombstone {
    base: usize,
    end: usize,
    name: String,
}

/// How many returned-frame regions to remember for diagnostics. A bounded quarantine:
/// good messages for the overwhelmingly common case (a pointer that just escaped the
/// call you are in) without letting a program that takes addresses in a hot loop grow
/// this table without limit. Beyond it, an escaped address degrades to
/// [`RawResolve::Unmapped`] — still a hard error, just a less specific one.
const TOMBSTONE_CAPACITY: usize = 64;

/// The interpreters' raw-pointer byte-address space. One instance per interpreter,
/// disjoint from the abstract heap. Empty until the first `addr_of`.
pub struct RawPointerMemory {
    regions: Vec<RawRegion>,
    /// Returned-frame regions, most recent last, capped at [`TOMBSTONE_CAPACITY`].
    tombstones: Vec<RawTombstone>,
    next_base: usize,
    /// The frame currently executing. `addr_of` stamps this onto the region it
    /// creates, so [`RawPointerMemory::exit_frame`] knows which regions die when that
    /// frame returns. It deliberately plays **no part in resolution**: a live region's
    /// place is reachable from a callee through the env shelf.
    current_frame: u64,
    /// Monotonic frame-id source. Ids are never reused, so a region from a returned
    /// frame can never be mistaken for one belonging to a later frame that happens
    /// to sit at the same call depth.
    next_frame: u64,
}

impl Default for RawPointerMemory {
    fn default() -> Self {
        Self {
            regions: Vec::new(),
            tombstones: Vec::new(),
            next_base: RAW_POINTER_BASE,
            current_frame: 0,
            next_frame: 1,
        }
    }
}

/// The C-natural element stride of an array: `size_of(T)` rounded up to
/// `align_of(T)`, matching [`Value::layout_size`]'s array formula. An empty array has
/// no element type, so it falls back to a pointer-word stride (its cells are never
/// readable — `len` is 0). Returns `None` only for an element with no defined layout
/// (a heap/growable value), which the type checker already forbids as an `addr_of`
/// element.
fn array_stride(cells: &[Value]) -> Option<usize> {
    match cells.first() {
        Some(element) => {
            let size = element.layout_size()?;
            let align = element.layout_align()?;
            Some(((size + align - 1) / align * align) as usize)
        }
        None => Some(8),
    }
}

impl RawPointerMemory {
    /// Whether a byte address belongs to this raw-pointer space (vs. an abstract
    /// heap-slot handle). Used by `ptr_read`/`ptr_write`/`volatile_*` to route a
    /// pointer to the place-backed region model instead of the heap.
    pub fn is_raw(addr: usize) -> bool {
        addr >= RAW_POINTER_BASE
    }

    /// Whether the interpreters need to shelve the caller's `Env` at a call boundary
    /// so a callee can reach it (see the module docs' *env shelf*).
    ///
    /// False until the program's first `addr_of`, which is the whole point: shelving
    /// costs a two-word swap and a `Vec` push per call, and a program that never takes
    /// an address must not pay it. Gating on it is **sound by construction, not an
    /// optimization gamble**: with no live region, [`RawPointerMemory::resolve`] can
    /// only return [`RawResolve::Dangling`] or [`RawResolve::Unmapped`], so no access
    /// can consult an `Env` and the shelf's contents are unobservable.
    ///
    /// Once a region exists it stays until its frame returns, so the flag simply
    /// tracks "this program is doing raw addressing right now". `#[inline]` because
    /// this is tested once per call on every interpreter's hot path.
    #[inline]
    pub fn shelf_needed(&self) -> bool {
        !self.regions.is_empty()
    }

    /// Begin a new interpreter frame, returning the id to hand back to
    /// [`RawPointerMemory::exit_frame`] on the way out. Every function invocation on
    /// every interpreter must be bracketed by these two, so that the regions a frame
    /// creates die exactly when it returns — that bracket is what turns a returned
    /// `addr_of(local)` into a refusal instead of a read of reused storage.
    pub fn enter_frame(&mut self) -> u64 {
        let previous = self.current_frame;
        self.current_frame = self.next_frame;
        self.next_frame += 1;
        previous
    }

    /// End the current frame and restore `previous` (the value [`enter_frame`]
    /// returned). Regions created by the frame being left are dropped: their root
    /// bindings are gone, so any surviving pointer to them is dangling and must be
    /// refused, not resolved.
    ///
    /// [`enter_frame`]: RawPointerMemory::enter_frame
    pub fn exit_frame(&mut self, previous: u64) {
        let dead = self.current_frame;
        for region in self.regions.iter().filter(|r| r.frame == dead) {
            self.tombstones.push(RawTombstone {
                base: region.base,
                end: region.end(),
                name: region.name.clone(),
            });
        }
        if self.tombstones.len() > TOMBSTONE_CAPACITY {
            let excess = self.tombstones.len() - TOMBSTONE_CAPACITY;
            self.tombstones.drain(..excess);
        }
        self.regions.retain(|region| region.frame != dead);
        self.current_frame = previous;
    }

    /// Reserve a fresh region, returning its byte base. Regions are laid out
    /// consecutively with a one-stride guard gap so two adjacent regions never share
    /// an address.
    ///
    /// The gap is **not a bounds check**, and must not be read as one: it only
    /// separates neighbours by a single element, so a sufficiently out-of-range
    /// `addr_of(a[k])` — `k` at least two elements past the end — can still land
    /// inside a *later* region and resolve against that region's place. That mirrors
    /// C, where an out-of-range `&a[k]` is undefined behaviour; the region model at
    /// least makes the outcome deterministic rather than arbitrary. Raw pointer
    /// arithmetic is unchecked by definition inside `unsafe`. (A read or write that
    /// lands past the end of the *root array* while still inside its own region is
    /// caught by `get_place`/`set_place` and reported as `L0413`.)
    fn push_region(&mut self, region: RawRegion) -> usize {
        // Taking the address of the *same place* twice yields the same address —
        // that is what a real `lea` does, and re-registering would otherwise hand
        // back a new address every time, so `addr_of(a[0])` inside a loop would both
        // drift and grow this table without bound. Regions describe a place, not a
        // value, so an existing one stays valid however the value changes.
        if let Some(existing) = self.regions.iter().find(|candidate| {
            candidate.frame == region.frame
                && candidate.root == region.root
                && candidate.shape == region.shape
                && candidate.stride == region.stride
                && candidate.len == region.len
                && candidate.path == region.path
        }) {
            return existing.base;
        }
        let base = region.base;
        let span = region
            .stride
            .saturating_mul(region.len)
            .max(region.stride)
            .max(8);
        self.next_base = base + span + region.stride.max(8);
        self.regions.push(region);
        base
    }

    /// `addr_of(place)` for a scalar or struct place: a one-cell region naming the
    /// value at `path` under the root binding at `root`. `value` is borrowed only to
    /// read its layout — it is never copied into the region.
    pub fn addr_of_place(
        &mut self,
        root: RootSlot,
        name: &str,
        path: Vec<ResolvedPlace>,
        value: &Value,
    ) -> Option<Value> {
        let stride = value.layout_size()? as usize;
        let base = self.push_region(RawRegion {
            base: self.next_base,
            stride,
            len: 1,
            frame: self.current_frame,
            root,
            name: name.to_string(),
            path,
            shape: RawShape::Scalar,
        });
        Some(Value::Ptr(base))
    }

    /// `addr_of(array[index])`, and whole-array decay `addr_of(array)` (`index` 0):
    /// a multi-cell region naming the array at `path`, returning a pointer to
    /// element `index` (`base + index*stride`) so `ptr_offset` can walk it. `cells`
    /// is borrowed only for its length and element layout.
    ///
    /// A negative or out-of-range `index` still yields a byte address — raw
    /// arithmetic is unchecked inside `unsafe` — and a later access through it fails
    /// when it resolves outside the region or past the end of the root array.
    pub fn addr_of_element(
        &mut self,
        root: RootSlot,
        name: &str,
        path: Vec<ResolvedPlace>,
        cells: &[Value],
        index: i64,
    ) -> Option<Value> {
        let stride = array_stride(cells)?;
        let base = self.push_region(RawRegion {
            base: self.next_base,
            stride,
            len: cells.len(),
            frame: self.current_frame,
            root,
            name: name.to_string(),
            path,
            shape: RawShape::ArrayElements,
        });
        Some(Value::Ptr((base as i64 + index * stride as i64) as usize))
    }

    /// `ptr_offset(p, n)`: advance the byte address by `n * size_of(T)`, where the
    /// element stride is taken from the region `p` points into. Returns `None` when
    /// `p` is not an `addr_of`-derived raw pointer (e.g. an `alloc`/`int_to_ptr`
    /// heap-slot handle), which the abstract model cannot walk.
    ///
    /// This deliberately ignores the frame: pointer *arithmetic* is pure address
    /// math and reveals nothing about the place, so it stays total. Only the
    /// dereference ([`RawPointerMemory::resolve`]) is lifetime-checked.
    pub fn offset(&self, addr: usize, n: i64) -> Option<usize> {
        let stride = self.region_at(addr).map(|region| region.stride)?;
        Some((addr as i64 + n * stride as i64) as usize)
    }

    fn region_at(&self, addr: usize) -> Option<&RawRegion> {
        self.regions.iter().find(|region| region.contains(addr))
    }

    /// Map a raw byte address back to the place it names. The caller reads or writes
    /// that place through its own `Env`, which is what makes an `addr_of` pointer
    /// alias in both directions.
    pub fn resolve(&self, addr: usize) -> RawResolve {
        let Some(region) = self.region_at(addr) else {
            // Not live. It may still be a *recently* returned frame's region, which
            // we can name precisely; otherwise it is simply not a mapped address.
            return match self
                .tombstones
                .iter()
                .rev()
                .find(|grave| addr >= grave.base && addr < grave.end)
            {
                Some(grave) => RawResolve::Dangling {
                    name: grave.name.clone(),
                },
                None => RawResolve::Unmapped,
            };
        };
        // No frame check: a region only exists while its frame is live (`exit_frame`
        // drops them), so any region we find names a place that is genuinely alive.
        // Reaching an *ancestor* frame's `Env` is the env shelf's job, not this
        // function's — addressing metadata says where the place is, not who may see it.
        match region.path_to(addr) {
            Some(path) => RawResolve::Place {
                root: region.root,
                path,
                name: region.name.clone(),
            },
            None => RawResolve::Unmapped,
        }
    }
}

// ---------------------------------------------------------------------------
// Freestanding static-buffer arenas (`documents/freestanding_tier_design.md` §5).
// ---------------------------------------------------------------------------

/// The env key holding a static-buffer arena's **bump cursor** (a cell index).
///
/// The space is deliberate and load-bearing: the lexer can never produce an
/// identifier containing one, so these keys are collision-proof against any user
/// binding **by construction** rather than by picking a name nobody is likely to
/// write. Storing arena state as ordinary env bindings — rather than on the
/// interpreter — gives it exactly the frame and block lifetime a `region`
/// declaration has, for free.
pub fn arena_cursor_key(region: &str) -> String {
    format!("arena cursor {region}")
}

/// The env key holding the *name* of a static-buffer arena's backing buffer.
/// See [`arena_cursor_key`] for why the space makes this collision-proof.
pub fn arena_buffer_key(region: &str) -> String {
    format!("arena buffer {region}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IntKind;

    fn root() -> RootSlot {
        RootSlot {
            env: 1,
            scope: 1,
            entry: 0,
        }
    }

    fn cells() -> Vec<Value> {
        vec![Value::I64(10), Value::I64(20), Value::I64(30)]
    }

    #[test]
    fn array_walk_resolves_to_consecutive_elements_and_holds_the_size_law() {
        let mut mem = RawPointerMemory::default();
        let Value::Ptr(base) = mem
            .addr_of_element(root(), "a", Vec::new(), &cells(), 0)
            .expect("addr_of")
        else {
            panic!("addr_of should yield a pointer");
        };
        // The address lives in the raw-pointer space, not the heap-slot space.
        assert!(RawPointerMemory::is_raw(base));
        // Walking names consecutive *elements of the root array* — no copies.
        for i in 0..3i64 {
            let addr = mem.offset(base, i).expect("offset");
            let RawResolve::Place { root: r, path, .. } = mem.resolve(addr) else {
                panic!("a live in-frame region must resolve to a place");
            };
            assert_eq!(r, root());
            assert_eq!(path, vec![ResolvedPlace::Index(i)]);
        }
        // The size law: (base+1) - base == size_of(i64) == 8.
        assert_eq!(mem.offset(base, 1).expect("offset") - base, 8);
    }

    #[test]
    fn stride_matches_element_size_for_narrow_scalars() {
        let mut mem = RawPointerMemory::default();
        let narrow = vec![Value::int(1, IntKind::I32), Value::int(2, IntKind::I32)];
        let Value::Ptr(base) = mem
            .addr_of_element(root(), "a", Vec::new(), &narrow, 0)
            .expect("addr_of")
        else {
            panic!("addr_of should yield a pointer");
        };
        // i32 stride is 4, so the size law reports 4 and offset 1 names element 1.
        assert_eq!(mem.offset(base, 1).expect("offset") - base, 4);
        let RawResolve::Place { path, .. } = mem.resolve(base + 4) else {
            panic!("expected a place");
        };
        assert_eq!(path, vec![ResolvedPlace::Index(1)]);
    }

    #[test]
    fn a_negative_offset_steps_back_to_an_earlier_element() {
        let mut mem = RawPointerMemory::default();
        let Value::Ptr(third) = mem
            .addr_of_element(root(), "a", Vec::new(), &cells(), 2)
            .expect("addr_of")
        else {
            panic!("addr_of should yield a pointer");
        };
        let back = mem.offset(third, -2).expect("offset");
        let RawResolve::Place { path, .. } = mem.resolve(back) else {
            panic!("expected a place");
        };
        assert_eq!(path, vec![ResolvedPlace::Index(0)]);
    }

    #[test]
    fn a_scalar_place_keeps_its_field_path_and_has_one_cell() {
        let mut mem = RawPointerMemory::default();
        let path = vec![ResolvedPlace::Field("hi".to_string())];
        let Value::Ptr(base) = mem
            .addr_of_place(root(), "pair", path.clone(), &Value::I64(9))
            .expect("addr_of")
        else {
            panic!("addr_of should yield a pointer");
        };
        let RawResolve::Place { path: got, .. } = mem.resolve(base) else {
            panic!("expected a place");
        };
        // A scalar region does not append a cell index: it *is* the place.
        assert_eq!(got, path);
        // One cell only — stepping off the end leaves the region entirely.
        assert_eq!(mem.resolve(base + 8), RawResolve::Unmapped);
    }

    #[test]
    fn a_pointer_passed_into_a_callee_still_names_its_place() {
        let mut mem = RawPointerMemory::default();
        let Value::Ptr(base) = mem
            .addr_of_element(root(), "a", Vec::new(), &cells(), 0)
            .expect("addr_of")
        else {
            panic!("addr_of should yield a pointer");
        };
        // Entering a callee: the caller's block has NOT ended (C11 6.2.4p6), so the
        // place is alive and the pointer must still name it. Reaching the owning
        // `Env` is the interpreter's env shelf's job; the region model must not
        // pretend the place is gone. (This is the out-parameter idiom.)
        let previous = mem.enter_frame();
        let RawResolve::Place { root: r, path, .. } = mem.resolve(base) else {
            panic!("a live place must resolve from a callee frame");
        };
        assert_eq!(r, root());
        assert_eq!(path, vec![ResolvedPlace::Index(0)]);
        // Arithmetic stays total across the boundary, and a callee can walk a buffer
        // its caller handed it.
        assert_eq!(mem.offset(base, 1), Some(base + 8));
        let RawResolve::Place { path, .. } = mem.resolve(base + 8) else {
            panic!("a callee must be able to walk a caller's buffer");
        };
        assert_eq!(path, vec![ResolvedPlace::Index(1)]);
        // Two frames deep is no different — depth is irrelevant to liveness.
        let inner = mem.enter_frame();
        assert!(matches!(mem.resolve(base), RawResolve::Place { .. }));
        mem.exit_frame(inner);
        mem.exit_frame(previous);
        assert!(matches!(mem.resolve(base), RawResolve::Place { .. }));
    }

    #[test]
    fn the_shelf_is_only_needed_once_an_address_has_been_taken() {
        let mut mem = RawPointerMemory::default();
        // A program that never calls `addr_of` must not pay for the shelf, and it is
        // sound not to: with no region, nothing can resolve to a place anyway.
        assert!(!mem.shelf_needed());
        assert_eq!(mem.resolve(RAW_POINTER_BASE), RawResolve::Unmapped);
        let previous = mem.enter_frame();
        mem.addr_of_element(root(), "a", Vec::new(), &cells(), 0)
            .expect("addr_of");
        assert!(mem.shelf_needed());
        // The region dies with its frame, and so does the need to shelve.
        mem.exit_frame(previous);
        assert!(!mem.shelf_needed());
    }

    #[test]
    fn regions_die_with_the_frame_that_created_them() {
        let mut mem = RawPointerMemory::default();
        let previous = mem.enter_frame();
        let Value::Ptr(base) = mem
            .addr_of_element(root(), "a", Vec::new(), &cells(), 0)
            .expect("addr_of")
        else {
            panic!("addr_of should yield a pointer");
        };
        assert!(matches!(mem.resolve(base), RawResolve::Place { .. }));
        // The callee returns: the region is gone, so a pointer it leaked out must
        // never resolve to a place again. A tombstone keeps the *name* so the
        // diagnostic can say precisely which place died, rather than reporting an
        // anonymous bad address.
        mem.exit_frame(previous);
        assert_eq!(
            mem.resolve(base),
            RawResolve::Dangling {
                name: "a".to_string()
            }
        );
    }

    #[test]
    fn frame_ids_are_never_reused_across_sibling_calls() {
        let mut mem = RawPointerMemory::default();
        // Two sibling calls sit at the same call *depth*; a depth-based check would
        // let the second resolve the first's region. Monotonic ids must not.
        let previous = mem.enter_frame();
        let Value::Ptr(base) = mem
            .addr_of_element(root(), "a", Vec::new(), &cells(), 0)
            .expect("addr_of")
        else {
            panic!("addr_of should yield a pointer");
        };
        mem.exit_frame(previous);
        let previous = mem.enter_frame();
        // The sibling must NOT see a place here — a depth-based check would.
        assert!(matches!(mem.resolve(base), RawResolve::Dangling { .. }));
        mem.exit_frame(previous);
    }

    #[test]
    fn an_unmapped_raw_address_is_not_an_addr_of_pointer() {
        let mem = RawPointerMemory::default();
        // An `int_to_ptr` MMIO-style address in raw space names no region.
        assert!(RawPointerMemory::is_raw(RAW_POINTER_BASE));
        assert_eq!(mem.resolve(RAW_POINTER_BASE), RawResolve::Unmapped);
        assert_eq!(mem.offset(RAW_POINTER_BASE, 1), None);
        // A heap-slot handle (small index) is not raw at all.
        assert_eq!(mem.offset(3, 1), None);
        assert!(!RawPointerMemory::is_raw(3));
    }
}
