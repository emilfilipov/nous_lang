//! WASM linear-memory helpers and string/collection-construction codegen
//! (part 2 of the WASM lowering). Split out of wasm_lowering.rs; sees the
//! module-assembly types via `use super::*`.

use super::*;

// -- Linear-memory helpers ---------------------------------------------------

/// `__alloc(size)` a run of `size` bytes and stash the returned pointer in a
/// fresh scratch `i32` local; return that local's index. The pointer is reused
/// for each field/element store and finally re-pushed as the aggregate value.
pub(crate) fn alloc_bytes(ctx: &mut LowerCtx, size: i32, out: &mut Vec<u8>) -> u32 {
    let alloc_index = *ctx
        .func_index
        .get(ALLOC_HELPER_NAME)
        .expect("__alloc index recorded");
    out.push(0x41); // i32.const size
    write_sleb(out, size as i64);
    out.push(0x10); // call __alloc
    write_uleb(out, alloc_index as u64);
    let ptr = ctx.add_local(WasmValType::I32);
    set_local(out, ptr);
    ptr
}

/// Deep-copy the aggregate whose pointer is on top of the stack, leaving a fresh,
/// independent record's pointer on the stack. This is the WASM realization of the
/// interpreters' recursive `Value::clone` on a struct/array/enum: a snapshot whose
/// mutation cannot be observed through the original pointer.
///
/// - A `struct` copies each field slot; a scalar/string slot is copied word-for-
///   word, and a nested MUTABLE aggregate slot (struct/array/enum) is itself
///   deep-copied so nested mutation stays isolated too.
/// - A fixed `array` reads its `[len]` header, allocates a fresh `[len][slots]`
///   block, and copies each element in a runtime loop (deep-copying nested
///   aggregate elements).
/// - An `enum` copies its `[tag][payload slots]` record word-for-word: enum
///   payloads are always scalar (see [`enum_layout`]), so no nested aggregate can
///   hide inside, and a flat copy of the whole record is an exact deep copy.
///
/// `ty` must be a mutable aggregate ([`is_mutable_aggregate`]); the caller checks
/// this before invoking. An `i32`-pointer source, an `i32`-pointer result.
pub(crate) fn emit_deep_copy(
    ctx: &mut LowerCtx,
    ty: &TypeRef,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    if ctx.structs.contains_key(&ty.name) {
        return emit_deep_copy_struct(ctx, ty, out);
    }
    if let Some(layout) = ctx.enum_layout(ty) {
        return emit_deep_copy_enum(ctx, &layout, out);
    }
    if ty.array_element().is_some() {
        return emit_deep_copy_array(ctx, ty, out);
    }
    if let Some(elem) = supported_list_element(ty, ctx.structs, ctx.enums) {
        return emit_list_deep_copy(ctx, &elem, out);
    }
    if let Some((_, value)) = supported_map_kv(ty, ctx.structs, ctx.enums) {
        return emit_map_deep_copy(ctx, &value, out);
    }
    Err(format!(
        "cannot deep-copy non-aggregate type `{}` (wasm backend)",
        ty.name
    ))
}

/// Deep-copy a struct: the source pointer is on the stack. Allocate a fresh run of
/// one 8-byte slot per field, copy each field (deep-copying nested mutable
/// aggregate fields), and leave the fresh pointer on the stack.
pub(crate) fn emit_deep_copy_struct(
    ctx: &mut LowerCtx,
    ty: &TypeRef,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let fields = ctx
        .structs
        .get(&ty.name)
        .ok_or_else(|| format!("`{}` is not a struct", ty.name))?
        .clone();
    // Stash the source pointer; allocate the destination.
    let src = ctx.add_local(WasmValType::I32);
    set_local(out, src);
    let dst = alloc_bytes(ctx, fields.len() as i32 * SLOT_SIZE, out);
    for (slot, (_, field_ty)) in fields.iter().enumerate() {
        let offset = slot as i32 * SLOT_SIZE;
        emit_copy_slot(ctx, field_ty, src, dst, offset, out)?;
    }
    get_local(out, dst);
    Ok(())
}

/// Deep-copy an enum: the source pointer is on the stack. The `[tag][payload slots]`
/// record is `size_bytes()` bytes (one padded tag slot plus `slot_count` payload
/// slots, every one 8-byte aligned).
///
/// A scalar or `string` payload is copied word-for-word (a `string` is immutable, so
/// sharing its pointer IS its value-semantic copy). When EVERY variant payload is a
/// scalar or `string`, the whole record is a flat `i64` word copy — an exact deep
/// copy with no tag dispatch.
///
/// When some variant has a MUTABLE-aggregate payload (`struct` / nested `list`, e.g.
/// `option<struct>`), the flat copy is done first (so the tag and every scalar/
/// string slot land), then the record's tag is loaded and, for the matching
/// variant, each mutable-aggregate payload slot is re-copied as an independent
/// [`emit_deep_copy`] — so mutating the copy's payload is never observable through
/// the original. This mirrors the interpreters' recursive `Value::clone` on an enum.
pub(crate) fn emit_deep_copy_enum(
    ctx: &mut LowerCtx,
    layout: &EnumLayout,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let src = ctx.add_local(WasmValType::I32);
    set_local(out, src);
    let dst = alloc_bytes(ctx, layout.size_bytes(), out);
    // Flat word copy of the whole record (tag + every payload slot). For a scalar/
    // string-only enum this is already the exact deep copy.
    let words = layout.size_bytes() / SLOT_SIZE;
    for word in 0..words {
        let offset = word * SLOT_SIZE;
        get_local(out, dst);
        get_local(out, src);
        emit_load_at(WasmValType::I64, offset, out);
        emit_store_at(WasmValType::I64, offset, out);
    }
    // If any variant carries a mutable-aggregate payload, re-deep-copy those slots
    // for the record's actual variant (branch on the loaded tag).
    let has_mutable_payload = layout.variants.iter().any(|(_, payload)| {
        payload
            .iter()
            .any(|p| is_mutable_aggregate(p, ctx.structs, ctx.enums))
    });
    if has_mutable_payload {
        let tag = ctx.add_local(WasmValType::I32);
        get_local(out, dst);
        emit_load_at(WasmValType::I32, 0, out);
        set_local(out, tag);
        let variants = layout.variants.clone();
        for (variant_tag, (_, payload)) in variants.iter().enumerate() {
            let mutable_slots: Vec<(usize, TypeRef)> = payload
                .iter()
                .enumerate()
                .filter(|(_, p)| is_mutable_aggregate(p, ctx.structs, ctx.enums))
                .map(|(i, p)| (i, p.clone()))
                .collect();
            if mutable_slots.is_empty() {
                continue;
            }
            // if tag == variant_tag { deep-copy each mutable payload slot }
            get_local(out, tag);
            out.push(0x41); // i32.const variant_tag
            write_sleb(out, variant_tag as i64);
            out.push(0x46); // i32.eq
            out.push(0x04); // if -> void
            out.push(0x40);
            for (slot, payload_ty) in &mutable_slots {
                let offset = ENUM_PAYLOAD_BASE + *slot as i32 * SLOT_SIZE;
                // dst_addr = dst + offset; load the payload pointer, deep-copy it,
                // store the fresh independent pointer back into dst's slot.
                get_local(out, dst);
                out.push(0x41); // i32.const offset
                write_sleb(out, offset as i64);
                out.push(0x6a); // i32.add -> dst slot addr
                get_local(out, dst);
                emit_load_at(WasmValType::I32, offset, out);
                emit_deep_copy(ctx, payload_ty, out)?;
                emit_store(WasmValType::I32, out);
            }
            out.push(0x0b); // end if
        }
    }
    get_local(out, dst);
    Ok(())
}

/// Deep-copy a fixed array: the source pointer is on the stack. Read the `[len]`
/// header, allocate a fresh `[len][slots]` block, store the header, and copy each
/// element in a runtime loop (deep-copying nested mutable aggregate elements).
/// Leaves the fresh pointer on the stack.
pub(crate) fn emit_deep_copy_array(
    ctx: &mut LowerCtx,
    ty: &TypeRef,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = ty
        .array_element()
        .ok_or_else(|| format!("`{}` is not an array", ty.name))?;
    // Validate the element is a supported slot type up front (the caller already
    // classified this array as a mutable aggregate, but re-check for the copy loop).
    slot_val_type(&elem_ty, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("array element type `{}` is unsupported", elem_ty.name))?;

    let src = ctx.add_local(WasmValType::I32);
    set_local(out, src);
    // len = i32.load [src + 0]
    let len = ctx.add_local(WasmValType::I32);
    get_local(out, src);
    emit_load_at(WasmValType::I32, 0, out);
    set_local(out, len);
    // Allocate LEN_HEADER + len * SLOT_SIZE bytes: push the runtime size, call
    // __alloc directly (alloc_bytes only takes a constant size), stash the dst.
    let alloc_index = *ctx
        .func_index
        .get(ALLOC_HELPER_NAME)
        .expect("__alloc index recorded");
    get_local(out, len);
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul  -> len * SLOT_SIZE
    out.push(0x41); // i32.const LEN_HEADER
    write_sleb(out, LEN_HEADER as i64);
    out.push(0x6a); // i32.add  -> LEN_HEADER + len * SLOT_SIZE
    out.push(0x10); // call __alloc
    write_uleb(out, alloc_index as u64);
    let dst = ctx.add_local(WasmValType::I32);
    set_local(out, dst);
    // Store the length header at dst + 0.
    get_local(out, dst);
    get_local(out, len);
    emit_store_at(WasmValType::I32, 0, out);

    // Runtime loop: for i in 0..len { copy element i }.
    let i = ctx.add_local(WasmValType::I32);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, i);
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // break when i >= len
    get_local(out, i);
    get_local(out, len);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of the block)
    write_uleb(out, 1);
    // Element slot offset from the base: LEN_HEADER + i * SLOT_SIZE, in a local.
    let off = ctx.add_local(WasmValType::I32);
    get_local(out, i);
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul
    out.push(0x41); // i32.const LEN_HEADER
    write_sleb(out, LEN_HEADER as i64);
    out.push(0x6a); // i32.add
    set_local(out, off);
    // Copy element: dst_addr = dst + off, src_addr = src + off.
    emit_copy_element_at(ctx, &elem_ty, src, dst, off, out)?;
    // i += 1
    get_local(out, i);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, i);
    out.push(0x0c); // br 0 (repeat)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block

    get_local(out, dst);
    Ok(())
}

