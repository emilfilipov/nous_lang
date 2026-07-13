//! Native x86-64 `.text` runtime helper emitters (RC allocator, list/map/string/drop
//! helpers). Split out of native_object.rs; each returns a self-contained
//! `HelperFunction` of raw machine code. See the parent module for the layout
//! constants and the small emit utilities these compose.

use super::*;

/// Emit the free-list allocator `__lullaby_alloc(payload size in rcx) -> payload
/// ptr in rax`.
///
/// Each block carries a 16-byte RC header `[size i64][refcount i64]` before the
/// payload; the returned pointer names the payload (`base + 16`), so every record
/// offset is unchanged and the refcount is at `[ptr - 8]`. The allocator first
/// scans the LIFO free list (`__lullaby_free_head`) for a first-fit block (stored
/// size ≥ needed); on a hit it unlinks the block, re-seeds its refcount to 1, and
/// returns it. Otherwise it bump-allocates from the reserved `.bss` region
/// (seeding the bump pointer to the region base on first use), writing the size
/// and a refcount of 1, and advancing the bump pointer 8-byte-rounded. A leaf (no
/// internal calls); uses only volatile registers.
pub(crate) fn emit_heap_alloc_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // r8 = need = payload size (rcx) + RC header.
    code.extend_from_slice(&[0x4C, 0x8D, 0x41, RC_HEADER_SIZE as u8]); // lea r8, [rcx + 16]

    // Free-list first-fit scan. r10 = &(prev's next slot) (starts at &free_head),
    // r11 = cur block base.
    code.extend_from_slice(&[0x4C, 0x8D, 0x15]); // lea r10, [rip + free_head]
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_FREE_HEAD_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x4D, 0x8B, 0x1A]); // mov r11, [r10]

    let scan = code.len();
    code.extend_from_slice(&[0x4D, 0x85, 0xDB]); // test r11, r11
    code.extend_from_slice(&[0x0F, 0x84]); // jz bump
    let bump_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x49, 0x8B, 0x03]); // mov rax, [r11] (block size)
    code.extend_from_slice(&[0x4C, 0x39, 0xC0]); // cmp rax, r8
    code.extend_from_slice(&[0x0F, 0x82]); // jb advance (block too small)
    let advance_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // Reuse: unlink ([prev.next] = cur.next), reset refcount, return payload.
    code.extend_from_slice(&[0x49, 0x8B, 0x53, 0x08]); // mov rdx, [r11 + 8] (cur.next)
    code.extend_from_slice(&[0x49, 0x89, 0x12]); // mov [r10], rdx
    code.extend_from_slice(&[0x49, 0xC7, 0x43, 0x08, 0x01, 0x00, 0x00, 0x00]); // mov qword [r11+8], 1
    code.extend_from_slice(&[0x49, 0x8D, 0x43, RC_HEADER_SIZE as u8]); // lea rax, [r11 + 16]
    code.push(0xC3); // ret
    // advance: prev = &cur.next; cur = cur.next; loop.
    patch_rel32(&mut code, advance_site);
    code.extend_from_slice(&[0x4D, 0x8D, 0x53, 0x08]); // lea r10, [r11 + 8]
    code.extend_from_slice(&[0x4D, 0x8B, 0x5B, 0x08]); // mov r11, [r11 + 8]
    emit_jmp_to(&mut code, scan);

    // bump: seed the bump pointer if zero, then carve `need` bytes.
    patch_rel32(&mut code, bump_site);
    code.extend_from_slice(&[0x48, 0x8B, 0x05]); // mov rax, [rip + heap_next]
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_NEXT_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.extend_from_slice(&[0x75, 0x07]); // jnz have (skip the 7-byte lea)
    code.extend_from_slice(&[0x48, 0x8D, 0x05]); // lea rax, [rip + heap_base]
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_BASE_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    // have: write the header (size = r8, refcount = 1).
    code.extend_from_slice(&[0x4C, 0x89, 0x00]); // mov [rax], r8
    code.extend_from_slice(&[0x48, 0xC7, 0x40, 0x08, 0x01, 0x00, 0x00, 0x00]); // mov qword [rax+8], 1
    // heap_next = (rax + need + 7) & ~7.
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax
    code.extend_from_slice(&[0x4C, 0x01, 0xC2]); // add rdx, r8
    code.extend_from_slice(&[0x48, 0x83, 0xC2, 0x07]); // add rdx, 7
    code.extend_from_slice(&[0x48, 0x83, 0xE2, 0xF8]); // and rdx, ~7
    code.extend_from_slice(&[0x48, 0x89, 0x15]); // mov [rip + heap_next], rdx
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_NEXT_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, RC_HEADER_SIZE as u8]); // add rax, 16 (payload)
    code.push(0xC3); // ret

    HelperFunction {
        name: HEAP_ALLOC_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_rc_free(payload ptr in rcx)`: push the block onto the LIFO free list.
/// The block base is `rcx - 16`; the "next" link is threaded through the freed
/// block's now-dead refcount slot (`[base + 8]`). A leaf (no calls); volatile only.
pub(crate) fn emit_rc_free_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    code.extend_from_slice(&[0x48, 0x8D, 0x41, 0xF0]); // lea rax, [rcx - 16] (block base)
    code.extend_from_slice(&[0x4C, 0x8D, 0x15]); // lea r10, [rip + free_head]
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_FREE_HEAD_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x49, 0x8B, 0x12]); // mov rdx, [r10] (old head)
    code.extend_from_slice(&[0x48, 0x89, 0x50, 0x08]); // mov [rax + 8], rdx (block.next = old head)
    code.extend_from_slice(&[0x49, 0x89, 0x02]); // mov [r10], rax (free_head = block)
    code.push(0xC3); // ret

    HelperFunction {
        name: RC_FREE_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_rc_dec(payload ptr in rcx)`: `dec qword [rcx - 8]`; if the refcount
/// reached zero, tail-call `__lullaby_rc_free` (which returns to our caller);
/// otherwise the block is still live and we return. `rcx` (the payload pointer) is
/// preserved by the `dec` and forwarded as `rc_free`'s argument.
pub(crate) fn emit_rc_dec_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    code.extend_from_slice(&[0x48, 0xFF, 0x49, 0xF8]); // dec qword [rcx - 8]
    code.extend_from_slice(&[0x75, 0x05]); // jnz keep (skip the 5-byte jmp)
    code.push(0xE9); // jmp __lullaby_rc_free (tail call)
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: RC_FREE_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    // keep:
    code.push(0xC3); // ret

    HelperFunction {
        name: RC_DEC_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_drop_string_array(block ptr in rcx)`: recursively drop a
/// `list<string>`-layout block — `rc_dec` each of its `len` shared string element
/// pointers, then `rc_dec` the block. Uses callee-saved `rbx` (block), `rdi` (len),
/// `rsi` (index) so they survive the internal `rc_dec` calls.
pub(crate) fn emit_drop_string_array_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Prologue: 3 callee-saved pushes (%16 -> 0), `sub rsp, 0x20` (shadow) keeps %16.
    code.push(0x53); // push rbx
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x20]); // sub rsp, 0x20

    code.extend_from_slice(&[0x48, 0x89, 0xCB]); // mov rbx, rcx (block)
    code.extend_from_slice(&[0x48, 0x8B, 0x7B, 0x00]); // mov rdi, [rbx + LIST_LEN_OFF] (len)
    code.extend_from_slice(&[0x31, 0xF6]); // xor esi, esi (i = 0)

    let loop_top = code.len();
    code.extend_from_slice(&[0x48, 0x39, 0xFE]); // cmp rsi, rdi
    code.extend_from_slice(&[0x0F, 0x83]); // jae done
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rc_dec(block[LIST_DATA_OFF + i*8]) — the i-th string element pointer.
    code.extend_from_slice(&[0x48, 0x8B, 0x8C, 0xF3]); // mov rcx, [rbx + rsi*8 + disp32]
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    emit_helper_call(&mut code, &mut relocations, RC_DEC_SYMBOL);
    code.extend_from_slice(&[0x48, 0xFF, 0xC6]); // inc rsi
    emit_jmp_to(&mut code, loop_top);

    // done: rc_dec the block itself.
    patch_rel32(&mut code, done_site);
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    emit_helper_call(&mut code, &mut relocations, RC_DEC_SYMBOL);

    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x20]); // add rsp, 0x20
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0x5B); // pop rbx
    code.push(0xC3); // ret

    HelperFunction {
        name: DROP_STRING_ARRAY_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// Emit `__lullaby_strlen_copy(src in rcx) -> len in rax`.
///
/// Measures the source (`.rdata`) length, bump-allocates `n + 1` bytes, copies
/// the string (including its terminator) into the heap, then scans the heap copy
/// for the terminator and returns that byte length. Uses the non-volatile
/// `rsi`/`rdi`/`rbx`, saved and restored around the body; keeps `rsp` 16-aligned
/// at the internal `call`.
pub(crate) fn emit_heap_strlen_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Prologue: preserve callee-saved regs we use, reserve aligned shadow space.
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.push(0x53); // push rbx
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x20]); // sub rsp, 32 (keeps rsp%16==0 at the call)

    // rsi = src
    code.extend_from_slice(&[0x48, 0x89, 0xCE]); // mov rsi, rcx

    // Measure length into rbx (scan .rdata bytes for NUL).
    code.extend_from_slice(&[0x48, 0x89, 0xF0]); // mov rax, rsi
    code.extend_from_slice(&[0x48, 0x31, 0xDB]); // xor rbx, rbx
    let measure = code.len();
    code.extend_from_slice(&[0x8A, 0x08]); // mov cl, [rax]
    code.extend_from_slice(&[0x84, 0xC9]); // test cl, cl
    // jz measured  (short forward; body below is inc rax; inc rbx; jmp = 3+3+2 = 8)
    code.extend_from_slice(&[0x74, 0x08]);
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
    code.extend_from_slice(&[0x48, 0xFF, 0xC3]); // inc rbx
    emit_short_jmp_back(&mut code, measure); // jmp measure

    // measured: allocate rbx + 1 bytes.
    code.extend_from_slice(&[0x48, 0x8D, 0x4B, 0x01]); // lea rcx, [rbx + 1]
    code.push(0xE8); // call __lullaby_alloc
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_ALLOC_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);

    // rdi = dest (heap pointer). Copy n+1 bytes rsi -> rdi.
    code.extend_from_slice(&[0x48, 0x89, 0xC7]); // mov rdi, rax
    code.push(0x50); // push rax (save dest base for the post-copy scan)
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]); // inc rcx (copy the terminator too)
    code.extend_from_slice(&[0xF3, 0xA4]); // rep movsb
    code.push(0x5A); // pop rdx (rdx = dest base)

    // Scan the heap copy for NUL, counting into rax (this read proves the copy).
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax
    let scan = code.len();
    code.extend_from_slice(&[0x8A, 0x0C, 0x02]); // mov cl, [rdx + rax]
    code.extend_from_slice(&[0x84, 0xC9]); // test cl, cl
    // jz done  (body below is inc rax; jmp = 3 + 2 = 5)
    code.extend_from_slice(&[0x74, 0x05]);
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
    emit_short_jmp_back(&mut code, scan); // jmp scan

    // done: epilogue.
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x20]); // add rsp, 32
    code.push(0x5B); // pop rbx
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret

    HelperFunction {
        name: HEAP_STRLEN_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// Emit a short `jmp rel8` back to an earlier `target` offset within `code`.
pub(crate) fn emit_short_jmp_back(code: &mut Vec<u8>, target: usize) {
    code.push(0xEB);
    let rel = target as i64 - (code.len() as i64 + 1);
    debug_assert!((-128..=127).contains(&rel), "short jmp out of range: {rel}");
    code.push(rel as i8 as u8);
}

// -- Growable-list runtime helpers (native) ----------------------------------
//
// Three `.text` helpers back the inline list op codegen. Each list value is a
// pointer to `[len i64][cap i64][cap * 8-byte slots]`. The helpers bump-allocate
// through `__lullaby_alloc` (no reclamation — grown/copied blocks orphan the old
// one) and copy whole 8-byte element words (elements are always scalar, so a flat
// word copy is an exact deep copy, mirroring the WASM backend's `list<T>`).

/// Emit a runtime loop that copies `count` 8-byte words from `[src_reg]` to
/// `[dst_reg]` (both pointing at each block's first element slot). Uses `rax` as
/// the loop counter and `r10`/`r11` as scratch, none of which the callers rely on
/// across the loop. `count_reg` holds the element count. Registers by encoding:
/// `src_reg`/`dst_reg`/`count_reg` are the 3-bit register numbers (rsi=6, rdi=7,
/// rbx=3, etc.). This helper assumes src=rsi, dst=rdi, count=rbx for compact
/// encodings, matching how the copy/grow helpers set them up.
pub(crate) fn emit_list_word_copy_loop_rsi_rdi_rbx(code: &mut Vec<u8>) {
    // xor rax, rax   (i = 0)
    code.extend_from_slice(&[0x48, 0x31, 0xC0]);
    let loop_top = code.len();
    // cmp rax, rbx
    code.extend_from_slice(&[0x48, 0x39, 0xD8]);
    // jge done (rel32, patched)
    code.extend_from_slice(&[0x0F, 0x8D]);
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // r10 = [rsi + rax*8 + LIST_DATA_OFF]   (mov r10, [rsi + rax*8 + disp32])
    code.extend_from_slice(&[0x4C, 0x8B, 0x94, 0xC6]);
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    // [rdi + rax*8 + LIST_DATA_OFF] = r10   (mov [rdi + rax*8 + disp32], r10)
    code.extend_from_slice(&[0x4C, 0x89, 0x94, 0xC7]);
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    // inc rax
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]);
    // jmp loop_top (rel32)
    emit_jmp_to(code, loop_top);
    // done:
    patch_rel32(code, done_site);
}

/// Emit `sub rsp, 40` / `add rsp, 40` framing that keeps `rsp` 16-byte aligned at
/// an internal `call` (the return address makes 8, `sub rsp, 40` restores %16==0,
/// and reserves the 32-byte Win64 shadow space).
pub(crate) fn emit_helper_shadow_prologue(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 40
}
pub(crate) fn emit_helper_shadow_epilogue(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]); // add rsp, 40
}

/// `__lullaby_list_new() -> ptr in rax`: allocate a fresh empty list block with
/// `len = 0`, `cap = LIST_INITIAL_CAP`.
pub(crate) fn emit_list_new_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();
    emit_helper_shadow_prologue(&mut code);
    // rcx = LIST_DATA_OFF + LIST_INITIAL_CAP * 8  (block byte size)
    let size = LIST_DATA_OFF as i64 + LIST_INITIAL_CAP * LIST_SLOT_SIZE as i64;
    // mov rcx, imm32 (size is small)  -> use mov ecx, imm32 (B9) zero-extends.
    code.push(0xB9);
    code.extend_from_slice(&(size as i32).to_le_bytes());
    // call __lullaby_alloc
    code.push(0xE8);
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_ALLOC_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    // mov qword [rax + LIST_LEN_OFF], 0
    code.extend_from_slice(&[0x48, 0xC7, 0x80]);
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&0i32.to_le_bytes());
    // mov qword [rax + LIST_CAP_OFF], LIST_INITIAL_CAP
    code.extend_from_slice(&[0x48, 0xC7, 0x80]);
    code.extend_from_slice(&LIST_CAP_OFF.to_le_bytes());
    code.extend_from_slice(&(LIST_INITIAL_CAP as i32).to_le_bytes());
    emit_helper_shadow_epilogue(&mut code);
    code.push(0xC3); // ret (rax = new block)
    HelperFunction {
        name: LIST_NEW_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_list_copy(rcx = src) -> rax = fresh copy`: allocate a block with the
/// source's `cap`, copy the `len`/`cap` headers and the `len` live element words.
pub(crate) fn emit_list_copy_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();
    // Preserve non-volatiles used: rsi (src), rdi (dst), rbx (len/cap scratch).
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.push(0x53); // push rbx
    emit_helper_shadow_prologue(&mut code); // sub rsp, 40 (keeps %16 at the call)
    // rsi = src
    code.extend_from_slice(&[0x48, 0x89, 0xCE]); // mov rsi, rcx
    // Allocation size = LIST_DATA_OFF + cap * 8. cap = [rsi + LIST_CAP_OFF].
    // rcx = [rsi + LIST_CAP_OFF]
    code.extend_from_slice(&[0x48, 0x8B, 0x8E]);
    code.extend_from_slice(&LIST_CAP_OFF.to_le_bytes());
    // rcx = rcx * 8 : shl rcx, 3
    code.extend_from_slice(&[0x48, 0xC1, 0xE1, 0x03]);
    // rcx = rcx + LIST_DATA_OFF : add rcx, imm32
    code.extend_from_slice(&[0x48, 0x81, 0xC1]);
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    // call __lullaby_alloc -> rax = dst
    code.push(0xE8);
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_ALLOC_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rdi = dst
    code.extend_from_slice(&[0x48, 0x89, 0xC7]); // mov rdi, rax
    // Copy len + cap headers: two 8-byte words at offsets 0 and 8.
    // r10 = [rsi + 0]; [rdi + 0] = r10  (len)
    code.extend_from_slice(&[0x4C, 0x8B, 0x96]);
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0x97]);
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    // r10 = [rsi + 8]; [rdi + 8] = r10  (cap)
    code.extend_from_slice(&[0x4C, 0x8B, 0x96]);
    code.extend_from_slice(&LIST_CAP_OFF.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0x97]);
    code.extend_from_slice(&LIST_CAP_OFF.to_le_bytes());
    // rbx = len (element count to copy)
    code.extend_from_slice(&[0x48, 0x8B, 0x9E]);
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    // Copy `rbx` element words from rsi to rdi.
    emit_list_word_copy_loop_rsi_rdi_rbx(&mut code);
    // rax = dst (return value)
    code.extend_from_slice(&[0x48, 0x89, 0xF8]); // mov rax, rdi
    emit_helper_shadow_epilogue(&mut code);
    code.push(0x5B); // pop rbx
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret
    HelperFunction {
        name: LIST_COPY_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_list_grow(rcx = list) -> rax = list with room for one more element`:
/// when `len < cap` the list is returned unchanged; otherwise a block with
/// `new_cap = (cap == 0 ? LIST_INITIAL_CAP : cap * 2)` is allocated, the `len`
/// header and the `len` live elements are copied, the new `cap` is written, and
/// the fresh block is returned (the old block is orphaned).
pub(crate) fn emit_list_grow_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.push(0x53); // push rbx
    emit_helper_shadow_prologue(&mut code); // sub rsp, 40
    // rsi = list
    code.extend_from_slice(&[0x48, 0x89, 0xCE]); // mov rsi, rcx
    // rax = len = [rsi + LEN]; rdx = cap = [rsi + CAP].
    code.extend_from_slice(&[0x48, 0x8B, 0x86]);
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8B, 0x96]);
    code.extend_from_slice(&LIST_CAP_OFF.to_le_bytes());
    // if len < cap: return the list unchanged.
    // cmp rax, rdx ; jl return_same (rel32)
    code.extend_from_slice(&[0x48, 0x39, 0xD0]); // cmp rax, rdx
    code.extend_from_slice(&[0x0F, 0x8C]); // jl rel32
    let return_same_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rbx = new_cap = (cap == 0 ? LIST_INITIAL_CAP : cap * 2).
    // rbx = cap ; test rbx, rbx ; jnz double ; rbx = LIST_INITIAL_CAP ; jmp sized
    code.extend_from_slice(&[0x48, 0x89, 0xD3]); // mov rbx, rdx (cap)
    code.extend_from_slice(&[0x48, 0x85, 0xDB]); // test rbx, rbx
    code.extend_from_slice(&[0x0F, 0x85]); // jnz double (rel32)
    let double_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rbx = LIST_INITIAL_CAP : mov ebx, imm32
    code.push(0xBB);
    code.extend_from_slice(&(LIST_INITIAL_CAP as i32).to_le_bytes());
    code.extend_from_slice(&[0xE9]); // jmp sized (rel32)
    let sized_jmp_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // double: rbx = cap * 2  (shl rbx, 1)
    patch_rel32(&mut code, double_site);
    code.extend_from_slice(&[0x48, 0xD1, 0xE3]); // shl rbx, 1
    // sized: allocate LIST_DATA_OFF + new_cap * 8.
    patch_rel32(&mut code, sized_jmp_site);
    // rcx = rbx * 8 + LIST_DATA_OFF
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0xC1, 0xE1, 0x03]); // shl rcx, 3
    code.extend_from_slice(&[0x48, 0x81, 0xC1]); // add rcx, imm32
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    // call __lullaby_alloc -> rax = dst
    code.push(0xE8);
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_ALLOC_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rdi = dst
    code.extend_from_slice(&[0x48, 0x89, 0xC7]); // mov rdi, rax
    // dst.len = src.len : r10 = [rsi + LEN]; [rdi + LEN] = r10.
    code.extend_from_slice(&[0x4C, 0x8B, 0x96]);
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0x97]);
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    // dst.cap = new_cap (rbx) : mov [rdi + CAP], rbx.
    code.extend_from_slice(&[0x48, 0x89, 0x9F]);
    code.extend_from_slice(&LIST_CAP_OFF.to_le_bytes());
    // Copy `len` element words. rbx = len (reuse rbx as the count now).
    code.extend_from_slice(&[0x48, 0x8B, 0x9E]);
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    emit_list_word_copy_loop_rsi_rdi_rbx(&mut code);
    // rax = dst (the grown block).
    code.extend_from_slice(&[0x48, 0x89, 0xF8]); // mov rax, rdi
    code.extend_from_slice(&[0xE9]); // jmp epilogue (rel32)
    let epi_jmp_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // return_same: rax = the original list (rsi).
    patch_rel32(&mut code, return_same_site);
    code.extend_from_slice(&[0x48, 0x89, 0xF0]); // mov rax, rsi
    // epilogue:
    patch_rel32(&mut code, epi_jmp_site);
    emit_helper_shadow_epilogue(&mut code);
    code.push(0x5B); // pop rbx
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret
    HelperFunction {
        name: LIST_GROW_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_struct_copy(rcx = src field-0 ptr) -> rax = fresh field-0 ptr`:
/// deep-copy a heap struct. Reads the `[rcx - STRUCT_HEADER_SIZE]` word count,
/// allocates `STRUCT_HEADER_SIZE + nwords * 8`, copies the header word and every
/// field word (a flat 8-byte word copy — heap-struct fields are scalars or shared
/// immutable strings at the one-level nesting bound, so the flat copy is an exact
/// deep copy), and returns the fresh block's field-0 pointer (`alloc_base +
/// STRUCT_HEADER_SIZE`). The independent block gives the struct value semantics:
/// mutating one heap-struct copy is never observable through another.
pub(crate) fn emit_struct_copy_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();
    // Preserve non-volatiles rsi (src base), rdi (dst base), rbx (nwords).
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.push(0x53); // push rbx
    emit_helper_shadow_prologue(&mut code); // sub rsp, 40 (keeps %16 at the call)
    // rsi = src block base = rcx - STRUCT_HEADER_SIZE (points at the header word).
    code.extend_from_slice(&[0x48, 0x8D, 0x71]); // lea rsi, [rcx + disp8]
    code.push((-STRUCT_HEADER_SIZE) as i8 as u8);
    // rbx = nwords = [rsi]  (the header word)
    code.extend_from_slice(&[0x48, 0x8B, 0x1E]); // mov rbx, [rsi]
    // alloc size = STRUCT_HEADER_SIZE + nwords * 8 -> rcx
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0xC1, 0xE1, 0x03]); // shl rcx, 3
    code.extend_from_slice(&[0x48, 0x81, 0xC1]); // add rcx, imm32
    code.extend_from_slice(&STRUCT_HEADER_SIZE.to_le_bytes());
    // call __lullaby_alloc -> rax = dst base
    code.push(0xE8);
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_ALLOC_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rdi = dst base
    code.extend_from_slice(&[0x48, 0x89, 0xC7]); // mov rdi, rax
    // Copy header + nwords fields: total (nwords + 1) words from rsi to rdi.
    // rbx currently holds nwords; count = nwords + 1.
    code.extend_from_slice(&[0x48, 0xFF, 0xC3]); // inc rbx (count = nwords + 1)
    // for i in 0..count: [rdi + i*8] = [rsi + i*8].  Use rax as the index.
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax  (i = 0)
    let loop_top = code.len();
    code.extend_from_slice(&[0x48, 0x39, 0xD8]); // cmp rax, rbx
    code.extend_from_slice(&[0x0F, 0x8D]); // jge done (rel32)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // r10 = [rsi + rax*8]
    code.extend_from_slice(&[0x4C, 0x8B, 0x14, 0xC6]); // mov r10, [rsi + rax*8]
    // [rdi + rax*8] = r10
    code.extend_from_slice(&[0x4C, 0x89, 0x14, 0xC7]); // mov [rdi + rax*8], r10
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
    emit_jmp_to(&mut code, loop_top);
    patch_rel32(&mut code, done_site);
    // rax = dst field-0 pointer = rdi + STRUCT_HEADER_SIZE
    code.extend_from_slice(&[0x48, 0x8D, 0x47]); // lea rax, [rdi + disp8]
    code.push(STRUCT_HEADER_SIZE as i8 as u8);
    emit_helper_shadow_epilogue(&mut code);
    code.push(0x5B); // pop rbx
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret
    HelperFunction {
        name: STRUCT_COPY_SYMBOL.to_string(),
        code,
        relocations,
    }
}

