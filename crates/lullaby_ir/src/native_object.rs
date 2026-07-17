use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use lullaby_parser::{AssignOp, BinaryOp, TypeRef};

use crate::native_contract::{
    NativeArchitecture, NativeObjectFormat, NativeTarget, native_backend_contract,
    x86_64_windows_target,
};
use crate::object_model::{
    ObjectModel, ObjectRelocation, ObjectRelocationKind, ObjectSection, ObjectSectionKind,
    ObjectSymbol, ObjectSymbolKind,
};
use crate::{
    BytecodeClosureDef, BytecodeExpr, BytecodeExprKind, BytecodeFunction, BytecodeIfBranch,
    BytecodeInstruction, BytecodeMatchArm, BytecodeMatchPattern, BytecodeModule, BytecodePlace,
    IntKind, IrEnumDef, IrStructDef,
};
use crate::{elf_object, macho_object};

#[path = "native_object_runtime_helpers.rs"]
mod runtime_helpers;
pub(crate) use runtime_helpers::*;

/// The fixed-width integer kind named by a Lullaby type name (`i8`/`u32`/…), or
/// `None` for `i64` and every non-fixed-width type. The native backend keeps a
/// fixed-width value in a 64-bit register as the same normalized `i64` cell the
/// interpreters use (see [`IntKind`] and [`IntKind::normalize`]): signed kinds
/// sign-extended, unsigned zero-extended, the 64-bit kinds filling the cell.
fn fixed_int_kind(type_name: &str) -> Option<IntKind> {
    match type_name {
        "i8" => Some(IntKind::I8),
        "i16" => Some(IntKind::I16),
        "i32" => Some(IntKind::I32),
        "u8" => Some(IntKind::U8),
        "u16" => Some(IntKind::U16),
        "u32" => Some(IntKind::U32),
        "u64" => Some(IntKind::U64),
        "isize" => Some(IntKind::Isize),
        "usize" => Some(IntKind::Usize),
        _ => None,
    }
}

/// The target [`IntKind`] of a `to_<T>` conversion builtin (`to_i8`/`to_u32`/…),
/// or `None` for `to_i64` (identity on the cell) and every non-conversion call.
/// These appear in the IR/bytecode as builtin calls; the native backend emits
/// them inline (a width-normalize of the argument's cell) rather than a real
/// call — see [`lower_native_expr`].
fn to_int_conversion_kind(name: &str) -> Option<IntKind> {
    match name {
        "to_i8" => Some(IntKind::I8),
        "to_i16" => Some(IntKind::I16),
        "to_i32" => Some(IntKind::I32),
        "to_u8" => Some(IntKind::U8),
        "to_u16" => Some(IntKind::U16),
        "to_u32" => Some(IntKind::U32),
        "to_u64" => Some(IntKind::U64),
        "to_isize" => Some(IntKind::Isize),
        "to_usize" => Some(IntKind::Usize),
        _ => None,
    }
}

/// The arithmetic operation of an overflow-aware builtin (`checked_*`/
/// `saturating_*`/`wrapping_*`). Maps to the corresponding wrapping [`BinaryOp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverflowOp {
    Add,
    Sub,
    Mul,
}

impl OverflowOp {
    /// The wrapping [`BinaryOp`] this operation shares with the default `+`/`-`/`*`
    /// (used to route the `wrapping_*` builtins through the existing fixed-width
    /// binary-op emitter).
    fn binary_op(self) -> BinaryOp {
        match self {
            OverflowOp::Add => BinaryOp::Add,
            OverflowOp::Sub => BinaryOp::Subtract,
            OverflowOp::Mul => BinaryOp::Multiply,
        }
    }
}

/// The overflow behaviour of an overflow-aware builtin: `wrapping_*` (wrap modulo
/// the width — the default arithmetic), `saturating_*` (clamp to `T`'s bounds),
/// or `checked_*` (`option<T>`, `none` on overflow). Mirrors the interpreters'
/// `OverflowMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverflowMode {
    Wrapping,
    Saturating,
    Checked,
}

/// Recognize an overflow-aware arithmetic builtin name (`checked_add`,
/// `saturating_mul`, `wrapping_sub`, …), returning its `(op, mode)`. Any other
/// name yields `None`.
fn overflow_builtin(name: &str) -> Option<(OverflowOp, OverflowMode)> {
    let (mode, op) = name.split_once('_')?;
    let mode = match mode {
        "checked" => OverflowMode::Checked,
        "saturating" => OverflowMode::Saturating,
        "wrapping" => OverflowMode::Wrapping,
        _ => return None,
    };
    let op = match op {
        "add" => OverflowOp::Add,
        "sub" => OverflowOp::Sub,
        "mul" => OverflowOp::Mul,
        _ => return None,
    };
    Some((op, mode))
}