// -- Growable list codegen ---------------------------------------------------

/// `__alloc(size)` where `size` (an `i32`) is already on the stack, stashing the
/// returned pointer in a fresh scratch `i32` local; return that local's index.
/// The constant-size companion is [`alloc_bytes`]; this variant handles a runtime
/// byte count (a list's `LIST_DATA_OFF + cap * SLOT_SIZE`).
pub(crate) fn alloc_runtime(ctx: &mut LowerCtx, out: &mut Vec<u8>) -> u32 {
    let alloc_index = *ctx
        .func_index
        .get(ALLOC_HELPER_NAME)
        .expect("__alloc index recorded");
    out.push(0x10); // call __alloc (size already on the stack)
    write_uleb(out, alloc_index as u64);
    let ptr = ctx.add_local(WasmValType::I32);
    set_local(out, ptr);
    ptr
}

/// Push `LIST_DATA_OFF + cap * SLOT_SIZE` (the byte size of a list backing block
/// with `cap` element slots) onto the stack, given an `i32` local holding `cap`.
pub(crate) fn emit_list_block_size(cap_local: u32, out: &mut Vec<u8>) {
    get_local(out, cap_local);
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul  -> cap * SLOT_SIZE
    out.push(0x41); // i32.const LIST_DATA_OFF
    write_sleb(out, LIST_DATA_OFF as i64);
    out.push(0x6a); // i32.add  -> LIST_DATA_OFF + cap * SLOT_SIZE
}

/// Copy the first `count` element slots (each `SLOT_SIZE` bytes) from `src` to
/// `dst` list backing blocks in a runtime loop. `count` is an `i32` local; `src`
/// and `dst` are `i32` locals holding list base pointers. `elem_ty` is the list's
/// element IR type, which decides the per-element copy:
///
/// - a **scalar or `string`** element is copied word-for-word by a `SLOT_SIZE`-
///   aligned `i64` load/store — a `string` is immutable, so sharing its pointer IS
///   its value-semantic copy;
/// - a **mutable-aggregate** element (a `struct` or a nested `list`) is
///   DEEP-COPIED: the element pointer is loaded from `src`, `emit_deep_copy`
///   produces a fresh independent record, and that fresh pointer is stored into
///   `dst`. This mirrors [`emit_copy_element_at`] (the array path) and the
///   interpreters' recursive `Value::clone`, so mutating an element of one list
///   copy is never observable through another.
pub(crate) fn emit_list_copy_elems(
    ctx: &mut LowerCtx,
    elem_ty: &TypeRef,
    src: u32,
    dst: u32,
    count: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let deep = is_mutable_aggregate(elem_ty, ctx.structs, ctx.enums);
    let i = ctx.add_local(WasmValType::I32);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, i);
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // break when i >= count
    get_local(out, i);
    get_local(out, count);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of the block)
    write_uleb(out, 1);
    // off = LIST_DATA_OFF + i * SLOT_SIZE
    let off = ctx.add_local(WasmValType::I32);
    get_local(out, i);
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul
    out.push(0x41); // i32.const LIST_DATA_OFF
    write_sleb(out, LIST_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    set_local(out, off);
    if deep {
        // dst_addr = dst + off; load the element pointer from src + off, deep-copy
        // it into a fresh record, store that fresh pointer into dst.
        get_local(out, dst);
        get_local(out, off);
        out.push(0x6a); // i32.add -> dst addr
        get_local(out, src);
        get_local(out, off);
        out.push(0x6a); // i32.add -> src addr
        emit_load(WasmValType::I32, out);
        emit_deep_copy(ctx, elem_ty, out)?;
        emit_store(WasmValType::I32, out);
    } else {
        // memory[dst + off] = memory[src + off] (one 8-byte word: scalar or a
        // shared immutable `string` pointer).
        get_local(out, dst);
        get_local(out, off);
        out.push(0x6a); // i32.add -> dst addr
        get_local(out, src);
        get_local(out, off);
        out.push(0x6a); // i32.add -> src addr
        emit_load(WasmValType::I64, out);
        emit_store(WasmValType::I64, out);
    }
    // i += 1
    get_local(out, i);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, i);
    out.push(0x0c); // br 0 (repeat)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
    Ok(())
}

/// Deep-copy the growable `list<T>` whose pointer is on the stack, leaving a fresh
/// independent `[len][cap][slots]` block's pointer on the stack. The copy keeps the
/// source's `len` and `cap` and duplicates its `len` element slots (deep-copying a
/// mutable-aggregate element per [`emit_list_copy_elems`]). This is the WASM
/// realization of the interpreters' recursive `Value::clone` on a list: mutating
/// the copy (or the original), or an element of either, is never observable through
/// the other pointer. `elem_ty` is the element IR type.
pub(crate) fn emit_list_deep_copy(
    ctx: &mut LowerCtx,
    elem_ty: &TypeRef,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let src = ctx.add_local(WasmValType::I32);
    set_local(out, src);
    // len = load [src + LIST_LEN_OFF], cap = load [src + LIST_CAP_OFF]
    let len = ctx.add_local(WasmValType::I32);
    get_local(out, src);
    emit_load_at(WasmValType::I32, LIST_LEN_OFF, out);
    set_local(out, len);
    let cap = ctx.add_local(WasmValType::I32);
    get_local(out, src);
    emit_load_at(WasmValType::I32, LIST_CAP_OFF, out);
    set_local(out, cap);
    // dst = __alloc(LIST_DATA_OFF + cap * SLOT_SIZE)
    emit_list_block_size(cap, out);
    let dst = alloc_runtime(ctx, out);
    // dst.len = len; dst.cap = cap
    get_local(out, dst);
    get_local(out, len);
    emit_store_at(WasmValType::I32, LIST_LEN_OFF, out);
    get_local(out, dst);
    get_local(out, cap);
    emit_store_at(WasmValType::I32, LIST_CAP_OFF, out);
    // copy the `len` live element slots
    emit_list_copy_elems(ctx, elem_ty, src, dst, len, out)?;
    get_local(out, dst);
    Ok(())
}

/// Lower `list_new() -> list<T>`: `__alloc` an empty `[len=0][cap=LIST_INITIAL_CAP]
/// [slots...]` block and leave its pointer on the stack. Allocating a small initial
/// capacity means the first few `push`es do not each realloc.
pub(crate) fn lower_list_new(
    ctx: &mut LowerCtx,
    args: &[IrExpr],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    if !args.is_empty() {
        return Err(format!("list_new expects 0 arguments, got {}", args.len()));
    }
    let ptr = alloc_bytes(ctx, LIST_DATA_OFF + LIST_INITIAL_CAP * SLOT_SIZE, out);
    get_local(out, ptr);
    out.push(0x41); // i32.const 0 (len)
    write_sleb(out, 0);
    emit_store_at(WasmValType::I32, LIST_LEN_OFF, out);
    get_local(out, ptr);
    out.push(0x41); // i32.const LIST_INITIAL_CAP (cap)
    write_sleb(out, LIST_INITIAL_CAP as i64);
    emit_store_at(WasmValType::I32, LIST_CAP_OFF, out);
    get_local(out, ptr);
    Ok(())
}

