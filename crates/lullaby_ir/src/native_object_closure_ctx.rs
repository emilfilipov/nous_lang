//! Closure-support plumbing that connects the native program driver and
//! [`NativeCtx`] to the closure codegen in `native_object_closure.rs`:
//!
//! - closure-aware frame/local planning ([`collect_native_locals`]), which — in
//!   addition to sizing every `let`/`for`/`match`/etc. local — classifies a
//!   `let f fn(...) = fn ... ` binding as a Stage-1 closure local,
//! - the [`NativeCtx`] constructor for a synthesized closure BODY
//!   ([`NativeCtx::for_closure_body`]) and its relocation accessor
//!   ([`NativeCtx::take_relocations`]), and
//! - the program-level orchestration loop that synthesizes exactly the closure
//!   bodies the compiled functions reference
//!   ([`synthesize_referenced_closure_bodies`]).
//!
//! Split out of `native_object.rs`; as a descendant module it sees the parent's
//! items — including [`NativeCtx`]'s and [`NativeLocal`]'s private fields — via
//! `use super::*`.

use super::*;

impl<'a> NativeCtx<'a> {
    /// Build a `NativeCtx` for lowering a synthesized closure BODY (`__closure_{id}`).
    /// Unlike [`plan`](NativeCtx::plan), the frame and env binding are computed by the
    /// closure synthesizer (`synthesize_closure_body`); this constructor only seats the
    /// pre-planned locals, env binding, and the shared module state, with every
    /// arena / register-promotion feature off (a closure body is a scalar leaf).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_closure_body(
        locals: HashMap<String, NativeLocal>,
        frame_size: i32,
        scratch_base: i32,
        closure_env: ClosureEnv,
        callable: &'a std::collections::HashSet<&'a str>,
        extern_sigs: &'a HashMap<&'a str, &'a crate::IrExternSignature>,
        structs: &'a [IrStructDef],
        enums: &'a [IrEnumDef],
        strings: &'a mut StringPool,
        signatures: &'a HashMap<String, NativeSignature>,
        closure_layouts: &'a HashMap<usize, ClosureLayout>,
    ) -> Self {
        Self {
            locals,
            frame_size,
            callable,
            extern_sigs,
            relocations: Vec::new(),
            strings,
            structs,
            enums,
            scratch_next: scratch_base + 8,
            sret_slot: None,
            return_ty: NativeType::I64,
            signatures,
            promoted: HashMap::new(),
            saved_reg_slots: Vec::new(),
            fast_math: false,
            is_arena: false,
            arena_mark_slot: 0,
            arena_loop_mark_base: 0,
            heap_aggregates: std::collections::HashSet::new(),
            closure_locals: HashMap::new(),
            closure_layouts,
            closure_env: Some(closure_env),
        }
    }

    /// Take the relocations accumulated while lowering (used by the closure-body
    /// synthesizer to build its `LoweredNativeFunction`).
    pub(crate) fn take_relocations(&mut self) -> Vec<CodeRelocation> {
        std::mem::take(&mut self.relocations)
    }
}