// -- Growable-map runtime helpers (native) -----------------------------------
//
// Four `.text` helpers back the inline map op codegen. Each map value is a
// pointer to `[len i64][cap i64][cap * 16-byte entries]` (entry = key word +
// value word). The helpers bump-allocate through `__lullaby_alloc` (no
// reclamation) and copy whole 8-byte words. Because `MAP_DATA_OFF == LIST_DATA_OFF
// == 16`, the shared `emit_list_word_copy_loop_rsi_rdi_rbx` copies map entry
// words too — a map with `len` entries copies `2 * len` words.

/// `__lullaby_map_new() -> ptr in rax`: allocate a fresh empty map block with
/// `len = 0`, `cap = MAP_INITIAL_CAP`.
pub(crate) fn emit_map_new_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();
    emit_helper_shadow_prologue(&mut code);
    // rcx = MAP_DATA_OFF + MAP_INITIAL_CAP * MAP_ENTRY_SIZE  (block byte size)
    let size = MAP_DATA_OFF as i64 + MAP_INITIAL_CAP * MAP_ENTRY_SIZE as i64;
    code.push(0xB9); // mov ecx, imm32 (zero-extends; size is small)
    code.extend_from_slice(&(size as i32).to_le_bytes());
    // call __lullaby_alloc
    code.push(0xE8);
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_ALLOC_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    // mov qword [rax + MAP_LEN_OFF], 0
    code.extend_from_slice(&[0x48, 0xC7, 0x80]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&0i32.to_le_bytes());
    // mov qword [rax + MAP_CAP_OFF], MAP_INITIAL_CAP
    code.extend_from_slice(&[0x48, 0xC7, 0x80]);
    code.extend_from_slice(&MAP_CAP_OFF.to_le_bytes());
    code.extend_from_slice(&(MAP_INITIAL_CAP as i32).to_le_bytes());
    emit_helper_shadow_epilogue(&mut code);
    code.push(0xC3); // ret (rax = new block)
    HelperFunction {
        name: MAP_NEW_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_map_copy(rcx = src) -> rax = fresh copy`: allocate a block with the
/// source's `cap`, copy the `len`/`cap` headers and the `2 * len` live entry
/// words.
pub(crate) fn emit_map_copy_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.push(0x53); // push rbx
    emit_helper_shadow_prologue(&mut code); // sub rsp, 40
    // rsi = src
    code.extend_from_slice(&[0x48, 0x89, 0xCE]); // mov rsi, rcx
    // Allocation size = MAP_DATA_OFF + cap * MAP_ENTRY_SIZE. cap = [rsi + CAP].
    // rcx = [rsi + MAP_CAP_OFF]
    code.extend_from_slice(&[0x48, 0x8B, 0x8E]);
    code.extend_from_slice(&MAP_CAP_OFF.to_le_bytes());
    // rcx = rcx * MAP_ENTRY_SIZE (16) : shl rcx, 4
    code.extend_from_slice(&[0x48, 0xC1, 0xE1, 0x04]);
    // rcx = rcx + MAP_DATA_OFF : add rcx, imm32
    code.extend_from_slice(&[0x48, 0x81, 0xC1]);
    code.extend_from_slice(&MAP_DATA_OFF.to_le_bytes());
    // call __lullaby_alloc -> rax = dst
    code.push(0xE8);
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_ALLOC_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rdi = dst
    code.extend_from_slice(&[0x48, 0x89, 0xC7]); // mov rdi, rax
    // Copy len + cap headers (offsets 0 and 8).
    // r10 = [rsi + LEN]; [rdi + LEN] = r10
    code.extend_from_slice(&[0x4C, 0x8B, 0x96]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0x97]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // r10 = [rsi + CAP]; [rdi + CAP] = r10
    code.extend_from_slice(&[0x4C, 0x8B, 0x96]);
    code.extend_from_slice(&MAP_CAP_OFF.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0x97]);
    code.extend_from_slice(&MAP_CAP_OFF.to_le_bytes());
    // rbx = 2 * len (entry word count to copy). rbx = [rsi + LEN]; shl rbx, 1.
    code.extend_from_slice(&[0x48, 0x8B, 0x9E]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xD1, 0xE3]); // shl rbx, 1
    // Copy `rbx` words from rsi to rdi (data offset 16 == MAP_DATA_OFF).
    emit_list_word_copy_loop_rsi_rdi_rbx(&mut code);
    // rax = dst (return value)
    code.extend_from_slice(&[0x48, 0x89, 0xF8]); // mov rax, rdi
    emit_helper_shadow_epilogue(&mut code);
    code.push(0x5B); // pop rbx
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret
    HelperFunction {
        name: MAP_COPY_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_map_grow(rcx = map) -> rax = map with room for one more entry`:
/// when `len < cap` the map is returned unchanged; otherwise a block with
/// `new_cap = (cap == 0 ? MAP_INITIAL_CAP : cap * 2)` is allocated, the `len`
/// header and the `2 * len` live entry words are copied, the new `cap` is
/// written, and the fresh block is returned (the old block is orphaned).
pub(crate) fn emit_map_grow_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.push(0x53); // push rbx
    emit_helper_shadow_prologue(&mut code); // sub rsp, 40
    // rsi = map
    code.extend_from_slice(&[0x48, 0x89, 0xCE]); // mov rsi, rcx
    // rax = len = [rsi + LEN]; rdx = cap = [rsi + CAP].
    code.extend_from_slice(&[0x48, 0x8B, 0x86]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8B, 0x96]);
    code.extend_from_slice(&MAP_CAP_OFF.to_le_bytes());
    // if len < cap: return the map unchanged.
    code.extend_from_slice(&[0x48, 0x39, 0xD0]); // cmp rax, rdx
    code.extend_from_slice(&[0x0F, 0x8C]); // jl return_same (rel32)
    let return_same_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rbx = new_cap = (cap == 0 ? MAP_INITIAL_CAP : cap * 2).
    code.extend_from_slice(&[0x48, 0x89, 0xD3]); // mov rbx, rdx (cap)
    code.extend_from_slice(&[0x48, 0x85, 0xDB]); // test rbx, rbx
    code.extend_from_slice(&[0x0F, 0x85]); // jnz double (rel32)
    let double_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.push(0xBB); // mov ebx, imm32 (MAP_INITIAL_CAP)
    code.extend_from_slice(&(MAP_INITIAL_CAP as i32).to_le_bytes());
    code.extend_from_slice(&[0xE9]); // jmp sized (rel32)
    let sized_jmp_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // double: rbx = cap * 2 (shl rbx, 1)
    patch_rel32(&mut code, double_site);
    code.extend_from_slice(&[0x48, 0xD1, 0xE3]); // shl rbx, 1
    // sized: allocate MAP_DATA_OFF + new_cap * MAP_ENTRY_SIZE.
    patch_rel32(&mut code, sized_jmp_site);
    // rcx = rbx * MAP_ENTRY_SIZE (16) + MAP_DATA_OFF
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0xC1, 0xE1, 0x04]); // shl rcx, 4
    code.extend_from_slice(&[0x48, 0x81, 0xC1]); // add rcx, imm32
    code.extend_from_slice(&MAP_DATA_OFF.to_le_bytes());
    // call __lullaby_alloc -> rax = dst
    code.push(0xE8);
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_ALLOC_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rdi = dst
    code.extend_from_slice(&[0x48, 0x89, 0xC7]); // mov rdi, rax
    // dst.len = src.len : r10 = [rsi + LEN]; [rdi + LEN] = r10.
    code.extend_from_slice(&[0x4C, 0x8B, 0x96]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0x97]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // dst.cap = new_cap (rbx) : mov [rdi + CAP], rbx.
    code.extend_from_slice(&[0x48, 0x89, 0x9F]);
    code.extend_from_slice(&MAP_CAP_OFF.to_le_bytes());
    // Copy 2 * len entry words. rbx = [rsi + LEN]; shl rbx, 1.
    code.extend_from_slice(&[0x48, 0x8B, 0x9E]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xD1, 0xE3]); // shl rbx, 1
    emit_list_word_copy_loop_rsi_rdi_rbx(&mut code);
    // rax = dst (the grown block).
    code.extend_from_slice(&[0x48, 0x89, 0xF8]); // mov rax, rdi
    code.extend_from_slice(&[0xE9]); // jmp epilogue (rel32)
    let epi_jmp_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // return_same: rax = the original map (rsi).
    patch_rel32(&mut code, return_same_site);
    code.extend_from_slice(&[0x48, 0x89, 0xF0]); // mov rax, rsi
    // epilogue:
    patch_rel32(&mut code, epi_jmp_site);
    emit_helper_shadow_epilogue(&mut code);
    code.push(0x5B); // pop rbx
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret
    HelperFunction {
        name: MAP_GROW_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_map_find(rcx = map, rdx = key) -> rax = index-or-len`: linear-scan
/// the map's entries front-to-back for the FIRST entry whose key word equals
/// `rdx`, returning its index; if none matches, return the map's `len` (the
/// "found index else len" convention). Key equality is an exact 8-byte word
/// compare (keys are integer-cell scalars), matching the interpreters' value
/// equality. No allocation, so no shadow space / callee-saved registers needed:
/// uses only volatile `rax`/`r10`/`r11`.
pub(crate) fn emit_map_find_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let relocations: Vec<CodeRelocation> = Vec::new();
    // r10 = len = [rcx + MAP_LEN_OFF]
    code.extend_from_slice(&[0x4C, 0x8B, 0x91]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // rax = 0 (i = 0; also the running index)
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax
    let loop_top = code.len();
    // cmp rax, r10  ; jge not_found (rel32)
    code.extend_from_slice(&[0x4C, 0x39, 0xD0]); // cmp rax, r10
    code.extend_from_slice(&[0x0F, 0x8D]); // jge not_found
    let not_found_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // entry key addr: r11 = rcx + MAP_DATA_OFF + rax*16. rax*16 = rax<<4 into r11.
    // mov r11, rax ; shl r11, 4
    code.extend_from_slice(&[0x49, 0x89, 0xC3]); // mov r11, rax
    code.extend_from_slice(&[0x49, 0xC1, 0xE3, 0x04]); // shl r11, 4
    // r11 = [rcx + r11 + MAP_DATA_OFF]  (load the key word)
    // mov r11, [rcx + r11 + disp32]  (REX.WRXB: r11 dest+base... base rcx no B;
    // index r11 sets X; dest r11 sets R) -> REX = W+R+X = 0x4E
    code.extend_from_slice(&[0x4E, 0x8B, 0x9C, 0x19]); // mov r11, [rcx + r11 + disp32]
    code.extend_from_slice(&MAP_DATA_OFF.to_le_bytes());
    // if r11 == rdx -> found (return rax). cmp r11, rdx ; je found.
    // The `je` skips `inc rax` (3 bytes) + `jmp loop_top` (5 bytes) = 8 bytes,
    // landing on the `ret` at `found:`.
    code.extend_from_slice(&[0x49, 0x39, 0xD3]); // cmp r11, rdx
    code.extend_from_slice(&[0x74, 0x08]); // je +8 -> found: ret
    // Not equal: rax += 1 ; jmp loop_top.
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
    emit_jmp_to(&mut code, loop_top); // jmp loop_top (rel32, 5 bytes)
    // found: (je target) — rax already holds the matching index.
    code.push(0xC3); // ret
    // not_found: rax = len (r10).
    patch_rel32(&mut code, not_found_site);
    code.extend_from_slice(&[0x4C, 0x89, 0xD0]); // mov rax, r10
    code.push(0xC3); // ret
    HelperFunction {
        name: MAP_FIND_SYMBOL.to_string(),
        code,
        relocations,
    }
}

// -- String runtime helpers (native) -----------------------------------------
//
// Each helper builds a heap `string` record `[char_len i64][byte_len i64][utf8]`
// via `__lullaby_alloc`. They preserve the non-volatile registers they use
// (`rsi`/`rdi`/`rbx`) and keep `rsp` 16-byte aligned at the internal `call`
// (three 8-byte pushes + `sub rsp, 8` restores alignment, the return address on
// entry making the fourth 8). The bump allocator never reclaims.

/// Emit `call __lullaby_alloc` (a rel32 relocation) inside a helper body.
pub(crate) fn emit_helper_call_alloc(code: &mut Vec<u8>, relocations: &mut Vec<CodeRelocation>) {
    code.push(0xE8);
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_ALLOC_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
}

/// `__lullaby_str_lit(rcx = NUL-terminated .rdata ptr) -> rax = string record`.
///
/// Scans the source for its byte length and its UTF-8 char count (a byte is a
/// char boundary when `(b & 0xC0) != 0x80`), allocates `STR_DATA_OFF + byte_len`,
/// writes the two headers, and copies the bytes.
pub(crate) fn emit_str_lit_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Prologue: preserve rsi/rdi/rbx; `sub rsp, 8` restores 16-byte alignment.
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.push(0x53); // push rbx
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]); // sub rsp, 8

    // rsi = src.
    code.extend_from_slice(&[0x48, 0x89, 0xCE]); // mov rsi, rcx

    // Scan: rbx = byte_len, rdi = char_len. rax walks the bytes.
    code.extend_from_slice(&[0x48, 0x89, 0xF0]); // mov rax, rsi
    code.extend_from_slice(&[0x48, 0x31, 0xDB]); // xor rbx, rbx (byte_len)
    code.extend_from_slice(&[0x48, 0x31, 0xFF]); // xor rdi, rdi (char_len)
    let scan = code.len();
    code.extend_from_slice(&[0x8A, 0x08]); // mov cl, [rax]
    code.extend_from_slice(&[0x84, 0xC9]); // test cl, cl
    // jz scan_done (rel32, patched).
    code.extend_from_slice(&[0x0F, 0x84]);
    let scan_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // Is this byte a char boundary? (cl & 0xC0) != 0x80  => inc char_len.
    code.extend_from_slice(&[0x88, 0xCA]); // mov dl, cl
    code.extend_from_slice(&[0x80, 0xE2, 0xC0]); // and dl, 0xC0
    code.extend_from_slice(&[0x80, 0xFA, 0x80]); // cmp dl, 0x80
    // je skip_inc (rel8): skip the `inc rdi` (3 bytes) if a continuation byte.
    code.extend_from_slice(&[0x74, 0x03]);
    code.extend_from_slice(&[0x48, 0xFF, 0xC7]); // inc rdi (char_len)
    // skip_inc:
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
    code.extend_from_slice(&[0x48, 0xFF, 0xC3]); // inc rbx (byte_len)
    emit_jmp_to(&mut code, scan); // jmp scan (rel32)
    // scan_done:
    patch_rel32(&mut code, scan_done_site);

    // Allocate STR_DATA_OFF + byte_len bytes. rcx = rbx + STR_DATA_OFF.
    code.extend_from_slice(&[0x48, 0x8D, 0x8B]); // lea rcx, [rbx + disp32]
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    emit_helper_call_alloc(&mut code, &mut relocations); // rax = dst

    // Write headers: [rax + CHAR_LEN] = rdi (char_len); [rax + BYTE_LEN] = rbx.
    code.extend_from_slice(&[0x48, 0x89, 0xB8]); // mov [rax + disp32], rdi
    code.extend_from_slice(&STR_CHAR_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0x98]); // mov [rax + disp32], rbx
    code.extend_from_slice(&STR_BYTE_LEN_OFF.to_le_bytes());

    // Copy byte_len bytes from rsi (src) to rax + STR_DATA_OFF.
    code.push(0x50); // push rax (save record base for return)
    // rdi = rax + STR_DATA_OFF (dest).
    code.extend_from_slice(&[0x48, 0x8D, 0xB8]); // lea rdi, [rax + disp32]
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx (count)
    // rsi already = src.
    code.extend_from_slice(&[0xF3, 0xA4]); // rep movsb
    code.push(0x58); // pop rax (record base)

    // Epilogue.
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]); // add rsp, 8
    code.push(0x5B); // pop rbx
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_LIT_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_to_cstr(rcx = string record ptr) -> rax = NUL-terminated C buffer`.
///
/// Reads `byte_len` from the record, bump-allocates `byte_len + 1` bytes, copies
/// the record's UTF-8 bytes, and writes a trailing NUL. The returned buffer is the
/// `const char*` a C function borrows for the duration of an FFI call. Preserves
/// the non-volatile `rsi`/`rdi`/`rbx` it uses; it only calls the leaf bump
/// allocator, so it tolerates any incoming `rsp` alignment.
pub(crate) fn emit_to_cstr_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Prologue: preserve rsi/rdi/rbx; `sub rsp, 8` keeps the frame balanced.
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.push(0x53); // push rbx
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]); // sub rsp, 8

    // rsi = record ptr; rbx = byte_len = [rsi + STR_BYTE_LEN_OFF].
    code.extend_from_slice(&[0x48, 0x89, 0xCE]); // mov rsi, rcx
    code.extend_from_slice(&[0x48, 0x8B, 0x9E]); // mov rbx, [rsi + disp32]
    code.extend_from_slice(&STR_BYTE_LEN_OFF.to_le_bytes());

    // Allocate byte_len + 1 bytes (the extra byte is the NUL terminator).
    code.extend_from_slice(&[0x48, 0x8D, 0x4B, 0x01]); // lea rcx, [rbx + 1]
    emit_helper_call_alloc(&mut code, &mut relocations); // rax = dst

    // Copy byte_len bytes from record+DATA to the buffer; save the base to return.
    code.push(0x50); // push rax (buffer base, returned)
    code.extend_from_slice(&[0x48, 0x89, 0xC7]); // mov rdi, rax (dest)
    code.extend_from_slice(&[0x48, 0x81, 0xC6]); // add rsi, imm32 (rsi = record + DATA)
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx (count = byte_len)
    code.extend_from_slice(&[0xF3, 0xA4]); // rep movsb (rdi ends at dst + byte_len)
    code.extend_from_slice(&[0xC6, 0x07, 0x00]); // mov byte [rdi], 0 (NUL terminator)
    code.push(0x58); // pop rax (buffer base = return value)

    // Epilogue.
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]); // add rsp, 8
    code.push(0x5B); // pop rbx
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret

    HelperFunction {
        name: TO_CSTR_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_str_concat(rcx = a, rdx = b) -> rax = fresh record`.
///
/// Allocates `STR_DATA_OFF + byte_a + byte_b`, sums the char/byte headers, and
/// byte-copies each operand's UTF-8 range. Mirrors the WASM backend's concat.
pub(crate) fn emit_str_concat_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Preserve non-volatiles; keep operand pointers/headers across the alloc call.
    //   rsi = a, r15 = b, rbx = byte_a, rbp = byte_b, r12 = char_a, r13 = char_b,
    //   r14 = dst (record base). 8 pushes (64 bytes) + return addr (8) = 72; a
    //   `sub rsp, 8` restores 16-byte alignment at the internal `call`.
    code.push(0x56); // push rsi
    code.push(0x53); // push rbx
    code.push(0x55); // push rbp
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x41, 0x55]); // push r13
    code.extend_from_slice(&[0x41, 0x56]); // push r14
    code.extend_from_slice(&[0x41, 0x57]); // push r15
    code.push(0x57); // push rdi (8th push; keeps count even and rsp aligned)
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]); // sub rsp, 8

    code.extend_from_slice(&[0x48, 0x89, 0xCE]); // mov rsi, rcx (a)
    code.extend_from_slice(&[0x49, 0x89, 0xD7]); // mov r15, rdx (b)

    // Load headers.
    code.extend_from_slice(&[0x48, 0x8B, 0x9E]); // rbx = [rsi + BYTE_LEN] (byte_a)
    code.extend_from_slice(&STR_BYTE_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x49, 0x8B, 0xAF]); // rbp = [r15 + BYTE_LEN] (byte_b)
    code.extend_from_slice(&STR_BYTE_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x8B, 0xA6]); // r12 = [rsi + CHAR_LEN] (char_a)
    code.extend_from_slice(&STR_CHAR_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x4D, 0x8B, 0xAF]); // r13 = [r15 + CHAR_LEN] (char_b)
    code.extend_from_slice(&STR_CHAR_LEN_OFF.to_le_bytes());

    // Allocate STR_DATA_OFF + byte_a + byte_b. rcx = rbx + rbp + STR_DATA_OFF.
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0x01, 0xE9]); // add rcx, rbp
    code.extend_from_slice(&[0x48, 0x81, 0xC1]); // add rcx, imm32
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    emit_helper_call_alloc(&mut code, &mut relocations); // rax = dst
    code.extend_from_slice(&[0x49, 0x89, 0xC6]); // mov r14, rax (save record base)

    // Headers: char_len = r12 + r13; byte_len = rbx + rbp.
    code.extend_from_slice(&[0x4C, 0x89, 0xE1]); // mov rcx, r12
    code.extend_from_slice(&[0x4C, 0x01, 0xE9]); // add rcx, r13
    code.extend_from_slice(&[0x49, 0x89, 0x8E]); // mov [r14 + CHAR_LEN], rcx
    code.extend_from_slice(&STR_CHAR_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0x01, 0xE9]); // add rcx, rbp
    code.extend_from_slice(&[0x49, 0x89, 0x8E]); // mov [r14 + BYTE_LEN], rcx
    code.extend_from_slice(&STR_BYTE_LEN_OFF.to_le_bytes());

    // Copy a's bytes: rdi = r14 + DATA (dest), rsi = a + DATA (src), rcx = byte_a.
    code.extend_from_slice(&[0x49, 0x8D, 0xBE]); // lea rdi, [r14 + disp32]
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x81, 0xC6]); // add rsi, imm32  (rsi = a + DATA)
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx (byte_a)
    code.extend_from_slice(&[0xF3, 0xA4]); // rep movsb  (rdi advanced by byte_a)

    // Copy b's bytes: rsi = b + DATA (rdi already at the append position),
    // rcx = byte_b (rbp).
    code.extend_from_slice(&[0x4C, 0x89, 0xFE]); // mov rsi, r15 (b)
    code.extend_from_slice(&[0x48, 0x81, 0xC6]); // add rsi, imm32 (b + DATA)
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp (byte_b)
    code.extend_from_slice(&[0xF3, 0xA4]); // rep movsb

    // rax = r14 (record base) — return value.
    code.extend_from_slice(&[0x4C, 0x89, 0xF0]); // mov rax, r14

    // Epilogue (reverse of prologue).
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]); // add rsp, 8
    code.push(0x5F); // pop rdi
    code.extend_from_slice(&[0x41, 0x5F]); // pop r15
    code.extend_from_slice(&[0x41, 0x5E]); // pop r14
    code.extend_from_slice(&[0x41, 0x5D]); // pop r13
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5D); // pop rbp
    code.push(0x5B); // pop rbx
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_CONCAT_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_str_concat_own(rcx = left, rdx = right, r8 = ownership mask) -> rax`.
///
/// Concatenates (via `__lullaby_str_concat`), then `rc_dec`s each operand the
/// compile-time mask marks as a uniquely-owned fresh temporary (bit 0 = left,
/// bit 1 = right) — reclaiming intermediate string temporaries. Preserves the two
/// operands and the mask across the concat call (callee-saved `rbx`/`rsi`/`rdi`)
/// and the result across the `rc_dec` calls (`r12`).
pub(crate) fn emit_str_concat_own_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Prologue: 4 callee-saved pushes (%16 -> 8), `sub rsp, 0x28` -> %16 == 0 with
    // 32 shadow bytes for the internal calls.
    code.push(0x53); // push rbx
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 0x28

    code.extend_from_slice(&[0x48, 0x89, 0xCB]); // mov rbx, rcx (left)
    code.extend_from_slice(&[0x48, 0x89, 0xD6]); // mov rsi, rdx (right)
    code.extend_from_slice(&[0x44, 0x89, 0xC7]); // mov edi, r8d (mask)

    // result = concat(left, right).
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0x89, 0xF2]); // mov rdx, rsi
    emit_helper_call(&mut code, &mut relocations, STR_CONCAT_SYMBOL); // rax = result
    code.extend_from_slice(&[0x49, 0x89, 0xC4]); // mov r12, rax (result)

    // if mask & 1: rc_dec(left).
    code.extend_from_slice(&[0xF7, 0xC7, 0x01, 0x00, 0x00, 0x00]); // test edi, 1
    code.extend_from_slice(&[0x74, 0x08]); // jz +8 (skip mov+call)
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx (left)
    emit_helper_call(&mut code, &mut relocations, RC_DEC_SYMBOL);
    // if mask & 2: rc_dec(right).
    code.extend_from_slice(&[0xF7, 0xC7, 0x02, 0x00, 0x00, 0x00]); // test edi, 2
    code.extend_from_slice(&[0x74, 0x08]); // jz +8
    code.extend_from_slice(&[0x48, 0x89, 0xF1]); // mov rcx, rsi (right)
    emit_helper_call(&mut code, &mut relocations, RC_DEC_SYMBOL);

    code.extend_from_slice(&[0x4C, 0x89, 0xE0]); // mov rax, r12 (result)
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]); // add rsp, 0x28
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0x5B); // pop rbx
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_CONCAT_OWN_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_str_len_own(rcx = fresh-temp string record) -> rax = char_len`.
///
/// Reads the `char_len` header into a callee-saved register, then `rc_dec`s the
/// record (reclaiming the uniquely-owned temporary), and returns the length. Used
/// for `len(<fresh temp>)` so the temporary the length is read from does not leak.
pub(crate) fn emit_str_len_own_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Prologue: 1 callee-saved push (%16 -> 0), `sub rsp, 32` (shadow) keeps %16 == 0
    // at the internal rc_dec call.
    code.push(0x53); // push rbx
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x20]); // sub rsp, 32

    // rbx = char_len (read while the record is still live); rcx (the pointer) is the
    // rc_dec argument and is preserved by the call.
    code.extend_from_slice(&[0x48, 0x8B, 0x59, 0x00]); // mov rbx, [rcx + STR_CHAR_LEN_OFF]
    emit_helper_call(&mut code, &mut relocations, RC_DEC_SYMBOL);
    code.extend_from_slice(&[0x48, 0x89, 0xD8]); // mov rax, rbx (the length)

    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x20]); // add rsp, 32
    code.push(0x5B); // pop rbx
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_LEN_OWN_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_str_from_int(rcx = value, rdx = signed_flag) -> rax = string record`.
///
/// Formats `value` in decimal. When `signed_flag` is nonzero the value is treated
/// as a signed `i64` (a leading `-` for a negative value, magnitude computed as a
/// `u64` so `i64::MIN` formats correctly); when zero it is an unsigned `u64`. Two
/// passes: pass 1 counts the digits (so the exact record size is known), pass 2
/// writes the digits backward directly into the freshly allocated heap record (no
/// stack buffer). `char_len == byte_len` (all ASCII). Matches the interpreters'
/// integer `Display`.
pub(crate) fn emit_str_from_int_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Prologue: preserve rbx/rsi/rdi/r12/r13 (5 callee-saved pushes). On entry
    // rsp%16 == 8; 5 pushes → %16 == 0; `sub rsp, 32` (shadow space) keeps %16 ==
    // 0 at the internal alloc call.
    //   rbx = magnitude, rdi = neg flag, r13 = digit count, r12 = byte_len / dst.
    code.push(0x53); // push rbx
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x41, 0x55]); // push r13
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x20]); // sub rsp, 32

    // rdi = neg flag (0/1); rbx = magnitude (u64).
    code.extend_from_slice(&[0x48, 0x31, 0xFF]); // xor rdi, rdi
    code.extend_from_slice(&[0x48, 0x89, 0xCB]); // mov rbx, rcx (value/magnitude)
    code.extend_from_slice(&[0x48, 0x85, 0xD2]); // test rdx, rdx (signed?)
    code.extend_from_slice(&[0x74, 0x0D]); // jz Lu (skip 13 bytes)
    code.extend_from_slice(&[0x48, 0x85, 0xDB]); // test rbx, rbx
    code.extend_from_slice(&[0x7D, 0x08]); // jns Lu (skip 8 bytes)
    code.extend_from_slice(&[0x48, 0xF7, 0xDB]); // neg rbx
    code.push(0xBF); // mov edi, 1
    code.extend_from_slice(&1i32.to_le_bytes());
    // Lu:

    // Pass 1: count digits into r13 (minimum 1), leaving rbx (magnitude) intact.
    code.extend_from_slice(&[0x48, 0x89, 0xD8]); // mov rax, rbx (temp copy)
    code.extend_from_slice(&[0x41, 0xBD]); // mov r13d, 1
    code.extend_from_slice(&1i32.to_le_bytes());
    code.push(0xB9); // mov ecx, 10
    code.extend_from_slice(&10i32.to_le_bytes());
    let count_loop = code.len();
    code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx
    code.extend_from_slice(&[0x48, 0xF7, 0xF1]); // div rcx (rax /= 10)
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.extend_from_slice(&[0x74, 0x05]); // jz Lcount_done (skip inc r13 (3) + jmp (2) = 5)
    code.extend_from_slice(&[0x49, 0xFF, 0xC5]); // inc r13
    emit_short_jmp_back(&mut code, count_loop); // jmp count_loop (2 bytes)
    // Lcount_done:

    // byte_len (r12) = digit count (r13) + neg flag (rdi).
    code.extend_from_slice(&[0x4D, 0x89, 0xEC]); // mov r12, r13
    code.extend_from_slice(&[0x49, 0x01, 0xFC]); // add r12, rdi

    // Allocate STR_DATA_OFF + byte_len. rcx = r12 + STR_DATA_OFF.
    code.extend_from_slice(&[0x49, 0x8D, 0x8C, 0x24]); // lea rcx, [r12 + disp32]
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    emit_helper_call_alloc(&mut code, &mut relocations); // rax = dst

    // Headers: char_len = byte_len = r12. Save dst base by pushing rax.
    code.extend_from_slice(&[0x4C, 0x89, 0xA0]); // mov [rax + CHAR_LEN], r12
    code.extend_from_slice(&STR_CHAR_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0xA0]); // mov [rax + BYTE_LEN], r12
    code.extend_from_slice(&STR_BYTE_LEN_OFF.to_le_bytes());
    code.push(0x50); // push rax (record base)

    // rsi = write cursor = rax + STR_DATA_OFF + byte_len (one past the last byte).
    code.extend_from_slice(&[0x48, 0x8D, 0xB0]); // lea rsi, [rax + disp32]
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x01, 0xE6]); // add rsi, r12

    // Pass 2: write digits backward into the heap. rcx = 10 divisor.
    code.push(0xB9); // mov ecx, 10
    code.extend_from_slice(&10i32.to_le_bytes());
    let write_loop = code.len();
    code.extend_from_slice(&[0x48, 0x89, 0xD8]); // mov rax, rbx
    code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx
    code.extend_from_slice(&[0x48, 0xF7, 0xF1]); // div rcx (rax=quot, rdx=rem)
    code.extend_from_slice(&[0x48, 0x89, 0xC3]); // mov rbx, rax (quotient)
    code.extend_from_slice(&[0x80, 0xC2, 0x30]); // add dl, '0'
    code.extend_from_slice(&[0x48, 0xFF, 0xCE]); // dec rsi
    code.extend_from_slice(&[0x88, 0x16]); // mov [rsi], dl
    code.extend_from_slice(&[0x48, 0x85, 0xDB]); // test rbx, rbx
    code.extend_from_slice(&[0x0F, 0x85]); // jnz write_loop (rel32)
    let write_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    patch_rel32_to(&mut code, write_site, write_loop);

    // If negative: dec rsi; [rsi] = '-'.
    code.extend_from_slice(&[0x48, 0x85, 0xFF]); // test rdi, rdi
    code.extend_from_slice(&[0x74, 0x06]); // jz Lns (skip dec rsi (3) + mov (3) = 6)
    code.extend_from_slice(&[0x48, 0xFF, 0xCE]); // dec rsi
    code.extend_from_slice(&[0xC6, 0x06, 0x2D]); // mov byte [rsi], '-'
    // Lns:

    code.push(0x58); // pop rax (record base) — return value
    // Epilogue.
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x20]); // add rsp, 32
    code.extend_from_slice(&[0x41, 0x5D]); // pop r13
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0x5B); // pop rbx
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_FROM_INT_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_str_from_bool(rcx = 0/1) -> rax = "false"/"true" record`.
///
/// Builds a fresh 4- or 5-byte record. The bytes are materialized from immediates
/// (no `.rdata` constant), so a bool-only program stays self-contained.
pub(crate) fn emit_str_from_bool_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Prologue: 1 push (rbx) makes rsp%16 == 0; a `sub rsp, 40` reserves shadow
    // space and preserves alignment (%16 == 8 → the call sees %16 == 0 after the
    // return-address push). rbx holds the 0/1 selector across the alloc call.
    code.push(0x53); // push rbx
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x20]); // sub rsp, 32
    code.extend_from_slice(&[0x48, 0x89, 0xCB]); // mov rbx, rcx (selector)

    // byte_len = (selector != 0) ? 4 : 5. rcx = 5; if rbx != 0, rcx = 4.
    code.push(0xB9);
    code.extend_from_slice(&5i32.to_le_bytes()); // mov ecx, 5
    code.extend_from_slice(&[0x48, 0x85, 0xDB]); // test rbx, rbx
    code.extend_from_slice(&[0x74, 0x05]); // jz alloc (skip mov ecx,4)
    code.push(0xB9);
    code.extend_from_slice(&4i32.to_le_bytes()); // mov ecx, 4
    // alloc: rcx += STR_DATA_OFF.
    code.extend_from_slice(&[0x48, 0x81, 0xC1]); // add rcx, imm32
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    emit_helper_call_alloc(&mut code, &mut relocations); // rax = dst

    // Write headers + bytes, branching on the selector with patched rel32 jumps.
    code.extend_from_slice(&[0x48, 0x85, 0xDB]); // test rbx, rbx
    // jz false_path (rel32, patched).
    code.extend_from_slice(&[0x0F, 0x84]);
    let to_false_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // true_path: char_len = byte_len = 4; bytes = "true".
    code.extend_from_slice(&[0x48, 0xC7, 0x80]); // mov qword [rax + CHAR_LEN], 4
    code.extend_from_slice(&STR_CHAR_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&4i32.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0x80]); // mov qword [rax + BYTE_LEN], 4
    code.extend_from_slice(&STR_BYTE_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&4i32.to_le_bytes());
    // mov dword [rax + STR_DATA_OFF], "true"
    code.extend_from_slice(&[0xC7, 0x80]);
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    code.extend_from_slice(b"true");
    // jmp done (rel32, patched).
    code.push(0xE9);
    let true_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // false_path: char_len = byte_len = 5; bytes = "false".
    patch_rel32(&mut code, to_false_site);
    code.extend_from_slice(&[0x48, 0xC7, 0x80]); // mov qword [rax + CHAR_LEN], 5
    code.extend_from_slice(&STR_CHAR_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&5i32.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0x80]); // mov qword [rax + BYTE_LEN], 5
    code.extend_from_slice(&STR_BYTE_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&5i32.to_le_bytes());
    // mov dword [rax + STR_DATA_OFF], "fals"
    code.extend_from_slice(&[0xC7, 0x80]);
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    code.extend_from_slice(b"fals");
    // mov byte [rax + STR_DATA_OFF + 4], 'e'
    code.extend_from_slice(&[0xC6, 0x80]);
    code.extend_from_slice(&(STR_DATA_OFF + 4).to_le_bytes());
    code.push(b'e');
    // done:
    patch_rel32(&mut code, true_done_site);

    // Epilogue.
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x20]); // add rsp, 32
    code.push(0x5B); // pop rbx
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_FROM_BOOL_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_str_from_char(rcx = code point) -> rax = one-char string record`.
///
/// Encodes the Unicode scalar value in `rcx` as UTF-8 (1–4 bytes) directly into
/// the record's data area, with `char_len = 1` and `byte_len` = the encoded
/// length. Matches Rust's `char` Display (the interpreters' `to_string(char)`).
/// The frontend guarantees a valid scalar value, so no range validation is done.
pub(crate) fn emit_str_from_char_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Prologue: preserve rbx (code point) and rsi/rdi; `sub rsp, 8` aligns
    // (3 pushes + ret = 32 → %16 == 0; sub 8 → still need %16 == 0 at the call:
    // 3 pushes make rsp%16 == 8, sub 8 → %16 == 0). Wait: use 3 pushes + sub 8.
    code.push(0x53); // push rbx
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]); // sub rsp, 8
    code.extend_from_slice(&[0x48, 0x89, 0xCB]); // mov rbx, rcx (code point)

    // Determine byte_len from the code point into rsi:
    //   cp < 0x80        -> 1
    //   cp < 0x800       -> 2
    //   cp < 0x10000     -> 3
    //   else             -> 4
    code.push(0xBE);
    code.extend_from_slice(&1i32.to_le_bytes()); // mov esi, 1
    code.extend_from_slice(&[0x48, 0x81, 0xFB]); // cmp rbx, 0x80
    code.extend_from_slice(&0x80i32.to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8C]); // jl len_done (rel32)
    let len1_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.push(0xBE);
    code.extend_from_slice(&2i32.to_le_bytes()); // mov esi, 2
    code.extend_from_slice(&[0x48, 0x81, 0xFB]); // cmp rbx, 0x800
    code.extend_from_slice(&0x800i32.to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8C]); // jl len_done
    let len2_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.push(0xBE);
    code.extend_from_slice(&3i32.to_le_bytes()); // mov esi, 3
    code.extend_from_slice(&[0x48, 0x81, 0xFB]); // cmp rbx, 0x10000
    code.extend_from_slice(&0x10000i32.to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8C]); // jl len_done
    let len3_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.push(0xBE);
    code.extend_from_slice(&4i32.to_le_bytes()); // mov esi, 4
    // len_done:
    let len_done = code.len();
    patch_rel32_to(&mut code, len1_site, len_done);
    patch_rel32_to(&mut code, len2_site, len_done);
    patch_rel32_to(&mut code, len3_site, len_done);

    // Allocate STR_DATA_OFF + byte_len (rsi). rcx = rsi + STR_DATA_OFF.
    code.extend_from_slice(&[0x48, 0x8D, 0x8E]); // lea rcx, [rsi + disp32]
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    emit_helper_call_alloc(&mut code, &mut relocations); // rax = dst

    // Headers: char_len = 1; byte_len = rsi.
    code.extend_from_slice(&[0x48, 0xC7, 0x80]); // mov qword [rax + CHAR_LEN], 1
    code.extend_from_slice(&STR_CHAR_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&1i32.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0xB0]); // mov [rax + BYTE_LEN], rsi
    code.extend_from_slice(&STR_BYTE_LEN_OFF.to_le_bytes());

    // rdi = data pointer = rax + STR_DATA_OFF.
    code.extend_from_slice(&[0x48, 0x8D, 0xB8]); // lea rdi, [rax + disp32]
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    // Save record base for return (push rax; restored at the end).
    code.push(0x50); // push rax

    // Branch on byte_len (rsi) to the encoder. cp is in rbx; work in rcx/rdx.
    // 1-byte: [rdi] = cp.
    code.extend_from_slice(&[0x48, 0x83, 0xFE, 0x01]); // cmp rsi, 1
    code.extend_from_slice(&[0x0F, 0x85]); // jne two_plus
    let one_ne_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x88, 0x1F]); // mov [rdi], bl
    code.push(0xE9); // jmp encode_done
    let one_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // two_plus:
    patch_rel32(&mut code, one_ne_site);
    code.extend_from_slice(&[0x48, 0x83, 0xFE, 0x02]); // cmp rsi, 2
    code.extend_from_slice(&[0x0F, 0x85]); // jne three_plus
    let two_ne_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // 2-byte: b0 = 0xC0 | (cp >> 6); b1 = 0x80 | (cp & 0x3F).
    // rcx = cp >> 6; or 0xC0; store.
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0xC1, 0xE9, 0x06]); // shr rcx, 6
    code.extend_from_slice(&[0x80, 0xC9, 0xC0]); // or cl, 0xC0
    code.extend_from_slice(&[0x88, 0x0F]); // mov [rdi], cl
    // rcx = cp & 0x3F; or 0x80; store at [rdi+1].
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x80, 0xE1, 0x3F]); // and cl, 0x3F
    code.extend_from_slice(&[0x80, 0xC9, 0x80]); // or cl, 0x80
    code.extend_from_slice(&[0x88, 0x4F, 0x01]); // mov [rdi + 1], cl
    code.push(0xE9); // jmp encode_done
    let two_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // three_plus:
    patch_rel32(&mut code, two_ne_site);
    code.extend_from_slice(&[0x48, 0x83, 0xFE, 0x03]); // cmp rsi, 3
    code.extend_from_slice(&[0x0F, 0x85]); // jne four
    let three_ne_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // 3-byte: b0 = 0xE0 | (cp >> 12); b1 = 0x80 | ((cp >> 6) & 0x3F);
    //         b2 = 0x80 | (cp & 0x3F).
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0xC1, 0xE9, 0x0C]); // shr rcx, 12
    code.extend_from_slice(&[0x80, 0xC9, 0xE0]); // or cl, 0xE0
    code.extend_from_slice(&[0x88, 0x0F]); // mov [rdi], cl
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0xC1, 0xE9, 0x06]); // shr rcx, 6
    code.extend_from_slice(&[0x80, 0xE1, 0x3F]); // and cl, 0x3F
    code.extend_from_slice(&[0x80, 0xC9, 0x80]); // or cl, 0x80
    code.extend_from_slice(&[0x88, 0x4F, 0x01]); // mov [rdi + 1], cl
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x80, 0xE1, 0x3F]); // and cl, 0x3F
    code.extend_from_slice(&[0x80, 0xC9, 0x80]); // or cl, 0x80
    code.extend_from_slice(&[0x88, 0x4F, 0x02]); // mov [rdi + 2], cl
    code.push(0xE9); // jmp encode_done
    let three_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // four:
    patch_rel32(&mut code, three_ne_site);
    // 4-byte: b0 = 0xF0 | (cp >> 18); b1 = 0x80 | ((cp >> 12) & 0x3F);
    //         b2 = 0x80 | ((cp >> 6) & 0x3F); b3 = 0x80 | (cp & 0x3F).
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0xC1, 0xE9, 0x12]); // shr rcx, 18
    code.extend_from_slice(&[0x80, 0xC9, 0xF0]); // or cl, 0xF0
    code.extend_from_slice(&[0x88, 0x0F]); // mov [rdi], cl
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0xC1, 0xE9, 0x0C]); // shr rcx, 12
    code.extend_from_slice(&[0x80, 0xE1, 0x3F]); // and cl, 0x3F
    code.extend_from_slice(&[0x80, 0xC9, 0x80]); // or cl, 0x80
    code.extend_from_slice(&[0x88, 0x4F, 0x01]); // mov [rdi + 1], cl
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x48, 0xC1, 0xE9, 0x06]); // shr rcx, 6
    code.extend_from_slice(&[0x80, 0xE1, 0x3F]); // and cl, 0x3F
    code.extend_from_slice(&[0x80, 0xC9, 0x80]); // or cl, 0x80
    code.extend_from_slice(&[0x88, 0x4F, 0x02]); // mov [rdi + 2], cl
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x80, 0xE1, 0x3F]); // and cl, 0x3F
    code.extend_from_slice(&[0x80, 0xC9, 0x80]); // or cl, 0x80
    code.extend_from_slice(&[0x88, 0x4F, 0x03]); // mov [rdi + 3], cl

    // encode_done:
    let encode_done = code.len();
    patch_rel32_to(&mut code, one_done_site, encode_done);
    patch_rel32_to(&mut code, two_done_site, encode_done);
    patch_rel32_to(&mut code, three_done_site, encode_done);

    code.push(0x58); // pop rax (record base)
    // Epilogue.
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]); // add rsp, 8
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0x5B); // pop rbx
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_FROM_CHAR_SYMBOL.to_string(),
        code,
        relocations,
    }
}