/// Lower `push(l, x) -> list<T>` (value-semantic append): deep-copy `l`, grow the
/// copy when it is full (double the capacity, or seed `LIST_INITIAL_CAP` from an
/// empty list, reallocating and copying the live elements — the old block is
/// orphaned in the no-reclaim bump heap), store `x` into slot `len`, bump `len`,
/// and leave the fresh list pointer on the stack. Because `push` always returns a
/// NEW list, `l = push(l, x)` matches the interpreters' `Value::clone`-then-append.
pub(crate) fn lower_list_push(
    ctx: &mut LowerCtx,
    list: &IrExpr,
    value: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = supported_list_element(&list.ty, ctx.structs, ctx.enums).ok_or_else(|| {
        format!(
            "push expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let slot_ty = collection_slot_type(&elem_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element type `{}` is unsupported", elem_ty.name))?;
    let deep_elem = is_mutable_aggregate(&elem_ty, ctx.structs, ctx.enums);
    // Deep-copy the source list into a fresh, independent block (value semantics).
    lower_expr(ctx, list, out)?;
    emit_list_deep_copy(ctx, &elem_ty, out)?;
    let lst = ctx.add_local(WasmValType::I32);
    set_local(out, lst);
    // Grow if len == cap.
    let len = ctx.add_local(WasmValType::I32);
    get_local(out, lst);
    emit_load_at(WasmValType::I32, LIST_LEN_OFF, out);
    set_local(out, len);
    let cap = ctx.add_local(WasmValType::I32);
    get_local(out, lst);
    emit_load_at(WasmValType::I32, LIST_CAP_OFF, out);
    set_local(out, cap);
    // if len >= cap { grow }
    get_local(out, len);
    get_local(out, cap);
    out.push(0x4e); // i32.ge_s
    out.push(0x04); // if
    out.push(0x40); // void block type
    emit_list_grow(ctx, &elem_ty, lst, len, cap, out)?;
    out.push(0x0b); // end if
    // slot address of element `len`: lst + LIST_DATA_OFF + len * SLOT_SIZE
    get_local(out, lst);
    get_local(out, len);
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul
    out.push(0x41); // i32.const LIST_DATA_OFF
    write_sleb(out, LIST_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    out.push(0x6a); // i32.add -> element slot address
    lower_expr(ctx, value, out)?; // the value to append
    if deep_elem {
        // A mutable-aggregate element is stored as an INDEPENDENT deep copy so a
        // later mutation of the source value never leaks into the list, matching the
        // interpreters (an argument `Value` is cloned before it is pushed).
        emit_deep_copy(ctx, &elem_ty, out)?;
    }
    emit_store(slot_ty, out);
    // len += 1
    get_local(out, lst);
    get_local(out, len);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    emit_store_at(WasmValType::I32, LIST_LEN_OFF, out);
    get_local(out, lst);
    Ok(())
}

/// Grow the list in `lst` (an `i32` local) so it has room past `len` (== `cap`):
/// compute `new_cap = if cap == 0 { LIST_INITIAL_CAP } else { cap * 2 }`, `__alloc`
/// a fresh block, copy the `len` live elements (deep-copying a mutable-aggregate
/// element per [`emit_list_copy_elems`], with `elem_ty` the element type), write the
/// new `cap` and preserved `len`, and update `lst` to the new pointer. `len` and
/// `cap` locals are refreshed so the caller sees the post-grow capacity; the old
/// block is orphaned. The elements copied here already came from a fresh deep copy
/// of the source list, so the grow reuses their independent records without a second
/// deep recursion — but the copy still routes through `emit_list_copy_elems`, whose
/// per-element deep-copy keeps the grown block's elements independent of the
/// pre-grow block that is about to be orphaned.
pub(crate) fn emit_list_grow(
    ctx: &mut LowerCtx,
    elem_ty: &TypeRef,
    lst: u32,
    len: u32,
    cap: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // new_cap = cap == 0 ? LIST_INITIAL_CAP : cap * 2
    let new_cap = ctx.add_local(WasmValType::I32);
    get_local(out, cap);
    out.push(0x45); // i32.eqz
    out.push(0x04); // if -> i32
    out.push(0x7f); // result i32
    out.push(0x41); // i32.const LIST_INITIAL_CAP
    write_sleb(out, LIST_INITIAL_CAP as i64);
    out.push(0x05); // else
    get_local(out, cap);
    out.push(0x41); // i32.const 2
    write_sleb(out, 2);
    out.push(0x6c); // i32.mul
    out.push(0x0b); // end if
    set_local(out, new_cap);
    // dst = __alloc(LIST_DATA_OFF + new_cap * SLOT_SIZE)
    emit_list_block_size(new_cap, out);
    let dst = alloc_runtime(ctx, out);
    // dst.len = len; dst.cap = new_cap
    get_local(out, dst);
    get_local(out, len);
    emit_store_at(WasmValType::I32, LIST_LEN_OFF, out);
    get_local(out, dst);
    get_local(out, new_cap);
    emit_store_at(WasmValType::I32, LIST_CAP_OFF, out);
    // copy the `len` live elements from the old block (lst) to dst
    emit_list_copy_elems(ctx, elem_ty, lst, dst, len, out)?;
    // lst = dst; cap = new_cap (len is unchanged)
    get_local(out, dst);
    set_local(out, lst);
    get_local(out, new_cap);
    set_local(out, cap);
    Ok(())
}

/// Lower `get(l, i) -> T`: load element `i` from `l + LIST_DATA_OFF + i*SLOT_SIZE`.
/// The interpreters bounds-check and raise `L0413`; the WASM backend relies on
/// linear-memory trapping for a truly out-of-range index, so in-bounds reads match
/// the interpreters exactly and an OOB read traps (a consistent, documented
/// behavior) instead of returning a poisoned value.
///
/// The interpreters return `values[i].clone()` — a DEEP CLONE of the element — so a
/// mutable-aggregate element (`struct`/nested `list`) is loaded and then
/// `emit_deep_copy`'d into a fresh independent record before it leaves `get`.
/// Mutating the returned copy (or pushing it into another list and mutating that)
/// therefore never affects the original list's element, exactly like the
/// interpreters. A scalar or immutable `string` element needs no copy (a `string`
/// is shared, which IS its value-semantic clone).
pub(crate) fn lower_list_get(
    ctx: &mut LowerCtx,
    list: &IrExpr,
    index: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = supported_list_element(&list.ty, ctx.structs, ctx.enums).ok_or_else(|| {
        format!(
            "get expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let slot_ty = collection_slot_type(&elem_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element type `{}` is unsupported", elem_ty.name))?;
    lower_expr(ctx, list, out)?; // base pointer
    emit_list_elem_offset(ctx, index, out)?; // += LIST_DATA_OFF + index * SLOT_SIZE
    emit_load(slot_ty, out);
    if is_mutable_aggregate(&elem_ty, ctx.structs, ctx.enums) {
        // Return an independent deep copy (the interpreters' `values[i].clone()`).
        emit_deep_copy(ctx, &elem_ty, out)?;
    }
    Ok(())
}

/// Lower `set(l, i, x) -> list<T>` (value-semantic replace): deep-copy `l`, store
/// `x` into element slot `i` of the copy, and leave the fresh list pointer on the
/// stack. In-bounds writes match the interpreters; an OOB index traps on the
/// linear-memory store, consistent with `get`.
pub(crate) fn lower_list_set(
    ctx: &mut LowerCtx,
    list: &IrExpr,
    index: &IrExpr,
    value: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = supported_list_element(&list.ty, ctx.structs, ctx.enums).ok_or_else(|| {
        format!(
            "set expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let slot_ty = collection_slot_type(&elem_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element type `{}` is unsupported", elem_ty.name))?;
    let deep_elem = is_mutable_aggregate(&elem_ty, ctx.structs, ctx.enums);
    lower_expr(ctx, list, out)?;
    emit_list_deep_copy(ctx, &elem_ty, out)?;
    let lst = ctx.add_local(WasmValType::I32);
    set_local(out, lst);
    // element slot address in the copy
    get_local(out, lst);
    emit_list_elem_offset(ctx, index, out)?;
    lower_expr(ctx, value, out)?;
    if deep_elem {
        // Store an independent deep copy of the replacement so a later mutation of
        // the source value never leaks into the list (interpreter clone semantics).
        emit_deep_copy(ctx, &elem_ty, out)?;
    }
    emit_store(slot_ty, out);
    get_local(out, lst);
    Ok(())
}

/// Lower `pop(l) -> list<T>` (value-semantic remove-last): deep-copy `l`, decrement
/// the copy's `len` (dropping the last element in place — the slot stays allocated,
/// exactly like the interpreters' `Vec::pop` shrinks the length), and leave the
/// fresh list pointer on the stack. Popping an empty list is `L0413` on the
/// interpreters; the WASM path decrements to `-1` len, so the enclosing program is
/// expected to keep the same non-empty precondition the interpreters require.
pub(crate) fn lower_list_pop(
    ctx: &mut LowerCtx,
    list: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = supported_list_element(&list.ty, ctx.structs, ctx.enums).ok_or_else(|| {
        format!(
            "pop expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    lower_expr(ctx, list, out)?;
    emit_list_deep_copy(ctx, &elem_ty, out)?;
    let lst = ctx.add_local(WasmValType::I32);
    set_local(out, lst);
    // len -= 1
    get_local(out, lst);
    get_local(out, lst);
    emit_load_at(WasmValType::I32, LIST_LEN_OFF, out);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6b); // i32.sub
    emit_store_at(WasmValType::I32, LIST_LEN_OFF, out);
    get_local(out, lst);
    Ok(())
}

/// After a list base pointer is on the stack, add `LIST_DATA_OFF + index*SLOT_SIZE`
/// so the top of stack is the element slot address. The `index` expression is an
/// `i64`, truncated to `i32` (`i32.wrap_i64`) exactly like array indexing.
pub(crate) fn emit_list_elem_offset(
    ctx: &mut LowerCtx,
    index: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    lower_expr(ctx, index, out)?; // index (i64)
    out.push(0xa7); // i32.wrap_i64
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul -> index * SLOT_SIZE
    out.push(0x41); // i32.const LIST_DATA_OFF
    write_sleb(out, LIST_DATA_OFF as i64);
    out.push(0x6a); // i32.add -> LIST_DATA_OFF + index * SLOT_SIZE
    out.push(0x6a); // i32.add base + that offset
    Ok(())
}

// -- Growable map codegen ----------------------------------------------------

/// Push `MAP_DATA_OFF + cap * MAP_ENTRY_SIZE` (the byte size of a map backing
/// block with `cap` entry records) onto the stack, given an `i32` local holding
/// `cap`.
pub(crate) fn emit_map_block_size(cap_local: u32, out: &mut Vec<u8>) {
    get_local(out, cap_local);
    out.push(0x41); // i32.const MAP_ENTRY_SIZE
    write_sleb(out, MAP_ENTRY_SIZE as i64);
    out.push(0x6c); // i32.mul  -> cap * MAP_ENTRY_SIZE
    out.push(0x41); // i32.const MAP_DATA_OFF
    write_sleb(out, MAP_DATA_OFF as i64);
    out.push(0x6a); // i32.add  -> MAP_DATA_OFF + cap * MAP_ENTRY_SIZE
}

/// Push `base + MAP_DATA_OFF + entry * MAP_ENTRY_SIZE` (the address of entry
/// record `entry`) onto the stack. `base` and `entry` are `i32` locals.
pub(crate) fn emit_map_entry_addr(base: u32, entry: u32, out: &mut Vec<u8>) {
    get_local(out, base);
    get_local(out, entry);
    out.push(0x41); // i32.const MAP_ENTRY_SIZE
    write_sleb(out, MAP_ENTRY_SIZE as i64);
    out.push(0x6c); // i32.mul -> entry * MAP_ENTRY_SIZE
    out.push(0x41); // i32.const MAP_DATA_OFF
    write_sleb(out, MAP_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    out.push(0x6a); // i32.add -> base + MAP_DATA_OFF + entry * MAP_ENTRY_SIZE
}

/// Copy the first `count` entry records (each `MAP_ENTRY_SIZE` = two `SLOT_SIZE`
/// words) from `src` to `dst` map backing blocks in a runtime loop. `count`, `src`,
/// and `dst` are `i32` locals; `value_ty` is the map's value IR type. A map KEY is
/// always a scalar or an immutable `string` pointer (a `string` is compared by
/// content but shared on copy), so the key word is copied flat. The VALUE word is:
///
/// - copied FLAT for a scalar or immutable `string` value — sharing the pointer IS
///   the value-semantic copy;
/// - DEEP-COPIED for a mutable-aggregate value (a `struct`, i.e. `map<K, struct>`):
///   the value pointer is loaded from `src`, `emit_deep_copy` produces a fresh
///   independent record, and that fresh pointer is stored into `dst` — matching the
///   interpreters' recursive `Value::clone` on a map, so mutating a value of one map
///   copy is never observable through another.
pub(crate) fn emit_map_copy_entries(
    ctx: &mut LowerCtx,
    value_ty: &TypeRef,
    src: u32,
    dst: u32,
    count: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let deep_value = is_mutable_aggregate(value_ty, ctx.structs, ctx.enums);
    let i = ctx.add_local(WasmValType::I32);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, i);
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // break when i >= count
    get_local(out, i);
    get_local(out, count);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of the block)
    write_uleb(out, 1);
    // off = MAP_DATA_OFF + i * MAP_ENTRY_SIZE
    let off = ctx.add_local(WasmValType::I32);
    get_local(out, i);
    out.push(0x41); // i32.const MAP_ENTRY_SIZE
    write_sleb(out, MAP_ENTRY_SIZE as i64);
    out.push(0x6c); // i32.mul
    out.push(0x41); // i32.const MAP_DATA_OFF
    write_sleb(out, MAP_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    set_local(out, off);
    // Copy the key word flat (a scalar or a shared immutable `string` pointer).
    get_local(out, dst);
    get_local(out, off);
    out.push(0x6a); // i32.add -> dst key addr
    get_local(out, src);
    get_local(out, off);
    out.push(0x6a); // i32.add -> src key addr
    emit_load_at(WasmValType::I64, 0, out);
    emit_store_at(WasmValType::I64, 0, out);
    // Copy the value word at MAP_VALUE_OFF: flat for a scalar/string, deep for a
    // mutable aggregate.
    if deep_value {
        get_local(out, dst);
        get_local(out, off);
        out.push(0x6a); // i32.add -> dst entry addr
        get_local(out, src);
        get_local(out, off);
        out.push(0x6a); // i32.add -> src entry addr
        emit_load_at(WasmValType::I32, MAP_VALUE_OFF, out);
        emit_deep_copy(ctx, value_ty, out)?;
        emit_store_at(WasmValType::I32, MAP_VALUE_OFF, out);
    } else {
        get_local(out, dst);
        get_local(out, off);
        out.push(0x6a); // i32.add -> dst entry addr
        get_local(out, src);
        get_local(out, off);
        out.push(0x6a); // i32.add -> src entry addr
        emit_load_at(WasmValType::I64, MAP_VALUE_OFF, out);
        emit_store_at(WasmValType::I64, MAP_VALUE_OFF, out);
    }
    // i += 1
    get_local(out, i);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, i);
    out.push(0x0c); // br 0 (repeat)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
    Ok(())
}

/// Deep-copy the growable `map<K, V>` whose pointer is on the stack, leaving a
/// fresh independent `[len][cap][entries]` block's pointer on the stack. The copy
/// keeps the source's `len` and `cap` and duplicates its `len` live entry records
/// (each two words; a mutable-aggregate value is deep-copied per
/// [`emit_map_copy_entries`], with `value_ty` the value type). This is the WASM
/// realization of the interpreters' recursive clone on a map: mutating the copy (or
/// the original), or a value of either, is never observable through the other.
pub(crate) fn emit_map_deep_copy(
    ctx: &mut LowerCtx,
    value_ty: &TypeRef,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let src = ctx.add_local(WasmValType::I32);
    set_local(out, src);
    // len = load [src + MAP_LEN_OFF], cap = load [src + MAP_CAP_OFF]
    let len = ctx.add_local(WasmValType::I32);
    get_local(out, src);
    emit_load_at(WasmValType::I32, MAP_LEN_OFF, out);
    set_local(out, len);
    let cap = ctx.add_local(WasmValType::I32);
    get_local(out, src);
    emit_load_at(WasmValType::I32, MAP_CAP_OFF, out);
    set_local(out, cap);
    // dst = __alloc(MAP_DATA_OFF + cap * MAP_ENTRY_SIZE)
    emit_map_block_size(cap, out);
    let dst = alloc_runtime(ctx, out);
    // dst.len = len; dst.cap = cap
    get_local(out, dst);
    get_local(out, len);
    emit_store_at(WasmValType::I32, MAP_LEN_OFF, out);
    get_local(out, dst);
    get_local(out, cap);
    emit_store_at(WasmValType::I32, MAP_CAP_OFF, out);
    // copy the `len` live entry records
    emit_map_copy_entries(ctx, value_ty, src, dst, len, out)?;
    get_local(out, dst);
    Ok(())
}

/// Lower `map_new() -> map<K, V>`: `__alloc` an empty
/// `[len=0][cap=MAP_INITIAL_CAP][entries...]` block and leave its pointer on the
/// stack. A small initial capacity means the first few `map_set`s do not realloc.
pub(crate) fn lower_map_new(
    ctx: &mut LowerCtx,
    args: &[IrExpr],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    if !args.is_empty() {
        return Err(format!("map_new expects 0 arguments, got {}", args.len()));
    }
    let ptr = alloc_bytes(ctx, MAP_DATA_OFF + MAP_INITIAL_CAP * MAP_ENTRY_SIZE, out);
    get_local(out, ptr);
    out.push(0x41); // i32.const 0 (len)
    write_sleb(out, 0);
    emit_store_at(WasmValType::I32, MAP_LEN_OFF, out);
    get_local(out, ptr);
    out.push(0x41); // i32.const MAP_INITIAL_CAP (cap)
    write_sleb(out, MAP_INITIAL_CAP as i64);
    emit_store_at(WasmValType::I32, MAP_CAP_OFF, out);
    get_local(out, ptr);
    Ok(())
}

/// Emit the equality opcode comparing two SCALAR key values (already pushed) of
/// WASM slot type `key_ty`. Scalar keys use `i64.eq` (`i64`/fixed-width), `i32.eq`
/// (`bool`/`char`/`byte`), or the ordered `f*.eq` (floats), matching how the
/// interpreters compare `Value` keys by content. A `string` key is NOT a scalar
/// and is compared by content via [`emit_string_eq`], never this opcode.
pub(crate) fn emit_key_eq(key_ty: WasmValType, out: &mut Vec<u8>) {
    match key_ty {
        WasmValType::I32 => out.push(0x46), // i32.eq
        WasmValType::I64 => out.push(0x51), // i64.eq
        WasmValType::F32 => out.push(0x5b), // f32.eq
        WasmValType::F64 => out.push(0x61), // f64.eq
    }
}

/// Emit a CONTENT equality test for two `string` pointers `a` and `b` (both `i32`
/// locals pointing at `[char_len: i32][byte_len: i32][utf8 bytes]` records),
/// leaving an `i32` boolean (1 if the strings hold identical bytes, else 0) on the
/// stack. This matches the interpreters' `Value::String` equality: two DISTINCT
/// string objects with the same bytes compare equal (content, not pointer
/// identity). The test first compares the `byte_len` headers; if they differ the
/// result is 0 without touching the bytes. If they match, it walks the UTF-8 bytes
/// front-to-back, returning 0 on the first differing byte and 1 once every byte
/// matched (a zero-length string trivially matches). A pointer-identity fast path
/// (`a == b`) short-circuits when the same record is passed for both sides.
pub(crate) fn emit_string_eq(ctx: &mut LowerCtx, a: u32, b: u32, out: &mut Vec<u8>) -> u32 {
    let result = ctx.add_local(WasmValType::I32);
    // Fast path: identical pointers are the same string, so equal.
    get_local(out, a);
    get_local(out, b);
    out.push(0x46); // i32.eq
    out.push(0x04); // if -> void
    out.push(0x40);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    set_local(out, result);
    out.push(0x05); // else
    // Compare byte-length headers.
    let byte_len = ctx.add_local(WasmValType::I32);
    get_local(out, a);
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    set_local(out, byte_len);
    get_local(out, byte_len);
    get_local(out, b);
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    out.push(0x47); // i32.ne
    out.push(0x04); // if -> void  (byte lengths differ => not equal)
    out.push(0x40);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, result);
    out.push(0x05); // else  (equal lengths: walk the bytes)
    // Assume equal until a byte mismatches; scan i in [0, byte_len).
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    set_local(out, result);
    let i = ctx.add_local(WasmValType::I32);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, i);
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // break when i >= byte_len
    get_local(out, i);
    get_local(out, byte_len);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of the block)
    write_uleb(out, 1);
    // if load8_u(a + STR_DATA_OFF + i) != load8_u(b + STR_DATA_OFF + i) { result = 0; break }
    get_local(out, a);
    get_local(out, i);
    out.push(0x6a); // i32.add
    out.push(0x2d); // i32.load8_u
    write_uleb(out, 0); // align 0
    write_uleb(out, STR_DATA_OFF as u64);
    get_local(out, b);
    get_local(out, i);
    out.push(0x6a); // i32.add
    out.push(0x2d); // i32.load8_u
    write_uleb(out, 0); // align 0
    write_uleb(out, STR_DATA_OFF as u64);
    out.push(0x47); // i32.ne
    out.push(0x04); // if -> void
    out.push(0x40);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, result);
    out.push(0x0c); // br 2 (out of the block)
    write_uleb(out, 2);
    out.push(0x0b); // end if
    // i += 1
    get_local(out, i);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, i);
    out.push(0x0c); // br 0 (repeat)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
    out.push(0x0b); // end if (byte-length mismatch)
    out.push(0x0b); // end if (pointer fast path)
    result
}