/// Emit the re-normalization of the value in `rax` into `kind`'s canonical cell,
/// matching [`IntKind::normalize`] exactly: truncate to the kind's width, then
/// sign-extend (signed kinds) or zero-extend (unsigned kinds) back into the
/// 64-bit register. The 64-bit kinds (`u64`/`usize`/`isize`) already fill the
/// cell, so normalization is a no-op for them.
fn emit_normalize_rax(code: &mut Vec<u8>, kind: IntKind) {
    match kind {
        // movsx rax, al  (48 0F BE C0)
        IntKind::I8 => code.extend_from_slice(&[0x48, 0x0F, 0xBE, 0xC0]),
        // movsx rax, ax  (48 0F BF C0)
        IntKind::I16 => code.extend_from_slice(&[0x48, 0x0F, 0xBF, 0xC0]),
        // movsxd rax, eax  (48 63 C0)
        IntKind::I32 => code.extend_from_slice(&[0x48, 0x63, 0xC0]),
        // movzx rax, al  (48 0F B6 C0)
        IntKind::U8 => code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]),
        // movzx rax, ax  (48 0F B7 C0)
        IntKind::U16 => code.extend_from_slice(&[0x48, 0x0F, 0xB7, 0xC0]),
        // mov eax, eax  (89 C0) — a 32-bit write zero-extends into rax.
        IntKind::U32 => code.extend_from_slice(&[0x89, 0xC0]),
        // 64-bit kinds fill the whole cell: normalization is the identity.
        IntKind::U64 | IntKind::Isize | IntKind::Usize => {}
    }
}

/// Whether a Lullaby type name is a native-FFI integer scalar, and — when it is —
/// the [`IntKind`] a narrow C return of that type must be re-normalized to in
/// `rax`. `None` return means "no normalization needed" (the value already fills
/// the 64-bit cell: `i64`/`u64`/`isize`/`usize`). A non-integer-scalar type
/// (float, pointer, aggregate, string) yields the outer `None`, demoting an
/// extern caller that uses it to the interpreters.
///
/// Per §5.1: `bool` marshals as `_Bool` (0/1, `u8`-class), `char` as `uint32_t`
/// (`u32`-class), `byte` as `uint8_t` (`u8`-class). The signed/unsigned fixed
/// widths map to their `IntKind`; `i64` is the un-normalized 64-bit cell.
fn ffi_scalar_int_kind(type_name: &str) -> Option<Option<IntKind>> {
    match type_name {
        "i64" | "u64" | "isize" | "usize" => Some(None),
        "bool" | "byte" => Some(Some(IntKind::U8)),
        "char" => Some(Some(IntKind::U32)),
        _ => fixed_int_kind(type_name).map(Some),
    }
}

/// The imported process-exit function (from kernel32). Referenced by the entry
/// stub through a REL32 relocation; the linker binds it to the import thunk.
const EXIT_PROCESS_SYMBOL: &str = "ExitProcess";

/// A function lowered to x86-64: its symbol name, machine-code bytes, and the
/// relocations (at byte offsets within the code) that reference other symbols.
pub(crate) struct LoweredNativeFunction {
    name: String,
    code: Vec<u8>,
    relocations: Vec<CodeRelocation>,
    /// 1-based source line of the function's declaration (from `BytecodeFunction.span`).
    /// Used only when `--debug` line info is requested; otherwise ignored.
    line: u32,
}

impl LoweredNativeFunction {
    /// Build a lowered synthesized closure body (`__closure_{id}`). It has no source
    /// declaration line (line `0` — closures share their enclosing function's `.lby`
    /// line and are not separately break-pointable), so `--debug` maps it to line 0.
    pub(crate) fn new_closure(
        name: String,
        code: Vec<u8>,
        relocations: Vec<CodeRelocation>,
    ) -> Self {
        LoweredNativeFunction {
            name,
            code,
            relocations,
            line: 0,
        }
    }
}

/// A relocation inside a function body: patch a 4-byte REL32 field at `offset`
/// (relative to the function's own code start) to reference `symbol`.
pub(crate) struct CodeRelocation {
    /// Byte offset of the 4-byte field within this function's code.
    offset: u32,
    /// The symbol name referenced.
    symbol: String,
}

/// Interns the string-literal constants a native program references. Each unique
/// string is stored once in `.rdata`, NUL-terminated, and named `__str{index}`.
/// Native code references a string constant's address through a REL32 relocation
/// against that symbol (an `IMAGE_REL_AMD64_REL32` on a RIP-relative `lea`).
#[derive(Default)]
pub(crate) struct StringPool {
    /// The unique string contents, in first-seen order. Index `i` owns symbol
    /// `__str{i}`.
    entries: Vec<String>,
}