// -- Index-based string op helpers (native) ----------------------------------
//
// These five leaf-ish helpers implement the char/byte-aware string operations
// over the heap `[char_len i64][byte_len i64][utf8]` record, matching the
// interpreters and the WASM backend bit-for-bit:
//   * `substring` is char-indexed (`[start, end)`), traps (`ud2`, mirroring
//     `L0413`) on an out-of-bounds range, and maps char indices to byte offsets
//     by walking the UTF-8 (a byte is a char boundary when `(b & 0xC0) != 0x80`);
//   * `find` returns the CHAR index of the first BYTE-level match (or `-1`),
//     counting the non-continuation bytes before the matched byte offset;
//   * `contains`/`starts_with`/`ends_with` are BYTE-exact predicates.
// Only `substring` allocates (it builds a fresh record), so only it needs a
// stack frame kept 16-byte aligned at the internal `__lullaby_alloc` call; the
// others are pure scans that preserve the callee-saved registers they use.

/// Emit a byte-compare of `needle` against the haystack window whose first byte
/// is addressed by `r11` (`hay_cur`). Reads `rdi` (needle_data) and `r12`
/// (needle_len). Leaves `1` in `rax` if every needle byte matches, else `0`. An
/// empty needle (`needle_len == 0`) yields `1` (the loop runs zero times),
/// matching Rust's empty-prefix/substring semantics. Clobbers `rax`, `rcx`, `rdx`,
/// `r9`. The caller guarantees the window has at least `needle_len` bytes.
pub(crate) fn emit_str_match_at_into_rax(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x4D, 0x31, 0xC9]); // xor r9, r9 (j = 0)
    let loop_top = code.len();
    // if j >= needle_len -> matched (rax = 1).
    code.extend_from_slice(&[0x4D, 0x39, 0xE1]); // cmp r9, r12
    code.extend_from_slice(&[0x0F, 0x8D]); // jge matched (rel32)
    let matched_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // cl = hay_cur[j]; dl = needle_data[j].
    code.extend_from_slice(&[0x43, 0x8A, 0x0C, 0x0B]); // mov cl, [r11 + r9]
    code.extend_from_slice(&[0x42, 0x8A, 0x14, 0x0F]); // mov dl, [rdi + r9]
    code.extend_from_slice(&[0x38, 0xD1]); // cmp cl, dl
    code.extend_from_slice(&[0x0F, 0x85]); // jne mismatch (rel32)
    let mismatch_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x49, 0xFF, 0xC1]); // inc r9
    emit_jmp_to(code, loop_top); // jmp loop_top
    // matched: rax = 1; jmp done.
    patch_rel32(code, matched_site);
    code.extend_from_slice(&[0x48, 0xC7, 0xC0]); // mov rax, 1
    code.extend_from_slice(&1i32.to_le_bytes());
    code.extend_from_slice(&[0xE9]); // jmp done (rel32)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // mismatch: rax = 0.
    patch_rel32(code, mismatch_site);
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax
    // done:
    patch_rel32(code, done_site);
}