/// Linear-scan the map in `map_local` for the entry whose key equals the value in
/// `key_local` (slot type `key_ty`), returning a fresh `i32` local holding the
/// matching entry index, or the map's `len` if no key matched (the "found index
/// else len" convention: `map_set` appends at `len`, `map_get`/`map_has` treat
/// `index == len` as absent). The scan visits entries front-to-back so the FIRST
/// matching key wins, mirroring the interpreters' insertion-ordered association
/// list. When `key_is_string`, the key slot holds an `i32` string POINTER and keys
/// are compared by CONTENT via [`emit_string_eq`] (decoded bytes, so two distinct
/// string objects with equal bytes match) instead of an integer slot compare;
/// otherwise the scalar `key_ty` compare of [`emit_key_eq`] is used. `len` is
/// loaded into a caller-visible local so callers can reuse it.
pub(crate) fn emit_map_find(
    ctx: &mut LowerCtx,
    map_local: u32,
    key_local: u32,
    key_ty: WasmValType,
    key_is_string: bool,
    len_local: u32,
    out: &mut Vec<u8>,
) -> u32 {
    // len = load [map + MAP_LEN_OFF]
    get_local(out, map_local);
    emit_load_at(WasmValType::I32, MAP_LEN_OFF, out);
    set_local(out, len_local);
    // found = len (sentinel: "not found")
    let found = ctx.add_local(WasmValType::I32);
    get_local(out, len_local);
    set_local(out, found);
    // i = 0
    let i = ctx.add_local(WasmValType::I32);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, i);
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // break when i >= len
    get_local(out, i);
    get_local(out, len_local);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of the block)
    write_uleb(out, 1);
    // if key(entry i) == key_local { found = i; break }
    if key_is_string {
        // Load the entry's stored string pointer, then compare by content.
        let entry_key = ctx.add_local(WasmValType::I32);
        emit_map_entry_addr(map_local, i, out); // entry addr
        emit_load(WasmValType::I32, out); // load key slot (string pointer at offset 0)
        set_local(out, entry_key);
        let eq = emit_string_eq(ctx, entry_key, key_local, out);
        get_local(out, eq);
    } else {
        emit_map_entry_addr(map_local, i, out); // entry addr
        emit_load(key_ty, out); // load key slot (offset 0 within the entry)
        get_local(out, key_local);
        emit_key_eq(key_ty, out);
    }
    out.push(0x04); // if
    out.push(0x40); // void
    get_local(out, i);
    set_local(out, found);
    out.push(0x0c); // br 2 (out of the block, skipping the increment)
    write_uleb(out, 2);
    out.push(0x0b); // end if
    // i += 1
    get_local(out, i);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, i);
    out.push(0x0c); // br 0 (repeat)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
    found
}