/// Recursively collect `let`/`for` locals, assigning each contiguous 8-byte-word
/// slots sized by its `NativeType`. `let` locals with an aggregate layout reserve
/// one word per flattened scalar; `for` counters and their hidden bound/step are
/// single i64 words.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_native_locals(
    body: &[BytecodeInstruction],
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    signatures: &HashMap<String, NativeSignature>,
    locals: &mut HashMap<String, NativeLocal>,
    next_slot: &mut i32,
    closure_layouts: &HashMap<usize, ClosureLayout>,
    closure_locals: &mut HashMap<String, usize>,
) -> Result<(), String> {
    for instruction in body {
        match instruction {
            BytecodeInstruction::Let {
                name, ty, value, ..
            } => {
                if !locals.contains_key(name) {
                    // A closure-bound local (`let f fn(...) = fn x -> …`) is a single
                    // pointer word to the closure's `[code_ptr][captures…]` heap
                    // block. Its declared `fn(...)` type is not resolvable by
                    // `resolve_native_type` (that path rejects function types so a
                    // closure-as-value elsewhere skips), so classify it here from the
                    // initializer: a DIRECT `Closure { id }` literal with a Stage-1
                    // layout is a supported pointer word; anything else (a closure
                    // returned from a call, an unsupported closure shape) is rejected
                    // so the function skips cleanly.
                    if ty.is_function() {
                        let BytecodeExprKind::Closure { id } = &value.kind else {
                            return Err(format!(
                                "closure local `{name}` is not bound to a direct closure literal; \
                                 native closures are Stage-1 (direct literal, scalar captures)"
                            ));
                        };
                        if !closure_layouts.contains_key(id) {
                            return Err(format!(
                                "closure #{id} bound to `{name}` is not in the native Stage-1 \
                                 subset (non-scalar capture/param/return, or >3 params)"
                            ));
                        }
                        closure_locals.insert(name.clone(), *id);
                        *next_slot += 8;
                        locals.insert(
                            name.clone(),
                            NativeLocal {
                                slot: *next_slot,
                                ty: NativeType::I64,
                            },
                        );
                        continue;
                    }
                    let native = if ty.name.starts_with("array<") {
                        native_type_of_init(value, structs, enums, signatures)?
                    } else {
                        resolve_native_type(ty, structs, enums)?
                    };
                    let words = native.words() as i32;
                    *next_slot += words * 8;
                    // ASCENDING layout: word 0 sits at the TOP of the reserved
                    // displacement range (the aggregate's lowest address).
                    locals.insert(
                        name.clone(),
                        NativeLocal {
                            slot: *next_slot,
                            ty: native,
                        },
                    );
                }
            }
            BytecodeInstruction::For { name, body, .. } => {
                locals.entry(name.clone()).or_insert_with(|| {
                    *next_slot += 8;
                    NativeLocal {
                        slot: *next_slot,
                        ty: NativeType::I64,
                    }
                });
                // Two hidden slots per `for`: the loop bound and the step. Keyed
                // by the counter name so `lower_native_for` finds the same slots.
                for suffix in ["__end", "__step"] {
                    let key = format!("{name}{suffix}");
                    locals.entry(key).or_insert_with(|| {
                        *next_slot += 8;
                        NativeLocal {
                            slot: *next_slot,
                            ty: NativeType::I64,
                        }
                    });
                }
                collect_native_locals(
                    body,
                    structs,
                    enums,
                    signatures,
                    locals,
                    next_slot,
                    closure_layouts,
                    closure_locals,
                )?;
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    collect_native_locals(
                        &branch.body,
                        structs,
                        enums,
                        signatures,
                        locals,
                        next_slot,
                        closure_layouts,
                        closure_locals,
                    )?;
                }
                collect_native_locals(
                    else_body,
                    structs,
                    enums,
                    signatures,
                    locals,
                    next_slot,
                    closure_layouts,
                    closure_locals,
                )?;
            }
            BytecodeInstruction::While { body, .. } | BytecodeInstruction::Loop { body, .. } => {
                collect_native_locals(
                    body,
                    structs,
                    enums,
                    signatures,
                    locals,
                    next_slot,
                    closure_layouts,
                    closure_locals,
                )?;
            }
            BytecodeInstruction::Match {
                scrutinee, arms, ..
            } => {
                // Each variant-pattern binding becomes a distinct local sized by
                // the matched variant's payload word at that position. The
                // scrutinee's static enum layout supplies the per-variant payload
                // types; a non-enum or heap-payload scrutinee errors out here so
                // the function skips gracefully.
                let layout = resolve_native_type(&scrutinee.ty, structs, enums)?;
                let NativeType::Enum { variants, .. } = &layout else {
                    return Err("match scrutinee is not a native enum layout".to_string());
                };
                for arm in arms {
                    if let BytecodeMatchPattern::Variant { name, bindings } = &arm.pattern {
                        let variant = variants
                            .iter()
                            .find(|v| &v.name == name)
                            .ok_or_else(|| format!("match arm names unknown variant `{name}`"))?;
                        if bindings.len() > variant.payload.len() {
                            return Err(format!(
                                "variant `{name}` has {} payload field(s) but the pattern binds {}",
                                variant.payload.len(),
                                bindings.len()
                            ));
                        }
                        for (binding, payload_ty) in bindings.iter().zip(variant.payload.iter()) {
                            if !locals.contains_key(binding) {
                                // A `HeapStruct` payload binds a STACK `Struct` local
                                // (bridged from the heap pointer at bind time), so the
                                // arm body's field access and by-pointer call ABI see
                                // the flat stack layout. It therefore reserves the
                                // struct's field words, not a single pointer word.
                                let bound_ty = match payload_ty {
                                    NativeType::HeapStruct { name, fields } => NativeType::Struct {
                                        name: name.clone(),
                                        fields: fields.clone(),
                                    },
                                    other => other.clone(),
                                };
                                let words = bound_ty.words() as i32;
                                *next_slot += words * 8;
                                // ASCENDING layout: word 0 at the top of the range.
                                locals.insert(
                                    binding.clone(),
                                    NativeLocal {
                                        slot: *next_slot,
                                        ty: bound_ty,
                                    },
                                );
                            }
                        }
                    }
                    collect_native_locals(
                        &arm.body,
                        structs,
                        enums,
                        signatures,
                        locals,
                        next_slot,
                        closure_layouts,
                        closure_locals,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Synthesize a native `.text` body (`__closure_{id}`) for each closure the
/// compiled functions reference, in deterministic order. A synthesis failure (a
/// body outside the Stage-1 subset — heap touch, user call, or otherwise
/// non-lowerable) returns the enclosing function as a [`NativeSkippedFunction`] so
/// the driver demotes it and re-runs the fixpoint, exactly like a top-level
/// lowering failure: the enclosing function then skips to the interpreters rather
/// than emitting a dangling `lea __closure_{id}` relocation. On success the bodies
/// are returned so the driver appends them to `lowered`, emitting them as ordinary
/// `.text` symbols so the enclosing `lea`/`call` resolves.
#[allow(clippy::too_many_arguments)]
pub(crate) fn synthesize_referenced_closure_bodies(
    eligible_names: &[String],
    module: &BytecodeModule,
    callable: &std::collections::HashSet<&str>,
    extern_sigs: &HashMap<&str, &crate::IrExternSignature>,
    strings: &mut StringPool,
    signatures: &HashMap<String, NativeSignature>,
    closure_layouts: &HashMap<usize, ClosureLayout>,
) -> Result<Vec<LoweredNativeFunction>, NativeSkippedFunction> {
    let mut closure_bodies: Vec<LoweredNativeFunction> = Vec::new();
    for name in eligible_names {
        let function = module
            .functions
            .iter()
            .find(|f| &f.name == name)
            .expect("eligible name exists");
        for id in referenced_closure_ids(function) {
            if closure_bodies.iter().any(|f| f.name == closure_symbol(id)) {
                continue;
            }
            let def = module.closures.iter().find(|c| c.id == id);
            let layout = closure_layouts.get(&id);
            let result = match (def, layout) {
                (Some(def), Some(layout)) => synthesize_closure_body(
                    def,
                    layout,
                    callable,
                    extern_sigs,
                    &module.structs,
                    &module.enums,
                    strings,
                    signatures,
                    closure_layouts,
                ),
                _ => Err(format!(
                    "closure #{id} referenced by `{name}` has no native body/layout"
                )),
            };
            match result {
                Ok(body) => closure_bodies.push(body),
                Err(reason) => {
                    return Err(NativeSkippedFunction {
                        name: name.clone(),
                        reason,
                    });
                }
            }
        }
    }
    Ok(closure_bodies)
}