/// Emit the first-match byte search. Reads `rsi` (hay_data), `rdi` (needle_data),
/// `r13` (hay_len), `r12` (needle_len). Leaves `1`/`0` in `rax` (found flag) and,
/// when found, the matched byte position in `r8`. Tries every start
/// `0..=(hay_len - needle_len)` and stops at the first full match; when
/// `needle_len > hay_len` the limit is negative so no start is tried and the flag
/// stays `0`. An empty needle matches at byte `0`. Clobbers `rax`, `rcx`, `rdx`,
/// `r8`, `r9`, `r10`, `r11`.
pub(crate) fn emit_str_byte_search(code: &mut Vec<u8>) {
    // limit = hay_len - needle_len (last valid start, inclusive; may be negative).
    code.extend_from_slice(&[0x4D, 0x89, 0xEA]); // mov r10, r13
    code.extend_from_slice(&[0x4D, 0x29, 0xE2]); // sub r10, r12
    code.extend_from_slice(&[0x4D, 0x31, 0xC0]); // xor r8, r8 (pos = 0)
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax (found = 0)
    let outer = code.len();
    code.extend_from_slice(&[0x4D, 0x39, 0xD0]); // cmp r8, r10
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done (pos > limit, rel32)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // hay_cur = hay_data + pos.
    code.extend_from_slice(&[0x49, 0x89, 0xF3]); // mov r11, rsi
    code.extend_from_slice(&[0x4D, 0x01, 0xC3]); // add r11, r8
    emit_str_match_at_into_rax(code); // rax = match_at(pos)
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.extend_from_slice(&[0x0F, 0x85]); // jnz done (found; rel32)
    let found_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x49, 0xFF, 0xC0]); // inc r8 (pos += 1)
    emit_jmp_to(code, outer); // jmp outer
    // done: rax already holds the flag (1 from the matched branch, or 0).
    patch_rel32(code, done_site);
    patch_rel32(code, found_done_site);
}