/// Lower `map_set(m, k, v) -> map<K, V>` (value-semantic insert/update): deep-copy
/// `m`, scan the copy for `k`; if found, overwrite that entry's value slot in
/// place (preserving the entry's position/order); otherwise grow when full (double
/// the capacity, or seed `MAP_INITIAL_CAP` from an empty map) and append a new
/// `(k, v)` entry, bumping `len`. Leaves the fresh map pointer on the stack.
/// Because `map_set` always returns a NEW map, `m = map_set(m, k, v)` matches the
/// interpreters' clone-then-mutate on the insertion-ordered association list.
pub(crate) fn lower_map_set(
    ctx: &mut LowerCtx,
    map: &IrExpr,
    key: &IrExpr,
    value: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (key_ty, value_ty) =
        supported_map_kv(&map.ty, ctx.structs, ctx.enums).ok_or_else(|| {
            format!(
                "map_set expects a scalar/string key and a supported value but got `{}`",
                map.ty.name
            )
        })?;
    let key_slot = scalar_or_string_slot_type(&key_ty)
        .ok_or_else(|| format!("map key type `{}` is unsupported", key_ty.name))?;
    let key_is_string = key_ty.name == "string";
    let value_slot = collection_slot_type(&value_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("map value type `{}` is unsupported", value_ty.name))?;
    let deep_value = is_mutable_aggregate(&value_ty, ctx.structs, ctx.enums);

    // Deep-copy the source map into a fresh, independent block (value semantics).
    lower_expr(ctx, map, out)?;
    emit_map_deep_copy(ctx, &value_ty, out)?;
    let mp = ctx.add_local(WasmValType::I32);
    set_local(out, mp);
    // Evaluate the key once into a local for the scan and (on append) the store.
    lower_expr(ctx, key, out)?;
    let key_local = ctx.add_local(key_slot);
    set_local(out, key_local);

    // Scan for the key; `found == len` means "not present".
    let len = ctx.add_local(WasmValType::I32);
    let found = emit_map_find(ctx, mp, key_local, key_slot, key_is_string, len, out);

    // if found == len { append } else { overwrite value in place }
    get_local(out, found);
    get_local(out, len);
    out.push(0x46); // i32.eq
    out.push(0x04); // if
    out.push(0x40); // void
    // --- append branch ---
    // Grow if len == cap.
    let cap = ctx.add_local(WasmValType::I32);
    get_local(out, mp);
    emit_load_at(WasmValType::I32, MAP_CAP_OFF, out);
    set_local(out, cap);
    get_local(out, len);
    get_local(out, cap);
    out.push(0x4e); // i32.ge_s
    out.push(0x04); // if
    out.push(0x40); // void
    emit_map_grow(ctx, &value_ty, mp, len, cap, out)?;
    out.push(0x0b); // end if (grow)
    // entry addr of index `len`
    emit_map_entry_addr(mp, len, out);
    let entry_addr = ctx.add_local(WasmValType::I32);
    set_local(out, entry_addr);
    // store key at entry + 0
    get_local(out, entry_addr);
    get_local(out, key_local);
    emit_store_at(key_slot, 0, out);
    // store value at entry + MAP_VALUE_OFF (an independent deep copy for a
    // mutable-aggregate value, matching the interpreters' clone-before-insert).
    get_local(out, entry_addr);
    lower_expr(ctx, value, out)?;
    if deep_value {
        emit_deep_copy(ctx, &value_ty, out)?;
    }
    emit_store_at(value_slot, MAP_VALUE_OFF, out);
    // len += 1
    get_local(out, mp);
    get_local(out, len);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    emit_store_at(WasmValType::I32, MAP_LEN_OFF, out);
    out.push(0x05); // else
    // --- overwrite branch: store value into entry `found`'s value slot ---
    emit_map_entry_addr(mp, found, out);
    lower_expr(ctx, value, out)?;
    if deep_value {
        emit_deep_copy(ctx, &value_ty, out)?;
    }
    emit_store_at(value_slot, MAP_VALUE_OFF, out);
    out.push(0x0b); // end if (append/overwrite)

    get_local(out, mp);
    Ok(())
}

/// Grow the map in `mp` (an `i32` local) so it has room past `len` (== `cap`):
/// compute `new_cap = if cap == 0 { MAP_INITIAL_CAP } else { cap * 2 }`, `__alloc`
/// a fresh block, copy the `len` live entries (deep-copying a mutable-aggregate
/// value per [`emit_map_copy_entries`], with `value_ty` the value type), write the
/// new `cap` and preserved `len`, and update `mp` to the new pointer. `len` and
/// `cap` locals are refreshed so the caller sees the post-grow capacity; the old
/// block is orphaned.
pub(crate) fn emit_map_grow(
    ctx: &mut LowerCtx,
    value_ty: &TypeRef,
    mp: u32,
    len: u32,
    cap: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // new_cap = cap == 0 ? MAP_INITIAL_CAP : cap * 2
    let new_cap = ctx.add_local(WasmValType::I32);
    get_local(out, cap);
    out.push(0x45); // i32.eqz
    out.push(0x04); // if -> i32
    out.push(0x7f); // result i32
    out.push(0x41); // i32.const MAP_INITIAL_CAP
    write_sleb(out, MAP_INITIAL_CAP as i64);
    out.push(0x05); // else
    get_local(out, cap);
    out.push(0x41); // i32.const 2
    write_sleb(out, 2);
    out.push(0x6c); // i32.mul
    out.push(0x0b); // end if
    set_local(out, new_cap);
    // dst = __alloc(MAP_DATA_OFF + new_cap * MAP_ENTRY_SIZE)
    emit_map_block_size(new_cap, out);
    let dst = alloc_runtime(ctx, out);
    // dst.len = len; dst.cap = new_cap
    get_local(out, dst);
    get_local(out, len);
    emit_store_at(WasmValType::I32, MAP_LEN_OFF, out);
    get_local(out, dst);
    get_local(out, new_cap);
    emit_store_at(WasmValType::I32, MAP_CAP_OFF, out);
    // copy the `len` live entries from the old block (mp) to dst
    emit_map_copy_entries(ctx, value_ty, mp, dst, len, out)?;
    // mp = dst; cap = new_cap (len unchanged)
    get_local(out, dst);
    set_local(out, mp);
    get_local(out, new_cap);
    set_local(out, cap);
    Ok(())
}

/// Lower `map_get(m, k) -> option<V>`: deep-copy `m` is NOT needed (read-only),
/// scan for `k`, and construct `some(v)` (loading the found entry's value slot)
/// or `none`, reusing the enum/option linear-memory layout. `result_ty` is the
/// call's `option<V>` type, from which the `some`/`none` [`EnumLayout`] is built.
pub(crate) fn lower_map_get(
    ctx: &mut LowerCtx,
    result_ty: &TypeRef,
    map: &IrExpr,
    key: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (key_ty, value_ty) =
        supported_map_kv(&map.ty, ctx.structs, ctx.enums).ok_or_else(|| {
            format!(
                "map_get expects a scalar/string key and a supported value but got `{}`",
                map.ty.name
            )
        })?;
    let key_slot = scalar_or_string_slot_type(&key_ty)
        .ok_or_else(|| format!("map key type `{}` is unsupported", key_ty.name))?;
    let key_is_string = key_ty.name == "string";
    let value_slot = collection_slot_type(&value_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("map value type `{}` is unsupported", value_ty.name))?;
    let deep_value = is_mutable_aggregate(&value_ty, ctx.structs, ctx.enums);
    // The result `option<V>` enum layout: variant `some(V)` is tag 0, `none` tag 1.
    // Built directly (not via `enum_layout`) so a mutable-aggregate value type
    // (`map<K, struct>` -> `option<struct>`) is laid out — the `some` payload is one
    // `i32` pointer slot, deep-copied above so the option owns an independent value.
    // The produced option is consumed locally (matched to extract the value), so its
    // enum-level deep copy across a boundary is not exercised by this construction.
    let inner = result_ty.option_element().ok_or_else(|| {
        format!(
            "map_get result type `{}` is not an `option<V>` enum",
            result_ty.name
        )
    })?;
    if inner.name != value_ty.name {
        return Err(format!(
            "map_get result `option<{}>` does not match value type `{}`",
            inner.name, value_ty.name
        ));
    }
    let layout = build_layout(vec![
        ("some".to_string(), vec![inner]),
        ("none".to_string(), Vec::new()),
    ]);

    // Evaluate the map to a pointer local and the key to a slot local.
    lower_expr(ctx, map, out)?;
    let mp = ctx.add_local(WasmValType::I32);
    set_local(out, mp);
    lower_expr(ctx, key, out)?;
    let key_local = ctx.add_local(key_slot);
    set_local(out, key_local);

    let len = ctx.add_local(WasmValType::I32);
    let found = emit_map_find(ctx, mp, key_local, key_slot, key_is_string, len, out);

    // Allocate the option record once; fill it in either branch. Result pointer is
    // stashed so both branches leave the same pointer on the stack at the `end`.
    let opt = alloc_bytes(ctx, layout.size_bytes(), out);
    // if found == len { none } else { some(value) }
    get_local(out, found);
    get_local(out, len);
    out.push(0x46); // i32.eq
    out.push(0x04); // if
    out.push(0x40); // void
    // --- none branch: tag = 1 ---
    let none_tag = layout
        .tag_of("none")
        .ok_or_else(|| "option layout missing `none` variant".to_string())?;
    get_local(out, opt);
    out.push(0x41); // i32.const none_tag
    write_sleb(out, none_tag as i64);
    emit_store_at(WasmValType::I32, 0, out);
    out.push(0x05); // else
    // --- some branch: tag = 0, payload = entry(found).value ---
    let some_tag = layout
        .tag_of("some")
        .ok_or_else(|| "option layout missing `some` variant".to_string())?;
    get_local(out, opt);
    out.push(0x41); // i32.const some_tag
    write_sleb(out, some_tag as i64);
    emit_store_at(WasmValType::I32, 0, out);
    // opt payload slot = load entry(found).value. A mutable-aggregate value is
    // deep-copied so the option's payload is an INDEPENDENT record — mutating the
    // retrieved value never affects the map's stored value, matching the
    // interpreters' `map_get` returning a clone of the value.
    get_local(out, opt);
    emit_map_entry_addr(mp, found, out);
    emit_load_at(value_slot, MAP_VALUE_OFF, out);
    if deep_value {
        emit_deep_copy(ctx, &value_ty, out)?;
    }
    emit_store_at(value_slot, ENUM_PAYLOAD_BASE, out);
    out.push(0x0b); // end if

    get_local(out, opt);
    Ok(())
}