impl StringPool {
    /// Intern `text`, returning the `.rdata` symbol name that addresses its
    /// first byte.
    fn intern(&mut self, text: &str) -> String {
        let index = match self.entries.iter().position(|existing| existing == text) {
            Some(index) => index,
            None => {
                self.entries.push(text.to_string());
                self.entries.len() - 1
            }
        };
        format!("__str{index}")
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A local's stack placement: its `NativeType` layout and the `[rbp - slot]`
/// displacement of its **word 0** — for an aggregate, its LOWEST address (the
/// largest displacement of its slot range).
///
/// **Aggregate word order is ASCENDING (C-compatible):** word `k` lives at
/// `[rbp - (slot - 8*k)]` — 8·k bytes *higher* than word 0 — and at `[ptr + 8*k]`
/// through a word-0 pointer, so the layout agrees with C, `size_of`/`offset_of`,
/// and the interpreters. The frame itself still grows DOWNWARD per the Win64 ABI;
/// `plan` reserves `words*8` bytes and points `slot` at the top of that range.
/// Canonical: `documents/native_backend_contract.md`, "Aggregate word order".
#[derive(Debug, Clone)]
pub(crate) struct NativeLocal {
    slot: i32,
    ty: NativeType,
}

/// Per-function native lowering state (a stack-machine codegen over `rax`).
pub(crate) struct NativeCtx<'a> {
    /// name -> local placement (base slot displacement + layout).
    locals: HashMap<String, NativeLocal>,
    /// Total local stack bytes reserved (16-byte aligned, incl. shadow space).
    frame_size: i32,
    /// The set of function names that can be called (compiled functions).
    callable: &'a std::collections::HashSet<&'a str>,
    /// C-ABI signatures of the declared `extern fn` symbols, keyed by name. A
    /// call whose target is here marshals its arguments/return to the C scalar
    /// widths in the signature (integer-register subset) rather than the internal
    /// Lullaby i64 convention.
    extern_sigs: &'a HashMap<&'a str, &'a crate::IrExternSignature>,
    /// Relocations accumulated while emitting this function.
    relocations: Vec<CodeRelocation>,
    /// Program-wide interned string constants (`.rdata`), shared across all
    /// functions being lowered.
    strings: &'a mut StringPool,
    /// Struct definitions, for resolving aggregate and enum-payload layouts.
    structs: &'a [IrStructDef],
    /// Enum definitions, for resolving enum-value layouts (tags/payloads).
    enums: &'a [IrEnumDef],
    /// Temporary stack slots for materialized enum scrutinees (a `match` on a
    /// call result or constructed enum spills the value here). Assigned lazily
    /// past the planned locals; `scratch_base` marks the first free word.
    scratch_next: i32,
    /// When the function returns an aggregate by pointer, the frame slot holding
    /// the hidden result pointer (the caller-allocated destination passed in the
    /// first integer-argument register). `None` for a scalar (register) return,
    /// including `main`'s `i64`.
    sret_slot: Option<i32>,
    /// The `NativeType` of the function's return value, so an aggregate `return`
    /// knows how many words to write into the hidden result pointer.
    return_ty: NativeType,
    /// Native signatures (parameter + return layouts) of every compiled function,
    /// keyed by name. A `Call` to an aggregate-parameter / aggregate-returning
    /// function uses these to materialize by-pointer arguments and allocate the
    /// hidden return destination.
    signatures: &'a HashMap<String, NativeSignature>,
    /// Scalar `i64` locals kept in callee-saved registers (slot -> register) for a
    /// purely-scalar function. Empty for every other function. Loads/stores of a
    /// promoted slot go to the register instead of `[rbp - slot]`.
    promoted: HashMap<i32, PReg>,
    /// For each promoted register, the frame slot into which the prologue spills
    /// the caller's (callee-saved) value and from which the epilogue restores it.
    saved_reg_slots: Vec<(PReg, i32)>,
    /// Opt-in `--fast-math`: permits parity-BREAKING float codegen (currently f64
    /// sum/dot reductions vectorized with a 2-lane packed accumulator, which
    /// reorders the additions). Off by default, so the default build stays
    /// bit-exact with the interpreters.
    fast_math: bool,
    /// Arena-first memory (stage 1): this function is a provably-local heap-using
    /// region. When set, the prologue saves `__lullaby_heap_next` into
    /// `arena_mark_slot` and sets the arena-mode flag; every return edge restores
    /// the bump pointer (reclaiming the whole region the function allocated) and
    /// clears the flag. The body codegen is otherwise unchanged (value-neutral).
    is_arena: bool,
    /// The frame slot (`[rbp - arena_mark_slot]`) holding the saved bump pointer
    /// when `is_arena`; `0` otherwise (unused).
    arena_mark_slot: i32,
    /// Arena-first memory (stage 2): base frame slot of a region of per-loop-depth
    /// "sub-region mark" words. A loop nested `d` levels deep (0 = outermost) saves
    /// its entry bump pointer into `[rbp - (arena_loop_mark_base + 8*d)]`; sibling
    /// loops at the same depth reuse the word (never live simultaneously), nested
    /// loops at different depths use distinct words. `0` when `is_arena` is false or
    /// the function has no loops. See `arena_loop_mark_slot`.
    arena_loop_mark_base: i32,
    /// Freestanding **static-buffer arenas** (§5): region name -> its backing
    /// buffer local and the frame word holding its bump cursor. Populated in `plan`
    /// from the `arena_region` markers in the body; read when lowering
    /// `arena_alloc`. Empty for every function that declares no
    /// `region <name> in <buffer>`, which keeps all existing codegen byte-identical.
    arena_buffers: HashMap<String, ArenaBinding>,
    /// Aggregate type names (`struct`/`enum`) that transitively carry a heap
    /// field/payload (a `string` field, an `option<string>`/user enum with a heap
    /// payload, etc.). Used by the arena loop-lowering (`arena_loop_reset_mark`) so
    /// the per-iteration sub-region decision matches the module-level arena
    /// escape analysis: a loop that stores a heap-carrying aggregate into an
    /// iteration-outliving location does NOT get a sub-region (default-deny).
    heap_aggregates: std::collections::HashSet<String>,
    /// Closure-bound locals of this function: local name -> the parse-order closure
    /// `id` its `let` binds. A `Call` whose callee name is here is an indirect
    /// closure call (env pointer in `rcx`, code pointer at word 0 of the block); the
    /// local itself holds a pointer word to the closure's `[code_ptr][captures…]`
    /// heap block. Populated by `collect_native_locals` from a `let` whose value is
    /// a `Closure { id }` literal.
    closure_locals: HashMap<String, usize>,
    /// Native layouts of every natively-lowerable closure in the module, keyed by parse-order
    /// `id`. Used to size a closure literal's block, resolve the `__closure_{id}`
    /// code symbol, and lay out the captured scalars.
    closure_layouts: &'a HashMap<usize, ClosureLayout>,
    /// When this `NativeCtx` lowers a synthesized closure BODY, the env binding: the
    /// frame slot holding the env pointer (block base) and each captured name's byte
    /// offset within the env block. `None` for an ordinary top-level function body.
    closure_env: Option<ClosureEnv>,
}

/// The native signature of a compiled function: the layout of each parameter and
/// of its return value. Aggregate parameters/returns cross the boundary by
/// pointer (see the aggregate ABI); scalars pass in registers.
#[derive(Debug, Clone)]
pub(crate) struct NativeSignature {
    params: Vec<NativeType>,
    ret: NativeType,
}

impl NativeSignature {
    /// Whether the return value is an aggregate (returned by hidden pointer).
    fn returns_aggregate(&self) -> bool {
        self.ret.is_aggregate()
    }
}

impl<'a> NativeCtx<'a> {
    /// Plan the stack frame: assign contiguous 8-byte-word slots to every
    /// parameter and `let`/`for` local (aggregates reserve one word per
    /// flattened scalar), plus 32 bytes of Win64 shadow space when the function
    /// makes calls. All slots are `[rbp - displacement]`.
    #[allow(clippy::too_many_arguments)]
    fn plan(
        function: &'a BytecodeFunction,
        callable: &'a std::collections::HashSet<&'a str>,
        extern_sigs: &'a HashMap<&'a str, &'a crate::IrExternSignature>,
        structs: &'a [IrStructDef],
        enums: &'a [IrEnumDef],
        strings: &'a mut StringPool,
        signatures: &'a HashMap<String, NativeSignature>,
        array_lengths: &ArrayLengths,
        is_arena: bool,
        closure_layouts: &'a HashMap<usize, ClosureLayout>,
    ) -> Result<Self, String> {
        let mut locals: HashMap<String, NativeLocal> = HashMap::new();
        let mut closure_locals: HashMap<String, usize> = HashMap::new();
        let mut next_slot: i32 = 0;

        // Return classification: an aggregate return is written through a hidden
        // pointer passed in the first integer-argument register (Win64 `rcx`),
        // shifting the visible parameters to the following registers. A `void`
        // return resolves to `NativeType::Void` (no value): not an aggregate, so no
        // hidden pointer and no return scratch — the parameters keep register 0.
        let return_ty =
            resolve_return_native_type(&function.return_type, structs, enums, array_lengths)?;
        let return_is_aggregate = return_ty.is_aggregate();
        let sret_slot = if return_is_aggregate {
            next_slot += 8;
            Some(next_slot)
        } else {
            None
        };

        // Parameters (spilled from / copied out of registers in the prologue). A
        // scalar parameter is one word; an aggregate parameter reserves the full
        // aggregate layout (the register holds a pointer to the caller's copy,
        // whose words the prologue copies into these slots — value semantics).
        for param in &function.params {
            let native = resolve_signature_native_type(
                &param.ty,
                structs,
                enums,
                array_lengths,
                &param.name,
            )?;
            let words = native.words() as i32;
            next_slot += words * 8;
            // ASCENDING layout: `slot` names word 0 at the aggregate's LOWEST
            // address, i.e. the TOP of the reserved displacement range, so word k
            // (at `slot - 8*k`) climbs to higher addresses within the same bytes.
            locals.insert(
                param.name.clone(),
                NativeLocal {
                    slot: next_slot,
                    ty: native,
                },
            );
        }

        // Then `let` and `for` induction locals discovered anywhere in the body.
        collect_native_locals(
            &function.instructions,
            structs,
            enums,
            signatures,
            &mut locals,
            &mut next_slot,
            closure_layouts,
            &mut closure_locals,
        )?;

        // Default-deny closure escape check: every closure-bound local must be used
        // ONLY as its own `let`'s closure-literal initializer or as the callee of a
        // direct call `f(args)`. A closure passed to a function, returned, stored,
        // reassigned, or read as a bare value is a higher-order/escaping use outside
        // the supported slice, so the function skips cleanly rather than miscompiling.
        for name in closure_locals.keys() {
            if !closure_local_ok(function, name) {
                return Err(format!(
                    "closure local `{name}` escapes or is used in an unsupported position \
                     (native closures support only a direct non-escaping call)"
                ));
            }
        }

        // Reserve scratch words for `match` scrutinees that are not plain locals
        // (a call result or freshly-constructed enum is spilled to scratch before
        // the tag dispatch), and for aggregate call arguments / aggregate returns,
        // which are materialized into scratch and then copied by pointer. One
        // shared region sized to the widest such temporary across the function
        // suffices, since each is fully consumed before the next runs. The scratch
        // base is the first word past the planned locals.
        let match_scratch = max_match_scratch_words(&function.instructions, structs, enums)?;
        // The return value, when an aggregate, is materialized in scratch before
        // being copied through the hidden return pointer.
        let return_scratch = if return_is_aggregate {
            return_ty.words()
        } else {
            0
        };
        // Aggregate call arguments are each materialized in scratch before their
        // address is passed; a single call may pass several, so size the region to
        // the widest single call's total aggregate-argument words.
        let arg_scratch = max_call_arg_scratch_words(
            &function.instructions,
            structs,
            enums,
            signatures,
            array_lengths,
        )?;
        // The scratch cursor starts one word past `scratch_base` (word 0 of the
        // region is a reserved guard), so reserve one extra word of headroom.
        let scratch_words = match_scratch.max(return_scratch).max(arg_scratch);
        let scratch_base = next_slot;
        next_slot += (scratch_words as i32 + 1) * 8;

        // Register promotion (purely-scalar functions only): keep a couple of hot
        // `i64` locals in callee-saved registers. Reserve one frame word per
        // promoted register to spill the caller's value across this function; the
        // promoted locals keep their (now unused) stack slots for simplicity.
        // A closure-using function is excluded from register promotion: a captured
        // scalar must live in its frame slot so the closure-literal lowering can
        // read it, and the closure-call sequence uses the volatile registers
        // directly. (The closure `let` already has a `fn(...)` type, which makes the
        // function non-promotable, so this is belt-and-suspenders — but explicit.)
        let (promoted, saved_regs) = if closure_locals.is_empty() {
            plan_register_promotion(function, &locals)
        } else {
            (HashMap::new(), Vec::new())
        };
        let mut saved_reg_slots = Vec::new();
        for reg in saved_regs {
            next_slot += 8;
            saved_reg_slots.push((reg, next_slot));
        }

        // Arena-first memory (stage 1): reserve one frame word to save the bump
        // pointer (`__lullaby_heap_next`) on entry, restored on every return edge.
        let arena_mark_slot = if is_arena {
            next_slot += 8;
            next_slot
        } else {
            0
        };

        // Freestanding static-buffer arenas (§5): reserve one frame word per
        // declared `region <name> in <buffer>` to hold that arena's bump cursor (a
        // cell count into the backing buffer). The cursor has no source-level
        // binding, so it cannot be an ordinary local, and two arenas over two
        // buffers can be live at once, so the word cannot be shared. The prologue
        // zeroes each one. Functions with no arena reserve nothing and stay
        // byte-identical.
        //
        // Note this is orthogonal to `is_arena` above: that is the arena-FIRST
        // escape analysis over the *host* heap bump pointer, which a static-buffer
        // arena never touches. See `native_object_arena.rs` for why the two cannot
        // interact.
        let mut arena_buffers: HashMap<String, ArenaBinding> = HashMap::new();
        for region in collect_arena_regions(&function.instructions) {
            next_slot += 8;
            arena_buffers.insert(
                region.name,
                ArenaBinding {
                    buffer: region.buffer,
                    cursor_slot: next_slot,
                },
            );
        }

        // Arena-first memory (stage 2): reserve one bump-pointer "sub-region mark"
        // word per level of loop nesting, so each loop that gets a per-iteration
        // sub-region can save/restore its own mark. A loop at nesting depth `d`
        // (0-based) uses word `d`; the region is sized to the deepest loop nest. Only
        // arena functions rewind loops, so non-arena functions reserve nothing and
        // stay byte-identical.
        let arena_loop_mark_base = if is_arena {
            let depth = max_loop_nesting(&function.instructions);
            if depth > 0 {
                let base = next_slot + 8;
                next_slot += depth as i32 * 8;
                base
            } else {
                0
            }
        } else {
            0
        };

        let has_call = body_has_call(&function.instructions);
        // Reserve local slots plus (if calling) 32 bytes of shadow space, plus an
        // outgoing stack-argument area for any call passing more than four
        // effective register arguments. The area lives at the bottom of the frame
        // (lowest addresses, where `rsp` points at a `call`): `[rsp .. rsp+32]` is
        // the shadow, `[rsp+32 .. rsp+32+8*out]` holds the 5th+ arguments.
        // `closure_locals` is populated by `collect_native_locals` above, so a call
        // through a closure local is counted with its hidden env pointer.
        let out_words =
            max_outgoing_stack_words(&function.instructions, signatures, &closure_locals);
        let shadow = if has_call { 32 } else { 0 };
        let raw = next_slot + shadow + out_words as i32 * 8;
        // Keep the frame a multiple of 16 so that after `push rbp` and a `call`
        // the callee sees a 16-byte-aligned rsp per the Win64 ABI.
        let frame_size = ((raw + 15) / 16) * 16;

        Ok(Self {
            locals,
            frame_size,
            callable,
            extern_sigs,
            relocations: Vec::new(),
            strings,
            structs,
            enums,
            // First scratch word sits one word past the scratch base.
            scratch_next: scratch_base + 8,
            sret_slot,
            return_ty,
            signatures,
            promoted,
            saved_reg_slots,
            fast_math: false,
            is_arena,
            arena_mark_slot,
            arena_loop_mark_base,
            arena_buffers,
            // Non-generic heap-carrying aggregate NAMES plus the heap-`T`
            // user-generic INSTANTIATION spellings used in this function
            // (`Box<string>`, `Opt<string>`), so the lowering-time loop confinement
            // check treats a monomorphized heap-`T` value as heap — consistent with
            // the module-wide `arena_eligible_functions` gate. Scalar instantiations
            // are never added, so scalar-generic codegen is byte-identical.
            heap_aggregates: {
                let mut aggs = heap_carrying_aggregates(structs, enums);
                aggs.extend(heap_carrying_generic_instantiations(
                    function, structs, enums, &aggs,
                ));
                aggs
            },
            closure_locals,
            closure_layouts,
            closure_env: None,
        })
    }