/// Load a string record's `hay_data`/`needle_data`/lengths for a two-string op.
/// After this, `rsi = a_data (a+DATA)`, `rdi = b_data (b+DATA)`, `r13 = a_byte_len`,
/// `r12 = b_byte_len`, with `rcx = a` and `rdx = b` on entry.
pub(crate) fn emit_load_two_string_operands(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x4C, 0x8B, 0x69, 0x08]); // mov r13, [rcx + 8] (a byte_len)
    code.extend_from_slice(&[0x48, 0x8D, 0x71, 0x10]); // lea rsi, [rcx + 16] (a data)
    code.extend_from_slice(&[0x4C, 0x8B, 0x62, 0x08]); // mov r12, [rdx + 8] (b byte_len)
    code.extend_from_slice(&[0x48, 0x8D, 0x7A, 0x10]); // lea rdi, [rdx + 16] (b data)
}

/// `__lullaby_str_find(rcx = haystack, rdx = needle) -> rax = i64 char index`.
///
/// Byte-searches for the first needle occurrence; on a hit, counts the UTF-8
/// characters before the matched byte offset (`text[..byte].chars().count()`) and
/// returns that char index; on a miss returns `-1`. An empty needle matches at
/// byte `0`, whose preceding char count is `0`. A leaf function (no allocation);
/// preserves the callee-saved `rsi`/`rdi`/`r12`/`r13` it uses.
pub(crate) fn emit_str_find_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();

    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x41, 0x55]); // push r13
    emit_load_two_string_operands(&mut code);
    emit_str_byte_search(&mut code); // rax = found flag, r8 = byte pos
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.extend_from_slice(&[0x0F, 0x84]); // jz not_found (rel32)
    let not_found_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // Count non-continuation bytes in hay_data[0 .. r8) into rax.
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax (count)
    code.extend_from_slice(&[0x4D, 0x31, 0xC9]); // xor r9, r9 (bi)
    let cloop = code.len();
    code.extend_from_slice(&[0x4D, 0x39, 0xC1]); // cmp r9, r8
    code.extend_from_slice(&[0x0F, 0x8D]); // jge count_done (rel32)
    let count_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x42, 0x8A, 0x0C, 0x0E]); // mov cl, [rsi + r9]
    code.extend_from_slice(&[0x80, 0xE1, 0xC0]); // and cl, 0xC0
    code.extend_from_slice(&[0x80, 0xF9, 0x80]); // cmp cl, 0x80
    code.extend_from_slice(&[0x0F, 0x84]); // je cskip (continuation byte; rel32)
    let cskip_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax (count += 1)
    patch_rel32(&mut code, cskip_site);
    code.extend_from_slice(&[0x49, 0xFF, 0xC1]); // inc r9 (bi += 1)
    emit_jmp_to(&mut code, cloop); // jmp cloop
    // not_found: rax = -1.
    patch_rel32(&mut code, not_found_site);
    code.extend_from_slice(&[0x48, 0xC7, 0xC0]); // mov rax, -1
    code.extend_from_slice(&(-1i32).to_le_bytes());
    // count_done:
    patch_rel32(&mut code, count_done_site);

    code.extend_from_slice(&[0x41, 0x5D]); // pop r13
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_FIND_SYMBOL.to_string(),
        code,
        relocations: Vec::new(),
    }
}

/// `__lullaby_str_contains(rcx = s, rdx = sub) -> rax = 0/1`.
///
/// Byte-exact substring test: emits the same byte search as `find` and returns its
/// found flag. An empty substring is contained. A leaf function; preserves the
/// callee-saved registers it uses.
pub(crate) fn emit_str_contains_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();

    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x41, 0x55]); // push r13
    emit_load_two_string_operands(&mut code);
    emit_str_byte_search(&mut code); // rax = found flag (0/1) — the result
    code.extend_from_slice(&[0x41, 0x5D]); // pop r13
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_CONTAINS_SYMBOL.to_string(),
        code,
        relocations: Vec::new(),
    }
}

/// `__lullaby_str_count(rcx = haystack, rdx = needle) -> rax = i64 count`.
///
/// Counts NON-overlapping byte-level needle occurrences (matches
/// `text.matches(sub).count()`): scans each start `pos`, and on a match at `pos`
/// increments the count and advances `pos` by `needle_len` (non-overlapping),
/// else advances by 1. An empty needle yields `0`. A leaf function; preserves the
/// callee-saved `rsi`/`rdi`/`r12`/`r13` it uses. `count` lives in the volatile
/// `r8`, `pos` in `r10` — neither is clobbered by `emit_str_match_at_into_rax`
/// (which only touches rax/rcx/rdx/r9 and reads r11/r12/rdi).
pub(crate) fn emit_str_count_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x41, 0x55]); // push r13
    emit_load_two_string_operands(&mut code); // rsi=hay, rdi=needle, r13=hay_len, r12=needle_len

    // Empty needle -> 0.
    code.extend_from_slice(&[0x4D, 0x85, 0xE4]); // test r12, r12
    code.extend_from_slice(&[0x0F, 0x85]); // jnz nonempty
    let nonempty_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax (count = 0)
    code.extend_from_slice(&[0xE9]); // jmp epilogue
    let empty_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    patch_rel32(&mut code, nonempty_site);
    code.extend_from_slice(&[0x4D, 0x31, 0xC0]); // xor r8, r8 (count = 0)
    code.extend_from_slice(&[0x4D, 0x31, 0xD2]); // xor r10, r10 (pos = 0)
    let loop_top = code.len();
    // limit = hay_len - needle_len; if pos > limit -> done.
    code.extend_from_slice(&[0x4C, 0x89, 0xE8]); // mov rax, r13 (hay_len)
    code.extend_from_slice(&[0x4C, 0x29, 0xE0]); // sub rax, r12 (needle_len)
    code.extend_from_slice(&[0x49, 0x39, 0xC2]); // cmp r10, rax
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // hay_cur = hay_data + pos.
    code.extend_from_slice(&[0x49, 0x89, 0xF3]); // mov r11, rsi
    code.extend_from_slice(&[0x4D, 0x01, 0xD3]); // add r11, r10
    emit_str_match_at_into_rax(&mut code); // rax = match_at(pos)
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.extend_from_slice(&[0x0F, 0x84]); // jz nomatch
    let nomatch_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // matched: count += 1; pos += needle_len.
    code.extend_from_slice(&[0x49, 0xFF, 0xC0]); // inc r8
    code.extend_from_slice(&[0x4D, 0x01, 0xE2]); // add r10, r12
    emit_jmp_to(&mut code, loop_top);
    // nomatch: pos += 1.
    patch_rel32(&mut code, nomatch_site);
    code.extend_from_slice(&[0x49, 0xFF, 0xC2]); // inc r10
    emit_jmp_to(&mut code, loop_top);
    // done: rax = count.
    patch_rel32(&mut code, done_site);
    code.extend_from_slice(&[0x4C, 0x89, 0xC0]); // mov rax, r8

    // epilogue:
    patch_rel32(&mut code, empty_done_site);
    code.extend_from_slice(&[0x41, 0x5D]); // pop r13
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_COUNT_SYMBOL.to_string(),
        code,
        relocations: Vec::new(),
    }
}