/// Lower `map_has(m, k) -> bool`: scan for `k`, leaving `found != len` (an `i32`
/// boolean: 1 if present, 0 if absent) on the stack.
pub(crate) fn lower_map_has(
    ctx: &mut LowerCtx,
    map: &IrExpr,
    key: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (key_ty, _value_ty) =
        supported_map_kv(&map.ty, ctx.structs, ctx.enums).ok_or_else(|| {
            format!(
                "map_has expects a scalar/string key and a supported value but got `{}`",
                map.ty.name
            )
        })?;
    let key_slot = scalar_or_string_slot_type(&key_ty)
        .ok_or_else(|| format!("map key type `{}` is unsupported", key_ty.name))?;
    let key_is_string = key_ty.name == "string";

    lower_expr(ctx, map, out)?;
    let mp = ctx.add_local(WasmValType::I32);
    set_local(out, mp);
    lower_expr(ctx, key, out)?;
    let key_local = ctx.add_local(key_slot);
    set_local(out, key_local);

    let len = ctx.add_local(WasmValType::I32);
    let found = emit_map_find(ctx, mp, key_local, key_slot, key_is_string, len, out);
    // result = found != len
    get_local(out, found);
    get_local(out, len);
    out.push(0x47); // i32.ne
    Ok(())
}

/// Lower `map_len(m) -> i64`: load the leading `i32` `len` header and extend to
/// `i64` (the builtin's interpreter result type). Reads offset 0, shared with the
/// string/array/list length header.
pub(crate) fn lower_map_len(
    ctx: &mut LowerCtx,
    map: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    supported_map_kv(&map.ty, ctx.structs, ctx.enums).ok_or_else(|| {
        format!(
            "map_len expects a scalar/string key and a supported value but got `{}`",
            map.ty.name
        )
    })?;
    lower_expr(ctx, map, out)?; // map pointer
    emit_load_at(WasmValType::I32, MAP_LEN_OFF, out);
    out.push(0xac); // i64.extend_i32_s
    Ok(())
}

/// Copy one struct field slot from `src + offset` to `dst + offset` (both are
/// `i32` local indices holding base pointers; `offset` is a constant byte offset).
/// A scalar/string slot is copied word-for-word (by its own WASM slot type); a
/// nested MUTABLE aggregate slot is deep-copied so nested mutation stays isolated.
pub(crate) fn emit_copy_slot(
    ctx: &mut LowerCtx,
    slot_ir_ty: &TypeRef,
    src: u32,
    dst: u32,
    offset: i32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let slot_ty = slot_val_type(slot_ir_ty, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("slot type `{}` is unsupported", slot_ir_ty.name))?;
    get_local(out, dst); // base for the store
    if is_mutable_aggregate(slot_ir_ty, ctx.structs, ctx.enums) {
        // Load the nested pointer, deep-copy it, store the fresh pointer.
        get_local(out, src);
        emit_load_at(WasmValType::I32, offset, out);
        emit_deep_copy(ctx, slot_ir_ty, out)?;
    } else {
        // Scalar or immutable string: copy the slot word by its own type.
        get_local(out, src);
        emit_load_at(slot_ty, offset, out);
    }
    emit_store_at(slot_ty, offset, out);
    Ok(())
}

/// Copy one array element from `src + off` to `dst + off`, where `off` is an `i32`
/// local holding the runtime byte offset (`LEN_HEADER + i * SLOT_SIZE`). A
/// scalar/string element is copied word-for-word; a nested MUTABLE aggregate
/// element is deep-copied. Mirrors [`emit_copy_slot`] but with a runtime offset.
pub(crate) fn emit_copy_element_at(
    ctx: &mut LowerCtx,
    elem_ir_ty: &TypeRef,
    src: u32,
    dst: u32,
    off: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let slot_ty = slot_val_type(elem_ir_ty, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("array element type `{}` is unsupported", elem_ir_ty.name))?;
    // dst_addr = dst + off
    get_local(out, dst);
    get_local(out, off);
    out.push(0x6a); // i32.add
    if is_mutable_aggregate(elem_ir_ty, ctx.structs, ctx.enums) {
        // src_addr = src + off; load the nested pointer; deep-copy; store fresh.
        get_local(out, src);
        get_local(out, off);
        out.push(0x6a); // i32.add
        emit_load(WasmValType::I32, out);
        emit_deep_copy(ctx, elem_ir_ty, out)?;
    } else {
        // src_addr = src + off; load the element word by its own type.
        get_local(out, src);
        get_local(out, off);
        out.push(0x6a); // i32.add
        emit_load(slot_ty, out);
    }
    emit_store(slot_ty, out);
    Ok(())
}

/// The `(byte offset, slot WASM type)` of a struct field, given the struct's
/// type and the field name.
pub(crate) fn struct_field_slot(
    ctx: &LowerCtx,
    struct_ty: &TypeRef,
    field: &str,
) -> Result<(i32, WasmValType), String> {
    let fields = ctx
        .structs
        .get(&struct_ty.name)
        .ok_or_else(|| format!("`{}` is not a struct", struct_ty.name))?;
    let position = fields
        .iter()
        .position(|(name, _)| name == field)
        .ok_or_else(|| format!("unknown field `{field}` on `{}`", struct_ty.name))?;
    let slot_ty = slot_val_type(&fields[position].1, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("field `{field}` has an unsupported type"))?;
    Ok((position as i32 * SLOT_SIZE, slot_ty))
}

/// Given a base pointer already on the stack, add `LEN_HEADER + index*SLOT_SIZE`
/// so the top of stack is the element slot address. The `index` expression is an
/// `i64`; it is truncated to `i32` for the address arithmetic.
pub(crate) fn lower_array_slot_offset(
    ctx: &mut LowerCtx,
    index: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // offset = LEN_HEADER + index * SLOT_SIZE (index is i64 -> i32).
    lower_expr(ctx, index, out)?;
    out.push(0xa7); // i32.wrap_i64
    out.push(0x41); // i32.const SLOT_SIZE
    write_sleb(out, SLOT_SIZE as i64);
    out.push(0x6c); // i32.mul
    out.push(0x41); // i32.const LEN_HEADER
    write_sleb(out, LEN_HEADER as i64);
    out.push(0x6a); // i32.add
    out.push(0x6a); // i32.add  (base + offset)
    Ok(())
}

/// Fold one assignment-path hop into the running address on the stack, returning
/// the hop's leaf IR type. On entry the current base/element pointer is on the
/// stack; on exit the slot address for this hop is on the stack.
pub(crate) fn lower_place_address(
    ctx: &mut LowerCtx,
    cur_ty: &TypeRef,
    place: &crate::IrPlace,
    out: &mut Vec<u8>,
) -> Result<TypeRef, String> {
    match place {
        crate::IrPlace::Field(field) => {
            let (offset, _) = struct_field_slot(ctx, cur_ty, field)?;
            if offset != 0 {
                out.push(0x41); // i32.const offset
                write_sleb(out, offset as i64);
                out.push(0x6a); // i32.add
            }
            let fields = ctx
                .structs
                .get(&cur_ty.name)
                .ok_or_else(|| format!("`{}` is not a struct", cur_ty.name))?;
            let field_ty = fields
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, ty)| ty.clone())
                .ok_or_else(|| format!("unknown field `{field}`"))?;
            Ok(field_ty)
        }
        crate::IrPlace::Index(index) => {
            let elem_ty = cur_ty
                .array_element()
                .ok_or_else(|| format!("indexing a non-array type `{}`", cur_ty.name))?;
            lower_array_slot_offset(ctx, index, out)?;
            Ok(elem_ty)
        }
    }
}

/// A non-mid-path store at a base pointer already on the stack followed by the
/// value: `emit_store` picks the opcode. Alignment `2` = 4-byte for `i32`, `3` =
/// 8-byte for `i64`/`f64` (offset 0).
pub(crate) fn emit_store(ty: WasmValType, out: &mut Vec<u8>) {
    emit_store_at(ty, 0, out);
}

/// Store the value on the stack (with the base pointer pushed just before it) at
/// `base + offset`.
pub(crate) fn emit_store_at(ty: WasmValType, offset: i32, out: &mut Vec<u8>) {
    let (opcode, align) = match ty {
        WasmValType::I32 => (0x36u8, 2u64), // i32.store
        WasmValType::I64 => (0x37, 3),      // i64.store
        WasmValType::F32 => (0x38, 2),      // f32.store (4-byte)
        WasmValType::F64 => (0x39, 3),      // f64.store
    };
    out.push(opcode);
    write_uleb(out, align);
    write_uleb(out, offset as u64);
}