    /// The frame slot of the arena sub-region mark for a loop at nesting depth
    /// `depth` (0 = outermost). Words are laid out contiguously from
    /// `arena_loop_mark_base`; `plan` reserves one per level of loop nesting.
    fn arena_loop_mark_slot(&self, depth: usize) -> i32 {
        self.arena_loop_mark_base + depth as i32 * 8
    }

    /// The register a local slot is promoted into, if any.
    fn promoted_reg(&self, slot: i32) -> Option<PReg> {
        self.promoted.get(&slot).copied()
    }

    /// Allocate `words` contiguous scratch words, returning the slot of **word 0**
    /// of the region. Used to spill a temporary enum scrutinee / aggregate.
    /// The cursor advances; callers restore it afterwards via the saved cursor.
    ///
    /// ASCENDING layout (see [`NativeLocal`]): word 0 sits at the region's lowest
    /// address, i.e. the LARGEST displacement in the reserved range, so word `k`
    /// is at `base - 8*k` and stays inside `[scratch_next ..= scratch_next +
    /// 8*(words-1)]`. For `words == 1` this is exactly the old cursor value, so a
    /// scalar scratch slot is unchanged.
    fn alloc_scratch(&mut self, words: usize) -> i32 {
        debug_assert!(words > 0, "a scratch region must be at least one word");
        let base = self.scratch_next + (words as i32 - 1) * 8;
        self.scratch_next += words as i32 * 8;
        base
    }