/// `__lullaby_str_trim(rcx = s) -> rax = record`.
///
/// Scans off leading/trailing ASCII whitespace (`0x20`, or `0x09..=0x0D`) to a
/// `[start, end)` byte range, then delegates to `__lullaby_str_substring` (passing
/// the byte offsets as char indices — equal for ASCII strings). An all-whitespace
/// input yields the empty string (`start == end`). One `call`, so `rsp` is aligned
/// with `sub rsp,8` first; uses only volatile scratch (rax/rdx/r8/r9/r10/r11) plus
/// the incoming `rcx` (the source, forwarded to substring), so no callee-saved
/// register needs preserving here.
pub(crate) fn emit_str_trim_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]); // sub rsp, 8 (align the substring call)
    code.extend_from_slice(&[0x4C, 0x8B, 0x49, 0x08]); // mov r9, [rcx+8] (byte_len)
    code.extend_from_slice(&[0x4C, 0x8D, 0x51, 0x10]); // lea r10, [rcx+16] (data)

    // A whitespace test on the byte in `dl`: two rel32 conditional jumps to
    // `on_ws`. Caller passes the two patch-site vectors to fill.
    // Forward scan: rax = start = first non-whitespace byte.
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax (start = 0)
    let fwd = code.len();
    code.extend_from_slice(&[0x4C, 0x39, 0xC8]); // cmp rax, r9
    code.extend_from_slice(&[0x0F, 0x8D]); // jge fwd_done
    let fwd_done_a = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x41, 0x0F, 0xB6, 0x14, 0x02]); // movzx edx, byte [r10+rax]
    code.extend_from_slice(&[0x80, 0xFA, 0x20]); // cmp dl, 0x20
    code.extend_from_slice(&[0x0F, 0x84]); // je is_ws_f
    let ws_f1 = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x44, 0x8D, 0x5A, 0xF7]); // lea r11d, [rdx-9]
    code.extend_from_slice(&[0x41, 0x83, 0xFB, 0x04]); // cmp r11d, 4
    code.extend_from_slice(&[0x0F, 0x86]); // jbe is_ws_f
    let ws_f2 = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.push(0xE9); // jmp fwd_done (non-ws found)
    let fwd_done_b = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // is_ws_f: inc rax; loop.
    patch_rel32(&mut code, ws_f1);
    patch_rel32(&mut code, ws_f2);
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
    emit_jmp_to(&mut code, fwd);
    // fwd_done:
    patch_rel32(&mut code, fwd_done_a);
    patch_rel32(&mut code, fwd_done_b);

    // Backward scan: r8 = end, shrink while data[end-1] is whitespace.
    code.extend_from_slice(&[0x4D, 0x89, 0xC8]); // mov r8, r9 (end = byte_len)
    let bwd = code.len();
    code.extend_from_slice(&[0x49, 0x39, 0xC0]); // cmp r8, rax
    code.extend_from_slice(&[0x0F, 0x8E]); // jle bwd_done (end <= start)
    let bwd_done_a = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x43, 0x0F, 0xB6, 0x54, 0x02, 0xFF]); // movzx edx, byte [r10+r8-1]
    code.extend_from_slice(&[0x80, 0xFA, 0x20]); // cmp dl, 0x20
    code.extend_from_slice(&[0x0F, 0x84]); // je is_ws_b
    let ws_b1 = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x44, 0x8D, 0x5A, 0xF7]); // lea r11d, [rdx-9]
    code.extend_from_slice(&[0x41, 0x83, 0xFB, 0x04]); // cmp r11d, 4
    code.extend_from_slice(&[0x0F, 0x86]); // jbe is_ws_b
    let ws_b2 = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.push(0xE9); // jmp bwd_done (non-ws at end-1)
    let bwd_done_b = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // is_ws_b: dec r8; loop.
    patch_rel32(&mut code, ws_b1);
    patch_rel32(&mut code, ws_b2);
    code.extend_from_slice(&[0x49, 0xFF, 0xC8]); // dec r8
    emit_jmp_to(&mut code, bwd);
    // bwd_done:
    patch_rel32(&mut code, bwd_done_a);
    patch_rel32(&mut code, bwd_done_b);

    // substring(rcx = text, rdx = start, r8 = end). rcx still holds the source.
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (start)
    code.push(0xE8); // call __lullaby_str_substring
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: STR_SUBSTRING_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]); // add rsp, 8
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_TRIM_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_str_repeat(rcx = s, rdx = count) -> rax = record`.
///
/// Builds a fresh `[char_len][byte_len][utf8]` record equal to the source repeated
/// `count` times (`count <= 0` → the empty string). Allocates `DATA + byte_len*
/// count` bytes and `rep movsb`-copies the source `count` times. Preserves the
/// callee-saved registers held across the internal `__lullaby_alloc` call and
/// keeps `rsp` 16-byte aligned at that call (8 pushes + return addr = even, then
/// `sub rsp,8`).
pub(crate) fn emit_str_repeat_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    code.push(0x53); // push rbx
    code.push(0x55); // push rbp
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x41, 0x55]); // push r13
    code.extend_from_slice(&[0x41, 0x56]); // push r14
    code.extend_from_slice(&[0x41, 0x57]); // push r15
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]); // sub rsp, 8

    // count <= 0 -> empty string.
    code.extend_from_slice(&[0x48, 0x85, 0xD2]); // test rdx, rdx
    code.extend_from_slice(&[0x0F, 0x8F]); // jg nonempty
    let nonempty_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // Empty record: alloc DATA bytes, char_len = byte_len = 0.
    code.extend_from_slice(&[0x48, 0xC7, 0xC1]); // mov rcx, imm32
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    emit_helper_call_alloc(&mut code, &mut relocations); // rax = record
    code.extend_from_slice(&[0x48, 0xC7, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00]); // mov qword [rax+0], 0
    code.extend_from_slice(&[0x48, 0xC7, 0x40, 0x08, 0x00, 0x00, 0x00, 0x00]); // mov qword [rax+8], 0
    code.extend_from_slice(&[0xE9]); // jmp epilogue
    let empty_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // nonempty:
    patch_rel32(&mut code, nonempty_site);
    code.extend_from_slice(&[0x49, 0x89, 0xCC]); // mov r12, rcx (source)
    code.extend_from_slice(&[0x49, 0x89, 0xD5]); // mov r13, rdx (count)
    code.extend_from_slice(&[0x4C, 0x8B, 0x71, 0x00]); // mov r14, [rcx+0] (orig char_len)
    code.extend_from_slice(&[0x4C, 0x8B, 0x79, 0x08]); // mov r15, [rcx+8] (orig byte_len)
    // new_byte_len = orig_byte_len * count.
    code.extend_from_slice(&[0x4C, 0x89, 0xF8]); // mov rax, r15
    code.extend_from_slice(&[0x49, 0x0F, 0xAF, 0xC5]); // imul rax, r13
    code.extend_from_slice(&[0x48, 0x89, 0xC3]); // mov rbx, rax (new_byte_len)
    // alloc(DATA + new_byte_len).
    code.extend_from_slice(&[0x48, 0x8D, 0x48, STR_DATA_OFF as u8]); // lea rcx, [rax+16]
    emit_helper_call_alloc(&mut code, &mut relocations); // rax = record
    code.extend_from_slice(&[0x48, 0x89, 0xC5]); // mov rbp, rax (record base)
    // char_len = orig_char_len * count = r14 * r13.
    code.extend_from_slice(&[0x4C, 0x89, 0xF0]); // mov rax, r14
    code.extend_from_slice(&[0x49, 0x0F, 0xAF, 0xC5]); // imul rax, r13
    code.extend_from_slice(&[0x48, 0x89, 0x45, 0x00]); // mov [rbp+0], rax (char_len)
    code.extend_from_slice(&[0x48, 0x89, 0x5D, 0x08]); // mov [rbp+8], rbx (byte_len)
    // Copy loop: dest cursor rdi = &record.data; for k=count..0 copy orig bytes.
    code.extend_from_slice(&[0x48, 0x8D, 0x7D, STR_DATA_OFF as u8]); // lea rdi, [rbp+16]
    let copy_top = code.len();
    code.extend_from_slice(&[0x4D, 0x85, 0xED]); // test r13, r13
    code.extend_from_slice(&[0x0F, 0x84]); // jz copy_done
    let copy_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x49, 0x8D, 0x74, 0x24, STR_DATA_OFF as u8]); // lea rsi, [r12+16] (src reset each iter)
    code.extend_from_slice(&[0x4C, 0x89, 0xF9]); // mov rcx, r15 (orig byte_len)
    code.extend_from_slice(&[0xF3, 0xA4]); // rep movsb (rdi advances, persists)
    code.extend_from_slice(&[0x49, 0xFF, 0xCD]); // dec r13
    emit_jmp_to(&mut code, copy_top);
    // copy_done: rax = record.
    patch_rel32(&mut code, copy_done_site);
    code.extend_from_slice(&[0x48, 0x89, 0xE8]); // mov rax, rbp

    // epilogue:
    patch_rel32(&mut code, empty_done_site);
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]); // add rsp, 8
    code.extend_from_slice(&[0x41, 0x5F]); // pop r15
    code.extend_from_slice(&[0x41, 0x5E]); // pop r14
    code.extend_from_slice(&[0x41, 0x5D]); // pop r13
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0x5D); // pop rbp
    code.push(0x5B); // pop rbx
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_REPEAT_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_str_starts_with(rcx = s, rdx = prefix) -> rax = 0/1`.
///
/// If `prefix_len > s_len` the result is `0`; otherwise it is whether the prefix
/// bytes match at byte position `0`. An empty prefix matches. A leaf function.
pub(crate) fn emit_str_starts_with_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();

    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x41, 0x55]); // push r13
    emit_load_two_string_operands(&mut code);
    // if needle_len (r12) > hay_len (r13) -> 0.
    code.extend_from_slice(&[0x4D, 0x39, 0xEC]); // cmp r12, r13
    code.extend_from_slice(&[0x0F, 0x8F]); // jg false (rel32)
    let false_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // match_at(pos = 0): hay_cur = hay_data.
    code.extend_from_slice(&[0x49, 0x89, 0xF3]); // mov r11, rsi
    emit_str_match_at_into_rax(&mut code); // rax = match result
    code.extend_from_slice(&[0xE9]); // jmp done (rel32)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // false: rax = 0.
    patch_rel32(&mut code, false_site);
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax
    // done:
    patch_rel32(&mut code, done_site);

    code.extend_from_slice(&[0x41, 0x5D]); // pop r13
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_STARTS_WITH_SYMBOL.to_string(),
        code,
        relocations: Vec::new(),
    }
}

/// `__lullaby_str_ends_with(rcx = s, rdx = suffix) -> rax = 0/1`.
///
/// If `suffix_len > s_len` the result is `0`; otherwise it is whether the suffix
/// bytes match at byte position `s_len - suffix_len`. An empty suffix matches. A
/// leaf function.
pub(crate) fn emit_str_ends_with_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();

    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x41, 0x55]); // push r13
    emit_load_two_string_operands(&mut code);
    // if needle_len (r12) > hay_len (r13) -> 0.
    code.extend_from_slice(&[0x4D, 0x39, 0xEC]); // cmp r12, r13
    code.extend_from_slice(&[0x0F, 0x8F]); // jg false (rel32)
    let false_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // match_at(pos = hay_len - needle_len): hay_cur = hay_data + hay_len - needle_len.
    code.extend_from_slice(&[0x49, 0x89, 0xF3]); // mov r11, rsi
    code.extend_from_slice(&[0x4D, 0x01, 0xEB]); // add r11, r13 (+ hay_len)
    code.extend_from_slice(&[0x4D, 0x29, 0xE3]); // sub r11, r12 (- needle_len)
    emit_str_match_at_into_rax(&mut code); // rax = match result
    code.extend_from_slice(&[0xE9]); // jmp done (rel32)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // false: rax = 0.
    patch_rel32(&mut code, false_site);
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax
    // done:
    patch_rel32(&mut code, done_site);

    code.extend_from_slice(&[0x41, 0x5D]); // pop r13
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_ENDS_WITH_SYMBOL.to_string(),
        code,
        relocations: Vec::new(),
    }
}

/// `__lullaby_parse_i64(rcx = source string record ptr) -> (rax = tag, rdx = payload)`.
///
/// Parses the source bytes as a base-10 signed 64-bit integer with exactly Rust's
/// `str::parse::<i64>()` semantics: an optional single leading `+`/`-`, then one or
/// more ASCII digits, no surrounding whitespace, and a checked accumulation
/// (`imul`/`add`/`sub` with a `jo` after each) so any out-of-range value is an
/// error. On success returns tag `0` (`ok`) in `rax` and the value in `rdx`. On any
/// failure returns tag `1` (`err`) in `rax` and a freshly bump-allocated string
/// record in `rdx` holding the same `` cannot parse `{text}` as i64 `` message the
/// interpreters produce (prefix + the source bytes + suffix), so every backend
/// matches byte-for-byte. Accumulates in the sign's direction (`acc*10 - digit` for
/// a negative literal) so `i64::MIN` parses exactly like `checked` Rust does.
pub(crate) fn emit_parse_i64_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();
    let mut err_sites: Vec<usize> = Vec::new();

    // Prologue: preserve rbx/rsi/rdi/r12/r13 (5 pushes → rsp%16 == 0), then
    // `sub rsp, 32` keeps %16 == 0 at the internal alloc call and reserves shadow.
    code.push(0x53); // push rbx
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x41, 0x55]); // push r13
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x20]); // sub rsp, 32

    // rbx = src ptr; rsi = count (byte_len); rdi = i; r10 = acc; r11 = neg flag.
    code.extend_from_slice(&[0x48, 0x89, 0xCB]); // mov rbx, rcx
    code.extend_from_slice(&[0x48, 0x8B, 0x73, 0x08]); // mov rsi, [rbx + STR_BYTE_LEN_OFF]
    code.extend_from_slice(&[0x48, 0x31, 0xFF]); // xor rdi, rdi
    code.extend_from_slice(&[0x4D, 0x31, 0xD2]); // xor r10, r10
    code.extend_from_slice(&[0x4D, 0x31, 0xDB]); // xor r11, r11

    // Empty string -> err.
    code.extend_from_slice(&[0x48, 0x85, 0xF6]); // test rsi, rsi
    code.extend_from_slice(&[0x0F, 0x84]); // jz err
    err_sites.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);

    // First byte: optional single sign. movzx eax, byte [rbx + rdi + STR_DATA_OFF].
    code.extend_from_slice(&[0x0F, 0xB6, 0x84, 0x3B]);
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x3C, 0x2D]); // cmp al, '-'
    code.extend_from_slice(&[0x0F, 0x85]); // jne chk_plus
    let jne_plus = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // '-' branch: neg = 1, i = 1, then fall to after_sign.
    code.extend_from_slice(&[0x41, 0xBB, 0x01, 0x00, 0x00, 0x00]); // mov r11d, 1
    code.extend_from_slice(&[0x48, 0xFF, 0xC7]); // inc rdi
    code.push(0xE9); // jmp after_sign
    let jmp_after1 = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // chk_plus:
    patch_rel32(&mut code, jne_plus);
    code.extend_from_slice(&[0x3C, 0x2B]); // cmp al, '+'
    code.extend_from_slice(&[0x0F, 0x85]); // jne after_sign
    let jne_after = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xFF, 0xC7]); // inc rdi ('+' consumed)
    // after_sign:
    patch_rel32(&mut code, jmp_after1);
    patch_rel32(&mut code, jne_after);

    // Require at least one digit: if i >= count -> err (a lone sign or empty).
    code.extend_from_slice(&[0x48, 0x39, 0xF7]); // cmp rdi, rsi
    code.extend_from_slice(&[0x0F, 0x83]); // jae err
    err_sites.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);

    // Digit loop.
    let loop_top = code.len();
    code.extend_from_slice(&[0x48, 0x39, 0xF7]); // cmp rdi, rsi
    code.extend_from_slice(&[0x0F, 0x83]); // jae ok
    let ok_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // c = data[i]; digit-classify via one unsigned range test.
    code.extend_from_slice(&[0x0F, 0xB6, 0x84, 0x3B]); // movzx eax, byte [rbx + rdi + disp32]
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x2C, 0x30]); // sub al, '0'
    code.extend_from_slice(&[0x3C, 0x09]); // cmp al, 9
    code.extend_from_slice(&[0x0F, 0x87]); // ja err (not a digit)
    err_sites.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x0F, 0xB6, 0xC0]); // movzx eax, al (digit 0..9)
    // acc = acc * 10 (checked).
    code.extend_from_slice(&[0x4D, 0x6B, 0xD2, 0x0A]); // imul r10, r10, 10
    code.extend_from_slice(&[0x0F, 0x80]); // jo err
    err_sites.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);
    // Sign-directed accumulate.
    code.extend_from_slice(&[0x4D, 0x85, 0xDB]); // test r11, r11
    code.extend_from_slice(&[0x0F, 0x85]); // jnz neg
    let jnz_neg = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x49, 0x01, 0xC2]); // add r10, rax
    code.extend_from_slice(&[0x0F, 0x80]); // jo err
    err_sites.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.push(0xE9); // jmp after_acc
    let jmp_after_acc = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // neg:
    patch_rel32(&mut code, jnz_neg);
    code.extend_from_slice(&[0x49, 0x29, 0xC2]); // sub r10, rax
    code.extend_from_slice(&[0x0F, 0x80]); // jo err
    err_sites.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);
    // after_acc:
    patch_rel32(&mut code, jmp_after_acc);
    code.extend_from_slice(&[0x48, 0xFF, 0xC7]); // inc rdi
    emit_jmp_to(&mut code, loop_top); // jmp loop_top

    // ok: tag = 0 (rax), payload = acc (rdx).
    patch_rel32(&mut code, ok_site);
    code.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
    code.extend_from_slice(&[0x4C, 0x89, 0xD2]); // mov rdx, r10
    code.push(0xE9); // jmp done
    let jmp_done = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // err: build the `cannot parse `{text}` as i64` message record.
    let err_target = code.len();
    for site in &err_sites {
        patch_rel32_to(&mut code, *site, err_target);
    }
    // r12 = src byte_len; allocate STR_DATA_OFF + 22 + byte_len.
    code.extend_from_slice(&[0x4C, 0x8B, 0x63, 0x08]); // mov r12, [rbx + STR_BYTE_LEN_OFF]
    code.extend_from_slice(&[0x49, 0x8D, 0x8C, 0x24]); // lea rcx, [r12 + disp32]
    code.extend_from_slice(&(STR_DATA_OFF + 22).to_le_bytes());
    emit_helper_call_alloc(&mut code, &mut relocations); // rax = dst record
    code.extend_from_slice(&[0x49, 0x89, 0xC5]); // mov r13, rax (preserve dst)
    // Headers: char_len = src char_len + 22, byte_len = src byte_len + 22.
    code.extend_from_slice(&[0x48, 0x8B, 0x53, 0x00]); // mov rdx, [rbx + STR_CHAR_LEN_OFF]
    code.extend_from_slice(&[0x48, 0x83, 0xC2, 0x16]); // add rdx, 22
    code.extend_from_slice(&[0x48, 0x89, 0x50, 0x00]); // mov [rax + STR_CHAR_LEN_OFF], rdx
    code.extend_from_slice(&[0x49, 0x8D, 0x54, 0x24, 0x16]); // lea rdx, [r12 + 22]
    code.extend_from_slice(&[0x48, 0x89, 0x50, 0x08]); // mov [rax + STR_BYTE_LEN_OFF], rdx
    // Prefix "cannot parse `" (14 bytes) at [rax + STR_DATA_OFF].
    code.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
    code.extend_from_slice(&u64::from_le_bytes(*b"cannot p").to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0x48, 0x10]); // mov [rax + 16], rcx
    code.push(0xB9); // mov ecx, imm32
    code.extend_from_slice(&u32::from_le_bytes(*b"arse").to_le_bytes());
    code.extend_from_slice(&[0x89, 0x48, 0x18]); // mov [rax + 24], ecx
    code.extend_from_slice(&[0x66, 0xB9]); // mov cx, imm16
    code.extend_from_slice(&u16::from_le_bytes(*b" `").to_le_bytes());
    code.extend_from_slice(&[0x66, 0x89, 0x48, 0x1C]); // mov [rax + 28], cx
    // Copy the source bytes: rsi = src data, rdi = dst data + 14, rcx = byte_len.
    code.extend_from_slice(&[0x48, 0x8D, 0x73, 0x10]); // lea rsi, [rbx + 16]
    code.extend_from_slice(&[0x48, 0x8D, 0x78, 0x1E]); // lea rdi, [rax + 30]
    code.extend_from_slice(&[0x4C, 0x89, 0xE1]); // mov rcx, r12
    code.extend_from_slice(&[0xF3, 0xA4]); // rep movsb
    // Suffix "` as i64" (8 bytes) at [rdi] (rdi is one past the copied source).
    code.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
    code.extend_from_slice(&u64::from_le_bytes(*b"` as i64").to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0x0F]); // mov [rdi], rcx
    // tag = 1 (err); payload = dst record.
    code.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // mov eax, 1
    code.extend_from_slice(&[0x4C, 0x89, 0xEA]); // mov rdx, r13

    // done: epilogue.
    patch_rel32(&mut code, jmp_done);
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x20]); // add rsp, 32
    code.extend_from_slice(&[0x41, 0x5D]); // pop r13
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0x5B); // pop rbx
    code.push(0xC3); // ret

    HelperFunction {
        name: PARSE_I64_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// Emit `call <symbol>` (rel32) inside a runtime helper, recording the relocation
/// against `relocations`. Generalizes [`emit_helper_call_alloc`] to any helper
/// symbol so a helper can compose other `.text` helpers.
pub(crate) fn emit_helper_call(
    code: &mut Vec<u8>,
    relocations: &mut Vec<CodeRelocation>,
    symbol: &str,
) {
    code.push(0xE8);
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: symbol.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
}

/// `__lullaby_str_split(rcx = text, rdx = sep) -> rax = list<string> block`.
///
/// Builds a fresh `[len][cap][slot…]` block of the fields, exactly matching the
/// interpreters' `text.split(sep)`. Composed from the tested string helpers:
/// `__lullaby_str_count` sizes the field count (occurrences + 1); then a loop uses
/// `__lullaby_str_find`/`__lullaby_str_substring` to slice each field between
/// separators (advancing non-overlapping, so leading/trailing/consecutive
/// separators yield empty fields and an empty input yields one empty field). An
/// empty separator traps with `ud2` (the interpreters' `L0417`; a program that can
/// pass an empty separator must run on an interpreter). Char indices equal byte
/// offsets for the ASCII strings the native subset builds.
pub(crate) fn emit_str_split_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Prologue: 7 callee-saved pushes → rsp%16 == 0; `sub rsp, 0x30` keeps %16 == 0
    // at the internal calls and reserves 32 shadow + a 16-byte spill area.
    code.push(0x53); // push rbx
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x41, 0x55]); // push r13
    code.extend_from_slice(&[0x41, 0x56]); // push r14
    code.extend_from_slice(&[0x41, 0x57]); // push r15
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x30]); // sub rsp, 0x30

    code.extend_from_slice(&[0x49, 0x89, 0xCC]); // mov r12, rcx (text)
    code.extend_from_slice(&[0x49, 0x89, 0xD5]); // mov r13, rdx (sep)

    // Empty separator -> trap (L0417). if sep.char_len == 0: ud2.
    code.extend_from_slice(&[0x49, 0x8B, 0x45, 0x00]); // mov rax, [r13 + STR_CHAR_LEN_OFF]
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.extend_from_slice(&[0x75, 0x02]); // jnz sepok
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2

    // nfields (r14) = str_count(text, sep) + 1.
    code.extend_from_slice(&[0x4C, 0x89, 0xE1]); // mov rcx, r12
    code.extend_from_slice(&[0x4C, 0x89, 0xEA]); // mov rdx, r13
    emit_helper_call(&mut code, &mut relocations, STR_COUNT_SYMBOL); // rax = count
    code.extend_from_slice(&[0x4C, 0x8D, 0x70, 0x01]); // lea r14, [rax + 1]

    // Allocate the block: LIST_DATA_OFF + nfields*8. rcx = [r14*8 + LIST_DATA_OFF].
    code.extend_from_slice(&[0x4A, 0x8D, 0x0C, 0xF5]); // lea rcx, [r14*8 + disp32]
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    emit_helper_call(&mut code, &mut relocations, HEAP_ALLOC_SYMBOL); // rax = block
    code.extend_from_slice(&[0x48, 0x89, 0xC3]); // mov rbx, rax (block)
    code.extend_from_slice(&[0x4C, 0x89, 0x73, 0x00]); // mov [rbx + LIST_LEN_OFF], r14
    code.extend_from_slice(&[0x4C, 0x89, 0x73, 0x08]); // mov [rbx + LIST_CAP_OFF], r14

    // Fill loop. rsi = pos (char index), rdi = slot, r15 = text char_len.
    code.extend_from_slice(&[0x31, 0xF6]); // xor esi, esi
    code.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi
    code.extend_from_slice(&[0x4D, 0x8B, 0x7C, 0x24, 0x00]); // mov r15, [r12 + STR_CHAR_LEN_OFF]

    let loop_top = code.len();
    // rest = substring(text, pos, text_char_len).
    code.extend_from_slice(&[0x4C, 0x89, 0xE1]); // mov rcx, r12
    code.extend_from_slice(&[0x48, 0x89, 0xF2]); // mov rdx, rsi (pos)
    code.extend_from_slice(&[0x4D, 0x89, 0xF8]); // mov r8, r15 (end = char_len)
    emit_helper_call(&mut code, &mut relocations, STR_SUBSTRING_SYMBOL); // rax = rest
    code.extend_from_slice(&[0x49, 0x89, 0xC6]); // mov r14, rax (rest)
    // idx = find(rest, sep).
    code.extend_from_slice(&[0x4C, 0x89, 0xF1]); // mov rcx, r14
    code.extend_from_slice(&[0x4C, 0x89, 0xEA]); // mov rdx, r13
    emit_helper_call(&mut code, &mut relocations, STR_FIND_SYMBOL); // rax = idx or -1
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.extend_from_slice(&[0x0F, 0x88]); // js last (idx < 0 -> remaining is the final field)
    let last_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // field = substring(rest, 0, idx). Spill idx for the pos update after the call.
    code.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x20]); // mov [rsp+0x20], rax
    code.extend_from_slice(&[0x4C, 0x89, 0xF1]); // mov rcx, r14 (rest)
    code.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx (start = 0)
    code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (end = idx)
    emit_helper_call(&mut code, &mut relocations, STR_SUBSTRING_SYMBOL); // rax = field
    // block.slot[slot] = field.
    code.extend_from_slice(&[0x48, 0x89, 0x84, 0xFB]); // mov [rbx + rdi*8 + disp32], rax
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xFF, 0xC7]); // inc rdi
    // The `rest` slice (r14) was only needed to locate/extract this field; on the
    // non-last path it is a dead intermediate, so reclaim it (`rc_dec`) — otherwise
    // a `split` in a loop would orphan one `rest` record per field each iteration.
    code.extend_from_slice(&[0x4C, 0x89, 0xF1]); // mov rcx, r14 (rest)
    emit_helper_call(&mut code, &mut relocations, RC_DEC_SYMBOL);
    // pos += idx + sep.char_len.
    code.extend_from_slice(&[0x48, 0x8B, 0x44, 0x24, 0x20]); // mov rax, [rsp+0x20] (idx)
    code.extend_from_slice(&[0x48, 0x01, 0xC6]); // add rsi, rax
    code.extend_from_slice(&[0x49, 0x03, 0x75, 0x00]); // add rsi, [r13 + STR_CHAR_LEN_OFF]
    emit_jmp_to(&mut code, loop_top); // jmp loop_top

    // last: block.slot[slot] = rest (the remaining suffix is the final field).
    patch_rel32(&mut code, last_site);
    code.extend_from_slice(&[0x4C, 0x89, 0xB4, 0xFB]); // mov [rbx + rdi*8 + disp32], r14
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0xD8]); // mov rax, rbx (return block)

    // Epilogue.
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x30]); // add rsp, 0x30
    code.extend_from_slice(&[0x41, 0x5F]); // pop r15
    code.extend_from_slice(&[0x41, 0x5E]); // pop r14
    code.extend_from_slice(&[0x41, 0x5D]); // pop r13
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0x5B); // pop rbx
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_SPLIT_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// `__lullaby_str_join(rcx = array<string> block, rdx = sep) -> rax = record`.
///
/// Joins the block's fields with the separator between them, matching the
/// interpreters' `parts.join(sep)`. Built by chaining the tested
/// `__lullaby_str_concat` (`acc = concat(concat(acc, sep), field)`), so the final
/// record's bytes/headers are exactly a direct join. An empty array yields a fresh
/// empty record.
pub(crate) fn emit_str_join_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Prologue: 5 callee-saved pushes → rsp%16 == 0; `sub rsp, 0x20` (shadow).
    code.push(0x53); // push rbx
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x41, 0x55]); // push r13
    code.extend_from_slice(&[0x41, 0x56]); // push r14
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x20]); // sub rsp, 0x20

    code.extend_from_slice(&[0x49, 0x89, 0xCC]); // mov r12, rcx (block)
    code.extend_from_slice(&[0x49, 0x89, 0xD5]); // mov r13, rdx (sep)
    code.extend_from_slice(&[0x4D, 0x8B, 0x74, 0x24, 0x00]); // mov r14, [r12 + LIST_LEN_OFF]

    // Empty array -> fresh empty record.
    code.extend_from_slice(&[0x4D, 0x85, 0xF6]); // test r14, r14
    code.extend_from_slice(&[0x0F, 0x85]); // jnz nonempty
    let nonempty_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0xB9, 0x10, 0x00, 0x00, 0x00]); // mov ecx, STR_DATA_OFF (16)
    emit_helper_call(&mut code, &mut relocations, HEAP_ALLOC_SYMBOL); // rax = rec
    code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx
    code.extend_from_slice(&[0x48, 0x89, 0x50, 0x00]); // mov [rax + STR_CHAR_LEN_OFF], rdx
    code.extend_from_slice(&[0x48, 0x89, 0x50, 0x08]); // mov [rax + STR_BYTE_LEN_OFF], rdx
    code.push(0xE9); // jmp done
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // nonempty: acc (rbx) = fields[0]; i (rdi) = 1.
    patch_rel32(&mut code, nonempty_site);
    code.extend_from_slice(&[0x49, 0x8B, 0x9C, 0x24]); // mov rbx, [r12 + disp32] (field 0)
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0xBF, 0x01, 0x00, 0x00, 0x00]); // mov edi, 1

    let loop_top = code.len();
    code.extend_from_slice(&[0x4C, 0x39, 0xF7]); // cmp rdi, r14
    code.extend_from_slice(&[0x0F, 0x83]); // jae ret_acc
    let ret_acc_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // acc = concat(acc, sep).
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx (acc)
    code.extend_from_slice(&[0x4C, 0x89, 0xEA]); // mov rdx, r13 (sep)
    emit_helper_call(&mut code, &mut relocations, STR_CONCAT_SYMBOL); // rax = acc+sep
    code.extend_from_slice(&[0x48, 0x89, 0xC3]); // mov rbx, rax
    // acc = concat(acc, fields[i]).
    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx
    code.extend_from_slice(&[0x49, 0x8B, 0x94, 0xFC]); // mov rdx, [r12 + rdi*8 + disp32]
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    emit_helper_call(&mut code, &mut relocations, STR_CONCAT_SYMBOL); // rax = acc+field
    code.extend_from_slice(&[0x48, 0x89, 0xC3]); // mov rbx, rax
    code.extend_from_slice(&[0x48, 0xFF, 0xC7]); // inc rdi
    emit_jmp_to(&mut code, loop_top); // jmp loop_top

    // ret_acc: return acc.
    patch_rel32(&mut code, ret_acc_site);
    code.extend_from_slice(&[0x48, 0x89, 0xD8]); // mov rax, rbx

    // done: epilogue.
    patch_rel32(&mut code, done_site);
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x20]); // add rsp, 0x20
    code.extend_from_slice(&[0x41, 0x5E]); // pop r14
    code.extend_from_slice(&[0x41, 0x5D]); // pop r13
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5F); // pop rdi
    code.push(0x5B); // pop rbx
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_JOIN_SYMBOL.to_string(),
        code,
        relocations,
    }
}