/// Load a slot value from the address on the stack.
pub(crate) fn emit_load(ty: WasmValType, out: &mut Vec<u8>) {
    emit_load_at(ty, 0, out);
}

/// Load a slot value from `base + offset` (base pointer on the stack).
pub(crate) fn emit_load_at(ty: WasmValType, offset: i32, out: &mut Vec<u8>) {
    let (opcode, align) = match ty {
        WasmValType::I32 => (0x28u8, 2u64), // i32.load
        WasmValType::I64 => (0x29, 3),      // i64.load
        WasmValType::F32 => (0x2a, 2),      // f32.load (4-byte)
        WasmValType::F64 => (0x2b, 3),      // f64.load
    };
    out.push(opcode);
    write_uleb(out, align);
    write_uleb(out, offset as u64);
}

/// Emit the opcode(s) for a binary op given the operand WASM type.
pub(crate) fn emit_binary_op_typed(
    op: BinaryOp,
    ty: WasmValType,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    match ty {
        WasmValType::I64 => emit_i64_binop(op, out),
        WasmValType::F32 => emit_f32_binop(op, out),
        WasmValType::F64 => emit_f64_binop(op, out),
        WasmValType::I32 => emit_i32_binop(op, out),
    }
}

pub(crate) fn emit_i64_binop(op: BinaryOp, out: &mut Vec<u8>) -> Result<(), String> {
    let opcode = match op {
        BinaryOp::Add => 0x7c,
        BinaryOp::Subtract => 0x7d,
        BinaryOp::Multiply => 0x7e,
        BinaryOp::Divide => 0x7f,    // i64.div_s (traps on 0)
        BinaryOp::Remainder => 0x81, // i64.rem_s (traps on 0; returns 0 for MIN%-1)
        BinaryOp::Equal => 0x51,
        BinaryOp::NotEqual => 0x52,
        BinaryOp::Less => 0x53,         // lt_s
        BinaryOp::LessEqual => 0x57,    // le_s
        BinaryOp::Greater => 0x55,      // gt_s
        BinaryOp::GreaterEqual => 0x59, // ge_s
        // `and`/`or` short-circuit and the integer bitwise ops are deferred on
        // this backend; both are routed away before reaching this opcode table.
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr => unreachable!("handled by caller"),
    };
    out.push(opcode);
    Ok(())
}

/// Emit a signed `i64` division that matches the interpreters' wrapping
/// semantics on the one overflow case `i64::MIN / -1`. WASM `i64.div_s` traps on
/// that input (as well as on a zero divisor), but the interpreters use
/// `wrapping_div`, which yields `i64::MIN`. Operands (dividend then divisor) must
/// already be on the stack. Stash both, and when the divisor is `-1` compute
/// `0 - dividend` — wrapping negation is exactly `x / -1` across the whole `i64`
/// range, including `i64::MIN / -1 == i64::MIN`. Otherwise divide normally, which
/// still traps on a zero divisor exactly like before (division-by-zero behavior
/// is unchanged). Used for both the plain-`i64` and fixed-width signed division
/// paths, so the WASM backend stays bit-for-bit with the interpreters and the
/// native backend.
pub(crate) fn emit_i64_signed_div_guarded(ctx: &mut LowerCtx, out: &mut Vec<u8>) {
    let divisor = ctx.add_local(WasmValType::I64);
    let dividend = ctx.add_local(WasmValType::I64);
    // Stack holds [dividend, divisor]; pop them into locals (divisor is on top).
    set_local(out, divisor);
    set_local(out, dividend);
    // divisor == -1 ?
    get_local(out, divisor);
    out.push(0x42); // i64.const -1
    write_sleb(out, -1);
    out.push(0x51); // i64.eq
    out.push(0x04); // if
    out.push(0x7e); // block type: i64 result
    // then: 0 - dividend (wrapping negation == dividend / -1)
    out.push(0x42); // i64.const 0
    write_sleb(out, 0);
    get_local(out, dividend);
    out.push(0x7d); // i64.sub
    out.push(0x05); // else
    // else: dividend / divisor (traps only on a zero divisor)
    get_local(out, dividend);
    get_local(out, divisor);
    out.push(0x7f); // i64.div_s
    out.push(0x0b); // end
}

pub(crate) fn emit_f64_binop(op: BinaryOp, out: &mut Vec<u8>) -> Result<(), String> {
    let opcode = match op {
        BinaryOp::Add => 0xa0,
        BinaryOp::Subtract => 0xa1,
        BinaryOp::Multiply => 0xa2,
        BinaryOp::Divide => 0xa3,
        BinaryOp::Equal => 0x61,
        BinaryOp::NotEqual => 0x62,
        BinaryOp::Less => 0x63,
        BinaryOp::LessEqual => 0x65,
        BinaryOp::Greater => 0x64,
        BinaryOp::GreaterEqual => 0x66,
        // `and`/`or` short-circuit, `%` is integer-only, and the integer bitwise
        // ops are deferred; all are routed away before reaching this opcode table.
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::Remainder
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr => unreachable!("handled by caller"),
    };
    out.push(opcode);
    Ok(())
}

/// Emit a single-precision `f32` binary op. Arithmetic (`f32.add/sub/mul/div`)
/// keeps the result in single precision, so it is bit-identical to the
/// interpreter's real `f32`. The comparison ops (`f32.eq/ne/lt/le/gt/ge`) are
/// IEEE-754: relational compares and `==` are false when either operand is NaN,
/// `!=` is true — exactly the interpreter's (Rust `f32`) semantics.
pub(crate) fn emit_f32_binop(op: BinaryOp, out: &mut Vec<u8>) -> Result<(), String> {
    let opcode = match op {
        BinaryOp::Add => 0x92,
        BinaryOp::Subtract => 0x93,
        BinaryOp::Multiply => 0x94,
        BinaryOp::Divide => 0x95,
        BinaryOp::Equal => 0x5b,
        BinaryOp::NotEqual => 0x5c,
        BinaryOp::Less => 0x5d,
        BinaryOp::Greater => 0x5e,
        BinaryOp::LessEqual => 0x5f,
        BinaryOp::GreaterEqual => 0x60,
        // `and`/`or` short-circuit, `%` is integer-only, and the integer bitwise
        // ops are deferred; all are routed away before reaching this opcode table.
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::Remainder
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr => unreachable!("handled by caller"),
    };
    out.push(opcode);
    Ok(())
}

/// `i32` operands are `bool`/`char`/`byte`. Comparisons use the signed opcodes;
/// arithmetic is supported defensively.
pub(crate) fn emit_i32_binop(op: BinaryOp, out: &mut Vec<u8>) -> Result<(), String> {
    let opcode = match op {
        BinaryOp::Add => 0x6a,
        BinaryOp::Subtract => 0x6b,
        BinaryOp::Multiply => 0x6c,
        BinaryOp::Divide => 0x6d,    // i32.div_s
        BinaryOp::Remainder => 0x6f, // i32.rem_s
        BinaryOp::Equal => 0x46,
        BinaryOp::NotEqual => 0x47,
        BinaryOp::Less => 0x48,         // lt_s
        BinaryOp::LessEqual => 0x4c,    // le_s
        BinaryOp::Greater => 0x4a,      // gt_s
        BinaryOp::GreaterEqual => 0x4e, // ge_s
        // `and`/`or` short-circuit and the integer bitwise ops are deferred on
        // this backend; both are routed away before reaching this opcode table.
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr => unreachable!("handled by caller"),
    };
    out.push(opcode);
    Ok(())
}

/// The WASM value type an expression leaves on the stack, using the IR's type
/// annotation. `None` for a `void` expression. A pointer type (string/struct/
/// array) reports `i32`.
pub(crate) fn expr_val_type(ctx: &LowerCtx, expr: &IrExpr) -> Result<Option<WasmValType>, String> {
    if expr.ty.is_void() {
        return Ok(None);
    }
    if let Some(vt) = value_val_type(&expr.ty, ctx.structs, ctx.enums) {
        return Ok(Some(vt));
    }
    Err(format!(
        "expression has unsupported type `{}`",
        expr.ty.name
    ))
}

/// Whether a non-void function body always leaves a value / returns on every
/// path. Conservative: accept a trailing `Return(Some)`, a value-producing tail
/// `Expr`, an exhaustive `If` whose branches all guarantee a value, an exhaustive
/// `Match` whose arms all guarantee a value, or a `loop` whose body contains a
/// `Return`.
pub(crate) fn body_guarantees_value(body: &[IrStmt]) -> bool {
    match body.last() {
        Some(IrStmt::Return(Some(_))) => true,
        Some(IrStmt::Expr(expr)) => !expr.ty.is_void(),
        Some(IrStmt::If {
            branches,
            else_body,
            ..
        }) => {
            !else_body.is_empty()
                && body_guarantees_value(else_body)
                && branches.iter().all(|b| body_guarantees_value(&b.body))
        }
        // A `match` is exhaustive (semantics enforces it), so it guarantees a
        // value iff every arm body does. This is what makes a `match`-tail
        // function like `fn count o option<i64> -> i64` eligible.
        Some(IrStmt::Match { arms, .. }) => {
            !arms.is_empty() && arms.iter().all(|arm| body_guarantees_value(&arm.body))
        }
        Some(IrStmt::Loop { body, .. }) => stmts_contain_return(body),
        _ => false,
    }
}

pub(crate) fn stmts_contain_return(stmts: &[IrStmt]) -> bool {
    stmts.iter().any(|s| match s {
        IrStmt::Return(_) => true,
        IrStmt::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().any(|b| stmts_contain_return(&b.body))
                || stmts_contain_return(else_body)
        }
        IrStmt::Match { arms, .. } => arms.iter().any(|arm| stmts_contain_return(&arm.body)),
        IrStmt::While { body, .. } | IrStmt::Loop { body, .. } | IrStmt::For { body, .. } => {
            stmts_contain_return(body)
        }
        _ => false,
    })
}

// -- Local get/set helpers ---------------------------------------------------

pub(crate) fn get_local(out: &mut Vec<u8>, index: u32) {
    out.push(0x20);
    write_uleb(out, index as u64);
}

pub(crate) fn set_local(out: &mut Vec<u8>, index: u32) {
    out.push(0x21);
    write_uleb(out, index as u64);
}