    /// The base-word slot displacement of a local (its first `[rbp - slot]`).
    fn local_slot(&self, name: &str) -> Result<i32, String> {
        self.locals
            .get(name)
            .map(|local| local.slot)
            .ok_or_else(|| format!("unknown native local `{name}`"))
    }

    /// The `(slot, layout)` of a local.
    fn local(&self, name: &str) -> Result<&NativeLocal, String> {
        self.locals
            .get(name)
            .ok_or_else(|| format!("unknown native local `{name}`"))
    }
}

/// A resolved scalar destination inside an aggregate. Aggregate storage ASCENDS in
/// memory (see [`NativeLocal`]), so a scalar lives at `[rbp - disp]` where
/// `disp = base_slot - (const_bytes + dynamic_bytes)`; `dynamic_bytes` is
/// `elem_bytes * index` computed at runtime when the path crossed a runtime array
/// index, else zero.
///
/// Offsets are in **bytes**, not words, because a packed narrow array element
/// (`NativeType::Narrow`) sits at a sub-word stride: element 1 of an `array<i32>`
/// is 4 bytes from element 0, not 8. Struct fields and 8-byte elements still
/// contribute whole multiples of 8, so every pre-existing path resolves to exactly
/// the displacement it did when these were word counts.
pub(crate) enum ScalarPlace {
    /// A fully static scalar at `[rbp - slot]`.
    Const { slot: i32 },
    /// A dynamic scalar. `base_slot` is the enclosing local's byte 0;
    /// `const_bytes` accumulates the static byte offset from field hops and
    /// constant indices; `elem_bytes` is the per-element BYTE stride of the
    /// dynamic array (its element's C width); `index_len` is the element count of
    /// the array the runtime index selects into (its static length), used to emit a
    /// bounds check; and the runtime index expression selects the element.
    Dynamic {
        base_slot: i32,
        const_bytes: i64,
        elem_bytes: i64,
        index_len: i64,
        index: BytecodeExpr,
    },
    /// A scalar inside a **fat-pointer** array parameter: the element address
    /// is `data_ptr + elem_bytes * index` (elements ASCEND from element 0,
    /// exactly like a stack array), where `data_ptr` lives in the frame at
    /// `[rbp - ptr_slot]` (descriptor word 0) and the runtime element count lives
    /// at `[rbp - len_slot]` (descriptor word 1, at `ptr_slot - 8`). The index is bounds-checked
    /// against that runtime length before the access (matching the interpreters'
    /// `L0413`). `elem_ty` is the scalar element layout of the loaded word.
    FatIndex {
        ptr_slot: i32,
        len_slot: i32,
        elem_bytes: i64,
        index: BytecodeExpr,
    },
}

/// Loop targets: the byte offsets a `break`/`continue` jumps to. Because loop
/// bodies are emitted before we know the loop-end (or, for `for`, the step)
/// offset, jumps whose target is not yet known are recorded as patch sites and
/// fixed up when the loop is fully emitted.
pub(crate) struct NativeLoop {
    /// Code offset of the `continue` target when already known (`while`/`loop`
    /// jump back to the top). `None` for a `for` loop, whose `continue` must
    /// jump forward to the step block: those jumps are recorded in
    /// `continue_sites` and patched once the step block's offset is known.
    continue_target: Option<usize>,
    /// Patch sites (offsets of 4-byte rel32 fields) for forward `continue` jumps.
    continue_sites: Vec<usize>,
    /// Patch sites (offsets of 4-byte rel32 fields) for `break` jumps.
    break_sites: Vec<usize>,
    /// RC scope-based drop insertion (stage 2), early-exit edges. The uniquely-owned,
    /// borrow-only heap locals declared directly in THIS loop's body that are LIVE at
    /// the current lowering position — i.e. their `let` has already been lowered. A
    /// `break`/`continue` drops exactly this set before jumping, so a value declared
    /// in the loop body is reclaimed on the early-exit edge too (not only the
    /// fallthrough back-edge). It is revealed incrementally as each top-level body
    /// statement is lowered (see `lower_loop_body_with_drops`), so an early exit only
    /// ever drops locals whose declaration textually precedes it — never a slot whose
    /// `let` has not run. Each entry is `(frame slot, drop-helper symbol)`.
    live_drops: Vec<(i32, &'static str)>,
    /// Arena-first memory (stage 2): the frame slot holding this loop's saved bump
    /// pointer when the loop gets a per-iteration **sub-region**. `Some(mark)` means
    /// every heap value the loop body allocates is confined to the iteration (it
    /// provably does not escape), so the bump pointer is rewound to `mark` at each
    /// iteration boundary — the fallthrough back-edge and the `break`/`continue`
    /// early-exit edges — reclaiming the iteration's scratch in bounded heap. The
    /// rewind is idempotent (restoring the same saved value), so an iteration taking
    /// more than one edge cannot double-free, and confinement guarantees it never
    /// rewinds past a value that survives the iteration. `None` for a loop with no
    /// sub-region (a non-arena function, a scalar loop, or a loop whose heap escapes).
    arena_reset_mark: Option<i32>,
}

#[path = "native_object_program.rs"]
mod program;
pub use program::*;

#[path = "native_object_frame.rs"]
mod frame;
pub(crate) use frame::*;

#[path = "native_object_coff.rs"]
mod coff;
pub use coff::*;

#[path = "native_object_layout.rs"]
mod layout;
pub(crate) use layout::*;

#[path = "native_object_types.rs"]
mod types;
pub(crate) use types::*;

#[path = "native_object_eligibility.rs"]
mod eligibility;
pub(crate) use eligibility::*;

#[path = "native_object_method.rs"]
mod method;
pub(crate) use method::*;

#[path = "native_object_stmt.rs"]
mod stmt_lowering;
pub(crate) use stmt_lowering::*;

#[path = "native_object_expr.rs"]
mod expr_lowering;
pub(crate) use expr_lowering::*;

#[path = "native_object_callargs.rs"]
mod call_args;
pub(crate) use call_args::*;

#[path = "native_object_rawptr.rs"]
mod rawptr;
pub(crate) use rawptr::*;

#[path = "native_object_heapbox.rs"]
mod heapbox;
pub(crate) use heapbox::*;

#[path = "native_object_portio.rs"]
mod portio;
pub(crate) use portio::*;

#[path = "native_object_arena.rs"]
mod arena;
pub(crate) use arena::*;

#[path = "native_object_closure.rs"]
mod closure;
pub(crate) use closure::*;

#[path = "native_object_closure_ctx.rs"]
mod closure_ctx;
pub(crate) use closure_ctx::*;

#[path = "native_object_lowering.rs"]
mod op_lowering;
pub(crate) use op_lowering::*;

// -- Small instruction helpers -----------------------------------------------

/// `mov rax, imm64` (always the 10-byte form for simplicity/correctness).
fn emit_mov_rax_imm(code: &mut Vec<u8>, value: i64) {
    code.extend_from_slice(&[0x48, 0xB8]);
    code.extend_from_slice(&value.to_le_bytes());
}

/// `mov rax, [rbp - slot]`.
fn load_local(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0x48, 0x8B, 0x85]);
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// `mov [rbp - slot], rax`.
fn store_local(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0x48, 0x89, 0x85]);
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// `mov rax, [rbp + disp]` — load an incoming **stack argument** into rax. Unlike
/// `load_local` (which addresses a callee-owned slot at a negative displacement),
/// this reads a positive `[rbp + disp]` address, where the 5th+ Win64 arguments
/// live: `[rbp+8]` is the return address, `[rbp]` the saved rbp, so the first
/// stack argument sits at `[rbp+16]`.
fn emit_mov_rax_from_rbp_pos(code: &mut Vec<u8>, disp: i32) {
    code.extend_from_slice(&[0x48, 0x8B, 0x85]); // mov rax, [rbp + disp32]
    code.extend_from_slice(&disp.to_le_bytes());
}