/// Emit the char-index-to-byte walk. Reads the target char index in `rax`, the
/// data pointer in `rsi`, and the byte length in `r15`; advances a byte offset
/// past exactly `target` whole UTF-8 characters and leaves that byte offset in
/// `rax`. Each step moves past one lead byte then over all continuation bytes
/// (`(b & 0xC0) == 0x80`). For `target == char_count` this lands on `byte_len`.
/// The caller's bounds check guarantees `target <= char_count`, so the walk stays
/// in range. Clobbers `rax`, `rcx`, `rdx`, `r9`.
pub(crate) fn emit_char_to_byte_walk(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x49, 0x89, 0xC1]); // mov r9, rax (target, saved)
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax (bi = 0)
    code.extend_from_slice(&[0x48, 0x31, 0xC9]); // xor rcx, rcx (c = 0)
    let wouter = code.len();
    code.extend_from_slice(&[0x4C, 0x39, 0xC9]); // cmp rcx, r9
    code.extend_from_slice(&[0x0F, 0x8D]); // jge wdone (c >= target; rel32)
    let wdone_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax (bi += 1, past lead byte)
    let winner = code.len();
    code.extend_from_slice(&[0x4C, 0x39, 0xF8]); // cmp rax, r15
    code.extend_from_slice(&[0x0F, 0x8D]); // jge wccont (bi >= byte_len; rel32)
    let winner_break_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x8A, 0x14, 0x06]); // mov dl, [rsi + rax]
    code.extend_from_slice(&[0x80, 0xE2, 0xC0]); // and dl, 0xC0
    code.extend_from_slice(&[0x80, 0xFA, 0x80]); // cmp dl, 0x80
    code.extend_from_slice(&[0x0F, 0x85]); // jne wccont (not continuation; rel32)
    let winner_break2_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax (bi += 1)
    emit_jmp_to(code, winner); // jmp winner
    // wccont: c += 1; continue outer.
    patch_rel32(code, winner_break_site);
    patch_rel32(code, winner_break2_site);
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]); // inc rcx (c += 1)
    emit_jmp_to(code, wouter); // jmp wouter
    // wdone: bi in rax.
    patch_rel32(code, wdone_site);
}