// -- Binary encoder ----------------------------------------------------------

/// Unsigned LEB128.
pub(crate) fn write_uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Signed LEB128.
pub(crate) fn write_sleb(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7; // arithmetic shift
        let sign_bit = byte & 0x40;
        let done = (value == 0 && sign_bit == 0) || (value == -1 && sign_bit != 0);
        if !done {
            byte |= 0x80;
        }
        out.push(byte);
        if done {
            break;
        }
    }
}

/// A distinct signature (parameters + optional result). Functions with the same
/// signature share a type-section entry.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct FuncType {
    params: Vec<WasmValType>,
    result: Option<WasmValType>,
}

/// The internal, non-exported bump-allocator helper `__alloc(size i32) -> i32`.
/// It reads the mutable bump-pointer global, advances it by `size`, and returns
/// the old value (the freshly allocated offset). Struct/array construction calls
/// it to reserve their layout in linear memory.
pub(crate) fn alloc_helper() -> LoweredFunction {
    let mut body = Vec::new();
    body.push(0x23); // global.get
    write_uleb(&mut body, BUMP_GLOBAL_INDEX as u64); // old bump = return value
    body.push(0x23); // global.get
    write_uleb(&mut body, BUMP_GLOBAL_INDEX as u64);
    get_local(&mut body, 0); // size (param 0)
    body.push(0x6a); // i32.add
    body.push(0x24); // global.set
    write_uleb(&mut body, BUMP_GLOBAL_INDEX as u64);
    LoweredFunction {
        name: ALLOC_HELPER_NAME.to_string(),
        params: vec![WasmValType::I32],
        result: Some(WasmValType::I32),
        extra_locals: Vec::new(),
        body,
    }
}

/// The `env`-module field names of the imported host functions, in the order
/// that fixes their low WASM function indices (must match `*_FUNC_INDEX`).
const IMPORT_FIELD_NAMES: [&str; IMPORT_FUNC_COUNT as usize] =
    ["log_i64", "console_log", "dom_set_text"];

/// The signatures of the imported host functions, one per low function index,
/// reserved as the leading entries of the Type section so the Import section can
/// reference them by their index.
pub(crate) fn import_func_types() -> Vec<FuncType> {
    vec![
        // 0: env.log_i64(i64) -> void
        FuncType {
            params: vec![WasmValType::I64],
            result: None,
        },
        // 1: env.console_log(ptr i32, len i32) -> void
        FuncType {
            params: vec![WasmValType::I32, WasmValType::I32],
            result: None,
        },
        // 2: env.dom_set_text(id_ptr i32, id_len i32, text_ptr i32, text_len i32) -> void
        FuncType {
            params: vec![
                WasmValType::I32,
                WasmValType::I32,
                WasmValType::I32,
                WasmValType::I32,
            ],
            result: None,
        },
    ]
}

/// Index of the mutable `i32` bump-pointer global.
const BUMP_GLOBAL_INDEX: u32 = 0;

/// Export name of the internal bump-allocator helper. It is distinct from any
/// Lullaby identifier (double underscore prefix) so it cannot collide.
pub(crate) const ALLOC_HELPER_NAME: &str = "__alloc";

/// Encode the whole module: header + Type, Import, Function, Memory, Global,
/// Export, Code, and Data sections.
///
/// The imports (`env.log_i64`, `env.console_log`, `env.dom_set_text`) occupy WASM
/// function indices `0..IMPORT_FUNC_COUNT`, so every internally-defined function
/// is numbered from `IMPORT_FUNC_COUNT` up; the caller already assigned those
/// shifted indices. The internal `__alloc` helper
/// is appended after the user functions. `pool` supplies the interned
/// string-literal bytes seeded into the Data section and fixes the bump global's
/// initial value (past the reserved region and the whole literal pool).
pub(crate) fn encode_module(user_functions: &[LoweredFunction], pool: &StringPool) -> Vec<u8> {
    // All internally-defined functions, in module (index) order: the compiled
    // user functions, then the bump-allocator helper.
    let mut functions: Vec<LoweredFunction> = user_functions.to_vec();
    functions.push(alloc_helper());

    // Type table. Entries 0..IMPORT_FUNC_COUNT are reserved for the imported host
    // functions' signatures (so the Import section references them by index);
    // internal functions dedup against the whole table.
    //   0: env.log_i64      (i64) -> void
    //   1: env.console_log  (i32, i32) -> void
    //   2: env.dom_set_text (i32, i32, i32, i32) -> void
    let mut types: Vec<FuncType> = import_func_types();
    let mut type_of_func: Vec<u32> = Vec::with_capacity(functions.len());
    for f in &functions {
        let sig = FuncType {
            params: f.params.clone(),
            result: f.result,
        };
        let idx = match types.iter().position(|t| *t == sig) {
            Some(i) => i as u32,
            None => {
                types.push(sig);
                (types.len() - 1) as u32
            }
        };
        type_of_func.push(idx);
    }

    let mut module = Vec::new();
    // Magic + version.
    module.extend_from_slice(&[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);

    // Type section (id 1).
    {
        let mut section = Vec::new();
        write_uleb(&mut section, types.len() as u64);
        for t in &types {
            section.push(0x60); // func type
            write_uleb(&mut section, t.params.len() as u64);
            for p in &t.params {
                section.push(p.byte());
            }
            match t.result {
                Some(vt) => {
                    write_uleb(&mut section, 1);
                    section.push(vt.byte());
                }
                None => write_uleb(&mut section, 0),
            }
        }
        push_section(&mut module, 1, &section);
    }

    // Import section (id 2): the host functions from module `env`, in the fixed
    // order that defines their low WASM function indices (0, 1, 2). Each
    // references the reserved type-table entry with the same index.
    {
        let mut section = Vec::new();
        write_uleb(&mut section, IMPORT_FUNC_COUNT as u64);
        for (i, field) in IMPORT_FIELD_NAMES.iter().enumerate() {
            write_name(&mut section, "env");
            write_name(&mut section, field);
            section.push(0x00); // import kind: func
            write_uleb(&mut section, i as u64); // reserved type index
        }
        push_section(&mut module, 2, &section);
    }

    // Function section (id 3): type index per internal function.
    {
        let mut section = Vec::new();
        write_uleb(&mut section, functions.len() as u64);
        for &t in &type_of_func {
            write_uleb(&mut section, t as u64);
        }
        push_section(&mut module, 3, &section);
    }

    // Memory section (id 5): one memory, min 1 page, no maximum.
    {
        let mut section = Vec::new();
        write_uleb(&mut section, 1); // one memory
        section.push(0x00); // limits: flag 0 = min only
        write_uleb(&mut section, 1); // min 1 page (64 KiB)
        push_section(&mut module, 5, &section);
    }

    // Global section (id 6): the mutable `i32` bump pointer, initialized past the
    // reserved region AND the string-literal pool so `__alloc` never overwrites
    // static string data.
    {
        let mut section = Vec::new();
        write_uleb(&mut section, 1); // one global
        section.push(WasmValType::I32.byte()); // value type i32
        section.push(0x01); // mutable
        section.push(0x41); // i32.const (init expr)
        write_sleb(&mut section, pool.heap_base() as i64);
        section.push(0x0b); // end init expr
        push_section(&mut module, 6, &section);
    }

    // Export section (id 7): the linear memory, then every internal function by
    // name. Function export indices are the shifted (post-import) indices.
    {
        let mut section = Vec::new();
        write_uleb(&mut section, (functions.len() + 1) as u64); // +1 for memory
        write_name(&mut section, "memory");
        section.push(0x02); // export kind: mem
        write_uleb(&mut section, 0); // memory index 0
        for (i, f) in functions.iter().enumerate() {
            write_name(&mut section, &f.name);
            section.push(0x00); // export kind: func
            write_uleb(&mut section, IMPORT_FUNC_COUNT as u64 + i as u64);
        }
        push_section(&mut module, 7, &section);
    }

    // Code section (id 10).
    {
        let mut section = Vec::new();
        write_uleb(&mut section, functions.len() as u64);
        for f in &functions {
            let mut code = Vec::new();
            // Locals: run-length compressed consecutive same-type runs.
            let runs = compress_locals(&f.extra_locals);
            write_uleb(&mut code, runs.len() as u64);
            for (count, ty) in runs {
                write_uleb(&mut code, count as u64);
                code.push(ty.byte());
            }
            code.extend_from_slice(&f.body);
            code.push(0x0b); // end
            write_uleb(&mut section, code.len() as u64);
            section.extend_from_slice(&code);
        }
        push_section(&mut module, 10, &section);
    }

    // Data section (id 11): one active segment at offset 0 seeding the reserved
    // region [0, RESERVED_BASE) with zeros (so a handed-out pointer is never null)
    // followed by the interned string-literal pool starting at `RESERVED_BASE`.
    {
        let mut segment = vec![0u8; RESERVED_BASE as usize];
        segment.extend_from_slice(&pool.bytes);

        let mut section = Vec::new();
        write_uleb(&mut section, 1); // one data segment
        section.push(0x00); // segment kind 0: active, memory 0, offset expr
        section.push(0x41); // i32.const (offset expr)
        write_sleb(&mut section, 0);
        section.push(0x0b); // end offset expr
        write_uleb(&mut section, segment.len() as u64);
        section.extend_from_slice(&segment);
        push_section(&mut module, 11, &section);
    }

    module
}

/// Write a WASM name: length-prefixed UTF-8 bytes.
pub(crate) fn write_name(out: &mut Vec<u8>, name: &str) {
    let bytes = name.as_bytes();
    write_uleb(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// Run-length compress a local declaration list into `(count, type)` runs.
pub(crate) fn compress_locals(locals: &[WasmValType]) -> Vec<(u32, WasmValType)> {
    let mut runs: Vec<(u32, WasmValType)> = Vec::new();
    for &ty in locals {
        match runs.last_mut() {
            Some((count, last)) if *last == ty => *count += 1,
            _ => runs.push((1, ty)),
        }
    }
    runs
}

/// Append a section: `id`, byte length, then the section contents.
pub(crate) fn push_section(module: &mut Vec<u8>, id: u8, contents: &[u8]) {
    module.push(id);
    write_uleb(module, contents.len() as u64);
    module.extend_from_slice(contents);
}