/// `mov rcx, [rax + disp]` — read a word at an offset from a pointer in rax.
/// Used to copy aggregate words out of a by-pointer argument or into a by-pointer
/// result. `disp` is a small non-negative byte offset (disp32 form).
fn emit_mov_rcx_from_rax_disp(code: &mut Vec<u8>, disp: i32) {
    code.extend_from_slice(&[0x48, 0x8B, 0x88]); // mov rcx, [rax + disp32]
    code.extend_from_slice(&disp.to_le_bytes());
}

/// `mov [rbp - slot], rcx` — store rcx into a frame slot.
fn emit_mov_slot_from_rcx(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0x48, 0x89, 0x8D]); // mov [rbp + disp32], rcx
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// `mov [rax + disp], rcx` — store rcx to an offset from a pointer in rax.
/// Used to write aggregate result words through the hidden return pointer.
fn emit_mov_rax_disp_from_rcx(code: &mut Vec<u8>, disp: i32) {
    code.extend_from_slice(&[0x48, 0x89, 0x88]); // mov [rax + disp32], rcx
    code.extend_from_slice(&disp.to_le_bytes());
}

/// `mov rcx, [rbp - slot]` — load a frame slot into rcx.
fn emit_mov_rcx_from_slot(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0x48, 0x8B, 0x8D]); // mov rcx, [rbp + disp32]
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// Emit `jmp rel32` to an already-known target offset.
fn emit_jmp_to(code: &mut Vec<u8>, target: usize) {
    code.push(0xE9);
    let site = code.len();
    let rel = (target as i64) - (site as i64 + 4);
    code.extend_from_slice(&(rel as i32).to_le_bytes());
}

/// Patch the 4-byte rel32 field at `site` so it points to the current end of
/// `code` (i.e. the instruction right after everything emitted so far).
fn patch_rel32(code: &mut [u8], site: usize) {
    let target = code.len();
    patch_rel32_to(code, site, target);
}

/// Patch the 4-byte rel32 field at `site` to point to `target`.
fn patch_rel32_to(code: &mut [u8], site: usize, target: usize) {
    let rel = (target as i64) - (site as i64 + 4);
    let bytes = (rel as i32).to_le_bytes();
    code[site..site + 4].copy_from_slice(&bytes);
}
#[path = "native_object_writers.rs"]
mod object_writers;
pub(crate) use object_writers::*;

// DWARF source-line debug info for the ELF/Mach-O targets — the portable
// counterpart of the COFF `.debug$S` CodeView emitter in `object_writers`. Its
// own file: it is a self-contained, format-neutral byte builder, and
// `native_object_writers.rs` is at the file-size cap.
#[path = "native_object_dwarf.rs"]
mod dwarf;
pub(crate) use dwarf::*;

#[path = "pe_image.rs"]
mod pe_image;
pub(crate) use pe_image::*;