/// `__lullaby_str_char_at(rcx = s, rdx = i) -> rax = code point`.
///
/// Returns the Unicode scalar of the `i`-th character. Bounds-checks `i` against
/// `char_count` (unsigned, so `i < 0` traps too) with `ud2` (mirroring `L0413`),
/// walks the UTF-8 to char `i`'s byte offset, then decodes the 1–4-byte sequence
/// there into its code point. Preserves the two callee-saved registers the walk
/// uses (`rsi`, `r15`); makes no internal call.
pub(crate) fn emit_str_char_at_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    code.push(0x56); // push rsi
    code.extend_from_slice(&[0x41, 0x57]); // push r15

    // Bounds check: unsigned i >= char_len -> ud2.
    code.extend_from_slice(&[0x48, 0x8B, 0x41, 0x00]); // mov rax, [rcx + 0] (char_len)
    code.extend_from_slice(&[0x48, 0x39, 0xC2]); // cmp rdx, rax
    code.extend_from_slice(&[0x72, 0x02]); // jb +2 (in bounds)
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2

    // Set up the walk: rsi = data (rcx+16), r15 = byte_len (rcx+8), rax = i.
    code.extend_from_slice(&[0x4C, 0x8B, 0x79, 0x08]); // mov r15, [rcx + 8]
    code.extend_from_slice(&[0x48, 0x8D, 0x71, 0x10]); // lea rsi, [rcx + 16]
    code.extend_from_slice(&[0x48, 0x89, 0xD0]); // mov rax, rdx (i)
    emit_char_to_byte_walk(&mut code); // rax = byte offset of char i

    // Decode the UTF-8 sequence at [rsi + rax] into r8 (the code point).
    code.extend_from_slice(&[0x44, 0x0F, 0xB6, 0x04, 0x06]); // movzx r8d, byte [rsi+rax]  (lead)
    // 1-byte: lead < 0x80 -> cp = lead.
    code.extend_from_slice(&[0x41, 0x81, 0xF8]);
    code.extend_from_slice(&0x80u32.to_le_bytes()); // cmp r8d, 0x80
    code.extend_from_slice(&[0x0F, 0x82]); // jb done
    let done1 = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // b1 = [rsi+rax+1] & 0x3F.
    code.extend_from_slice(&[0x44, 0x0F, 0xB6, 0x4C, 0x06, 0x01]); // movzx r9d, byte [rsi+rax+1]
    code.extend_from_slice(&[0x41, 0x83, 0xE1, 0x3F]); // and r9d, 0x3F
    // 2-byte: lead < 0xE0 -> cp = ((lead & 0x1F) << 6) | b1.
    code.extend_from_slice(&[0x41, 0x81, 0xF8]);
    code.extend_from_slice(&0xE0u32.to_le_bytes()); // cmp r8d, 0xE0
    code.extend_from_slice(&[0x0F, 0x83]); // jae three_plus
    let three_plus = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x41, 0x83, 0xE0, 0x1F]); // and r8d, 0x1F
    code.extend_from_slice(&[0x41, 0xC1, 0xE0, 0x06]); // shl r8d, 6
    code.extend_from_slice(&[0x45, 0x09, 0xC8]); // or r8d, r9d
    code.push(0xE9); // jmp done
    let done2 = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // three_plus: b2 = [rsi+rax+2] & 0x3F.
    patch_rel32(&mut code, three_plus);
    code.extend_from_slice(&[0x44, 0x0F, 0xB6, 0x54, 0x06, 0x02]); // movzx r10d, byte [rsi+rax+2]
    code.extend_from_slice(&[0x41, 0x83, 0xE2, 0x3F]); // and r10d, 0x3F
    // 3-byte: lead < 0xF0 -> cp = ((lead & 0x0F) << 12) | (b1 << 6) | b2.
    code.extend_from_slice(&[0x41, 0x81, 0xF8]);
    code.extend_from_slice(&0xF0u32.to_le_bytes()); // cmp r8d, 0xF0
    code.extend_from_slice(&[0x0F, 0x83]); // jae four
    let four = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x41, 0x83, 0xE0, 0x0F]); // and r8d, 0x0F
    code.extend_from_slice(&[0x41, 0xC1, 0xE0, 0x0C]); // shl r8d, 12
    code.extend_from_slice(&[0x41, 0xC1, 0xE1, 0x06]); // shl r9d, 6
    code.extend_from_slice(&[0x45, 0x09, 0xC8]); // or r8d, r9d
    code.extend_from_slice(&[0x45, 0x09, 0xD0]); // or r8d, r10d
    code.push(0xE9); // jmp done
    let done3 = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // four: b3 = [rsi+rax+3] & 0x3F ; cp = ((lead & 0x07)<<18)|(b1<<12)|(b2<<6)|b3.
    patch_rel32(&mut code, four);
    code.extend_from_slice(&[0x44, 0x0F, 0xB6, 0x5C, 0x06, 0x03]); // movzx r11d, byte [rsi+rax+3]
    code.extend_from_slice(&[0x41, 0x83, 0xE3, 0x3F]); // and r11d, 0x3F
    code.extend_from_slice(&[0x41, 0x83, 0xE0, 0x07]); // and r8d, 0x07
    code.extend_from_slice(&[0x41, 0xC1, 0xE0, 0x12]); // shl r8d, 18
    code.extend_from_slice(&[0x41, 0xC1, 0xE1, 0x0C]); // shl r9d, 12
    code.extend_from_slice(&[0x41, 0xC1, 0xE2, 0x06]); // shl r10d, 6
    code.extend_from_slice(&[0x45, 0x09, 0xC8]); // or r8d, r9d
    code.extend_from_slice(&[0x45, 0x09, 0xD0]); // or r8d, r10d
    code.extend_from_slice(&[0x45, 0x09, 0xD8]); // or r8d, r11d
    // done: rax = r8 (code point).
    patch_rel32(&mut code, done1);
    patch_rel32(&mut code, done2);
    patch_rel32(&mut code, done3);
    code.extend_from_slice(&[0x4C, 0x89, 0xC0]); // mov rax, r8
    code.extend_from_slice(&[0x41, 0x5F]); // pop r15
    code.push(0x5E); // pop rsi
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_CHAR_AT_SYMBOL.to_string(),
        code,
        relocations: Vec::new(),
    }
}

/// `__lullaby_str_substring(rcx = s, rdx = start, r8 = end) -> rax = record`.
///
/// Char-indexed `[start, end)` slice. Bounds-checks exactly like the interpreters
/// (`start < 0 || end < 0 || start > end || end > char_count`) and traps (`ud2`,
/// mirroring `L0413`) on a violation. Otherwise maps the char indices to byte
/// offsets by walking the UTF-8, allocates a fresh `[char_len][byte_len][utf8]`
/// record, writes the sliced headers, and byte-copies the slice. Uses eight
/// callee-saved registers across the internal `__lullaby_alloc` call and keeps
/// `rsp` 16-byte aligned at that call.
pub(crate) fn emit_str_substring_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // Prologue: preserve the eight callee-saved regs we hold across the alloc call;
    // 8 pushes keep rsp%16 == 8 (return addr makes the count even), then `sub rsp,8`
    // → %16 == 0 at the internal `call`.
    //   rsi = data, r15 = byte_len, rbx = start_byte, rbp = end_byte,
    //   r12 = start_char, r13 = end_char, r14 = dst record.
    code.push(0x53); // push rbx
    code.push(0x55); // push rbp
    code.push(0x56); // push rsi
    code.push(0x57); // push rdi
    code.extend_from_slice(&[0x41, 0x54]); // push r12
    code.extend_from_slice(&[0x41, 0x55]); // push r13
    code.extend_from_slice(&[0x41, 0x56]); // push r14
    code.extend_from_slice(&[0x41, 0x57]); // push r15
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]); // sub rsp, 8

    // Bounds check against char_count = [rcx + CHAR_LEN]. r9 = char_count.
    code.extend_from_slice(&[0x4C, 0x8B, 0x49, 0x00]); // mov r9, [rcx + 0]
    // start < 0 -> trap.
    code.extend_from_slice(&[0x48, 0x85, 0xD2]); // test rdx, rdx
    code.extend_from_slice(&[0x0F, 0x88]); // js trap (start < 0; rel32)
    let trap1_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // end < 0 -> trap.
    code.extend_from_slice(&[0x4D, 0x85, 0xC0]); // test r8, r8
    code.extend_from_slice(&[0x0F, 0x88]); // js trap (end < 0; rel32)
    let trap2_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // start > end -> trap.
    code.extend_from_slice(&[0x49, 0x39, 0xD0]); // cmp r8, rdx  (r8 - rdx)
    code.extend_from_slice(&[0x0F, 0x8C]); // jl trap (r8 < rdx, i.e. start > end; rel32)
    let trap3_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // end > char_count -> trap.
    code.extend_from_slice(&[0x4D, 0x39, 0xC8]); // cmp r8, r9  (r8 - r9)
    code.extend_from_slice(&[0x0F, 0x8F]); // jg trap (end > char_count; rel32)
    let trap4_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // In-bounds path. Save start_char/end_char and the data/byte_len.
    code.extend_from_slice(&[0x49, 0x89, 0xD4]); // mov r12, rdx (start_char)
    code.extend_from_slice(&[0x4D, 0x89, 0xC5]); // mov r13, r8 (end_char)
    code.extend_from_slice(&[0x4C, 0x8B, 0x79, 0x08]); // mov r15, [rcx + 8] (byte_len)
    code.extend_from_slice(&[0x48, 0x8D, 0x71, 0x10]); // lea rsi, [rcx + 16] (data)

    // start_byte = walk(start_char); end_byte = walk(end_char).
    code.extend_from_slice(&[0x4C, 0x89, 0xE0]); // mov rax, r12 (start_char)
    emit_char_to_byte_walk(&mut code); // rax = start_byte
    code.extend_from_slice(&[0x48, 0x89, 0xC3]); // mov rbx, rax (start_byte)
    code.extend_from_slice(&[0x4C, 0x89, 0xE8]); // mov rax, r13 (end_char)
    emit_char_to_byte_walk(&mut code); // rax = end_byte
    code.extend_from_slice(&[0x48, 0x89, 0xC5]); // mov rbp, rax (end_byte)

    // Allocate STR_DATA_OFF + slice_bytes. rcx = (rbp - rbx) + STR_DATA_OFF.
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x29, 0xD9]); // sub rcx, rbx (slice_bytes)
    code.extend_from_slice(&[0x48, 0x81, 0xC1]); // add rcx, imm32
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    emit_helper_call_alloc(&mut code, &mut relocations); // rax = dst
    code.extend_from_slice(&[0x49, 0x89, 0xC6]); // mov r14, rax (record base)

    // char_len = end_char - start_char (r13 - r12).
    code.extend_from_slice(&[0x4C, 0x89, 0xE9]); // mov rcx, r13
    code.extend_from_slice(&[0x4C, 0x29, 0xE1]); // sub rcx, r12
    code.extend_from_slice(&[0x49, 0x89, 0x8E]); // mov [r14 + CHAR_LEN], rcx
    code.extend_from_slice(&STR_CHAR_LEN_OFF.to_le_bytes());
    // byte_len = slice_bytes = rbp - rbx.
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x29, 0xD9]); // sub rcx, rbx
    code.extend_from_slice(&[0x49, 0x89, 0x8E]); // mov [r14 + BYTE_LEN], rcx
    code.extend_from_slice(&STR_BYTE_LEN_OFF.to_le_bytes());

    // Copy slice_bytes from data + start_byte to r14 + DATA.
    code.extend_from_slice(&[0x49, 0x8D, 0xBE]); // lea rdi, [r14 + disp32] (dest)
    code.extend_from_slice(&STR_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x01, 0xDE]); // add rsi, rbx (src = data + start_byte)
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x29, 0xD9]); // sub rcx, rbx (count)
    code.extend_from_slice(&[0xF3, 0xA4]); // rep movsb

    // rax = r14 (record base) — return value.
    code.extend_from_slice(&[0x4C, 0x89, 0xF0]); // mov rax, r14
    code.extend_from_slice(&[0xE9]); // jmp epilogue (rel32)
    let epilogue_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // trap: out-of-bounds range (mirrors the interpreters' L0413).
    patch_rel32(&mut code, trap1_site);
    patch_rel32(&mut code, trap2_site);
    patch_rel32(&mut code, trap3_site);
    patch_rel32(&mut code, trap4_site);
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2

    // epilogue:
    patch_rel32(&mut code, epilogue_site);
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]); // add rsp, 8
    code.extend_from_slice(&[0x41, 0x5F]); // pop r15
    code.extend_from_slice(&[0x41, 0x5E]); // pop r14
    code.extend_from_slice(&[0x41, 0x5D]); // pop r13
    code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    code.push(0x5F); // pop rdi
    code.push(0x5E); // pop rsi
    code.push(0x5D); // pop rbp
    code.push(0x5B); // pop rbx
    code.push(0xC3); // ret

    HelperFunction {
        name: STR_SUBSTRING_SYMBOL.to_string(),
        code,
        relocations,
    }
}
