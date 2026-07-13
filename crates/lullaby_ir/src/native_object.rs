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
    BytecodeExpr, BytecodeExprKind, BytecodeFunction, BytecodeIfBranch, BytecodeInstruction,
    BytecodeMatchArm, BytecodeMatchPattern, BytecodeModule, BytecodePlace, IntKind, IrEnumDef,
    IrStructDef,
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

const AMD64_MACHINE: u16 = 0x8664;
const COFF_HEADER_SIZE: u32 = 20;
const SECTION_HEADER_SIZE: u32 = 40;
const SYMBOL_RECORD_SIZE: u32 = 18;
const TEXT_CHARACTERISTICS: u32 = 0x6050_0020;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeObjectFile {
    pub target: NativeTarget,
    pub format: NativeObjectFormat,
    pub entry_symbol: String,
    pub sections: Vec<NativeObjectSection>,
    pub symbols: Vec<NativeObjectSymbol>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeObjectSection {
    pub name: String,
    pub offset: u32,
    pub size: u32,
    pub characteristics: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeObjectSymbol {
    pub name: String,
    pub section: String,
    pub offset: u32,
    pub storage_class: NativeSymbolStorageClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeSymbolStorageClass {
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeObjectSnapshot {
    pub target_triple: String,
    pub object_format: NativeObjectFormat,
    pub entry_symbol: String,
    pub sections: Vec<NativeObjectSection>,
    pub symbols: Vec<NativeObjectSymbol>,
    pub bytes_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeObjectError {
    MissingEntry { entry: String },
    UnsupportedSignature { function: String, reason: String },
    UnsupportedBody { function: String, reason: String },
    UnsupportedSymbol { symbol: String, reason: String },
}

impl fmt::Display for NativeObjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NativeObjectError::MissingEntry { entry } => {
                write!(
                    formatter,
                    "native object entry function `{entry}` was not found"
                )
            }
            NativeObjectError::UnsupportedSignature { function, reason } => {
                write!(
                    formatter,
                    "unsupported native signature for `{function}`: {reason}"
                )
            }
            NativeObjectError::UnsupportedBody { function, reason } => {
                write!(
                    formatter,
                    "unsupported native body for `{function}`: {reason}"
                )
            }
            NativeObjectError::UnsupportedSymbol { symbol, reason } => {
                write!(formatter, "unsupported native symbol `{symbol}`: {reason}")
            }
        }
    }
}

impl std::error::Error for NativeObjectError {}

pub fn emit_coff_object(module: &BytecodeModule) -> Result<NativeObjectFile, NativeObjectError> {
    let contract = native_backend_contract();
    let entry_symbol = contract.calling_convention.entry_function;
    let target = contract.first_target;
    let function = module
        .functions
        .iter()
        .find(|function| function.name == entry_symbol)
        .ok_or_else(|| NativeObjectError::MissingEntry {
            entry: entry_symbol.clone(),
        })?;

    validate_entry_signature(function)?;
    let text = lower_entry_function_to_x86_64(function)?;
    let bytes = write_x86_64_coff_object(&entry_symbol, &text)?;

    Ok(NativeObjectFile {
        target,
        format: NativeObjectFormat::Coff,
        entry_symbol: entry_symbol.clone(),
        sections: vec![NativeObjectSection {
            name: ".text".to_string(),
            offset: COFF_HEADER_SIZE + SECTION_HEADER_SIZE,
            size: text.len() as u32,
            characteristics: TEXT_CHARACTERISTICS,
        }],
        symbols: vec![NativeObjectSymbol {
            name: entry_symbol,
            section: ".text".to_string(),
            offset: 0,
            storage_class: NativeSymbolStorageClass::External,
        }],
        bytes,
    })
}

pub fn snapshot_native_object(object: &NativeObjectFile) -> NativeObjectSnapshot {
    NativeObjectSnapshot {
        target_triple: object.target.triple.clone(),
        object_format: object.format,
        entry_symbol: object.entry_symbol.clone(),
        sections: object.sections.clone(),
        symbols: object.symbols.clone(),
        bytes_hex: hex_encode(&object.bytes),
    }
}

fn validate_entry_signature(function: &BytecodeFunction) -> Result<(), NativeObjectError> {
    if !function.params.is_empty() {
        return Err(NativeObjectError::UnsupportedSignature {
            function: function.name.clone(),
            reason: "entry function must not have parameters".to_string(),
        });
    }

    match function.return_type.name.as_str() {
        "void" | "i64" | "bool" => Ok(()),
        other => Err(NativeObjectError::UnsupportedSignature {
            function: function.name.clone(),
            reason: format!("return type `{other}` is not part of the prototype emitter"),
        }),
    }
}

fn lower_entry_function_to_x86_64(
    function: &BytecodeFunction,
) -> Result<Vec<u8>, NativeObjectError> {
    if function
        .instructions
        .iter()
        .any(|instruction| matches!(instruction, BytecodeInstruction::Let { .. }))
    {
        return NativeFunctionCodegen::new(function)?.emit();
    }

    match function.instructions.as_slice() {
        [BytecodeInstruction::Return(Some(expr))] => lower_return_expr(function, expr),
        [BytecodeInstruction::Return(None)] if function.return_type.is_void() => Ok(vec![0xC3]),
        [BytecodeInstruction::Expr(expr)] if !function.return_type.is_void() => {
            lower_return_expr(function, expr)
        }
        [BytecodeInstruction::Expr(_)] => Err(NativeObjectError::UnsupportedBody {
            function: function.name.clone(),
            reason: "void entry function cannot return an expression".to_string(),
        }),
        [] if function.return_type.is_void() => Ok(vec![0xC3]),
        _ => Err(NativeObjectError::UnsupportedBody {
            function: function.name.clone(),
            reason: "prototype emitter only supports a single literal return".to_string(),
        }),
    }
}

struct NativeFunctionCodegen<'a> {
    function: &'a BytecodeFunction,
    locals: HashMap<String, i32>,
    stack_size: u8,
}

impl<'a> NativeFunctionCodegen<'a> {
    fn new(function: &'a BytecodeFunction) -> Result<Self, NativeObjectError> {
        let mut locals = HashMap::new();

        for instruction in &function.instructions {
            match instruction {
                BytecodeInstruction::Let { name, ty, .. } if ty.name == "i64" => {
                    let offset = ((locals.len() + 1) * 8) as i32;
                    locals.insert(name.clone(), offset);
                }
                BytecodeInstruction::Let { name, ty, .. } => {
                    return Err(NativeObjectError::UnsupportedBody {
                        function: function.name.clone(),
                        reason: format!(
                            "prototype emitter only supports i64 locals; `{name}` has `{}`",
                            ty.name
                        ),
                    });
                }
                BytecodeInstruction::Return(_)
                | BytecodeInstruction::Expr(_)
                | BytecodeInstruction::Assign { .. }
                | BytecodeInstruction::Break(_)
                | BytecodeInstruction::Continue(_)
                | BytecodeInstruction::If { .. }
                | BytecodeInstruction::While { .. }
                | BytecodeInstruction::For { .. }
                | BytecodeInstruction::Throw { .. }
                | BytecodeInstruction::Try { .. }
                | BytecodeInstruction::Match { .. }
                | BytecodeInstruction::Asm { .. }
                | BytecodeInstruction::Loop { .. } => {}
            }
        }

        let local_bytes = locals.len() * 8;
        let stack_size = local_bytes.div_ceil(16) * 16;
        if stack_size > i8::MAX as usize {
            return Err(NativeObjectError::UnsupportedBody {
                function: function.name.clone(),
                reason: "prototype emitter supports at most 120 bytes of local stack".to_string(),
            });
        }

        Ok(Self {
            function,
            locals,
            stack_size: stack_size as u8,
        })
    }

    fn emit(&self) -> Result<Vec<u8>, NativeObjectError> {
        let mut code = Vec::new();
        self.emit_prologue(&mut code);

        for (index, instruction) in self.function.instructions.iter().enumerate() {
            let is_last = index + 1 == self.function.instructions.len();
            match instruction {
                BytecodeInstruction::Let {
                    name, ty, value, ..
                } => {
                    if ty.name != "i64" {
                        return self.unsupported(format!(
                            "prototype emitter only supports i64 locals; `{name}` has `{}`",
                            ty.name
                        ));
                    }
                    self.emit_i64_expr(value, &mut code)?;
                    self.emit_store_local(name, &mut code)?;
                }
                BytecodeInstruction::Return(Some(expr)) => {
                    self.emit_return_expr(expr, &mut code)?;
                    self.emit_epilogue(&mut code);
                    return Ok(code);
                }
                BytecodeInstruction::Return(None) if self.function.return_type.is_void() => {
                    self.emit_epilogue(&mut code);
                    return Ok(code);
                }
                BytecodeInstruction::Expr(expr)
                    if is_last && !self.function.return_type.is_void() =>
                {
                    self.emit_return_expr(expr, &mut code)?;
                    self.emit_epilogue(&mut code);
                    return Ok(code);
                }
                BytecodeInstruction::Expr(_) => {
                    return self.unsupported(
                        "prototype emitter only supports a final return expression".to_string(),
                    );
                }
                BytecodeInstruction::Assign {
                    name, op, value, ..
                } => {
                    self.emit_i64_assignment(name, *op, value, &mut code)?;
                }
                BytecodeInstruction::Break(_)
                | BytecodeInstruction::Continue(_)
                | BytecodeInstruction::If { .. }
                | BytecodeInstruction::While { .. }
                | BytecodeInstruction::For { .. }
                | BytecodeInstruction::Throw { .. }
                | BytecodeInstruction::Try { .. }
                | BytecodeInstruction::Match { .. }
                | BytecodeInstruction::Asm { .. }
                | BytecodeInstruction::Loop { .. } => {
                    return self.unsupported(
                        "prototype emitter only supports let, assignment, and one return"
                            .to_string(),
                    );
                }
                BytecodeInstruction::Return(None) => {
                    return self
                        .unsupported("non-void entry function must return a value".to_string());
                }
            }
        }

        if self.function.return_type.is_void() {
            self.emit_epilogue(&mut code);
            Ok(code)
        } else {
            self.unsupported("entry function does not return a value".to_string())
        }
    }

    fn emit_return_expr(
        &self,
        expr: &BytecodeExpr,
        code: &mut Vec<u8>,
    ) -> Result<(), NativeObjectError> {
        match self.function.return_type.name.as_str() {
            "i64" => self.emit_i64_expr(expr, code),
            "bool" => self.emit_bool_expr(expr, code),
            "void" => self.unsupported("void entry function cannot return a value".to_string()),
            other => self.unsupported(format!(
                "prototype emitter cannot lower return type `{other}`"
            )),
        }
    }

    fn emit_i64_expr(
        &self,
        expr: &BytecodeExpr,
        code: &mut Vec<u8>,
    ) -> Result<(), NativeObjectError> {
        if expr.ty.name != "i64" {
            return self.unsupported(format!(
                "prototype emitter expected i64 expression, found `{}`",
                expr.ty.name
            ));
        }

        match &expr.kind {
            BytecodeExprKind::Integer(value) => {
                code.extend_from_slice(&[0x48, 0xB8]);
                code.extend_from_slice(&(*value as u64).to_le_bytes());
                Ok(())
            }
            BytecodeExprKind::Variable(name) => self.emit_load_local(name, code),
            BytecodeExprKind::Binary { left, op, right } => {
                self.emit_i64_expr(left, code)?;
                code.push(0x50);
                self.emit_i64_expr(right, code)?;
                code.extend_from_slice(&[0x48, 0x89, 0xC1]);
                code.push(0x58);
                match op {
                    BinaryOp::Add => code.extend_from_slice(&[0x48, 0x01, 0xC8]),
                    BinaryOp::Subtract => code.extend_from_slice(&[0x48, 0x29, 0xC8]),
                    BinaryOp::Multiply => code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]),
                    other => {
                        return self.unsupported(format!(
                            "prototype emitter does not support i64 binary operator `{other:?}`"
                        ));
                    }
                }
                Ok(())
            }
            _ => self.unsupported(
                "prototype emitter only supports i64 literals, locals, and + - * expressions"
                    .to_string(),
            ),
        }
    }

    fn emit_bool_expr(
        &self,
        expr: &BytecodeExpr,
        code: &mut Vec<u8>,
    ) -> Result<(), NativeObjectError> {
        match &expr.kind {
            BytecodeExprKind::Bool(value) => {
                code.push(0xB8);
                code.extend_from_slice(&u32::from(*value).to_le_bytes());
                Ok(())
            }
            _ => self.unsupported(
                "prototype emitter only supports literal bool return values".to_string(),
            ),
        }
    }

    fn emit_i64_assignment(
        &self,
        name: &str,
        op: AssignOp,
        value: &BytecodeExpr,
        code: &mut Vec<u8>,
    ) -> Result<(), NativeObjectError> {
        match op {
            AssignOp::Replace => {
                self.emit_i64_expr(value, code)?;
            }
            AssignOp::Add | AssignOp::Subtract | AssignOp::Multiply => {
                self.emit_i64_expr(value, code)?;
                code.extend_from_slice(&[0x48, 0x89, 0xC1]);
                self.emit_load_local(name, code)?;
                match op {
                    AssignOp::Add => code.extend_from_slice(&[0x48, 0x01, 0xC8]),
                    AssignOp::Subtract => code.extend_from_slice(&[0x48, 0x29, 0xC8]),
                    AssignOp::Multiply => code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]),
                    AssignOp::Replace | AssignOp::Divide | AssignOp::Remainder => unreachable!(),
                }
            }
            AssignOp::Divide | AssignOp::Remainder => {
                return self.unsupported(
                    "prototype emitter does not support native i64 division/remainder assignment"
                        .to_string(),
                );
            }
        }

        self.emit_store_local(name, code)
    }

    fn emit_prologue(&self, code: &mut Vec<u8>) {
        if self.stack_size == 0 {
            return;
        }

        code.extend_from_slice(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, self.stack_size]);
    }

    fn emit_epilogue(&self, code: &mut Vec<u8>) {
        if self.stack_size > 0 {
            code.extend_from_slice(&[0x48, 0x83, 0xC4, self.stack_size, 0x5D]);
        }
        code.push(0xC3);
    }

    fn emit_load_local(&self, name: &str, code: &mut Vec<u8>) -> Result<(), NativeObjectError> {
        let offset = self.local_offset(name)?;
        code.extend_from_slice(&[0x48, 0x8B, 0x45, displacement_for_offset(offset)]);
        Ok(())
    }

    fn emit_store_local(&self, name: &str, code: &mut Vec<u8>) -> Result<(), NativeObjectError> {
        let offset = self.local_offset(name)?;
        code.extend_from_slice(&[0x48, 0x89, 0x45, displacement_for_offset(offset)]);
        Ok(())
    }

    fn local_offset(&self, name: &str) -> Result<i32, NativeObjectError> {
        self.locals
            .get(name)
            .copied()
            .ok_or_else(|| NativeObjectError::UnsupportedBody {
                function: self.function.name.clone(),
                reason: format!("unknown native local `{name}`"),
            })
    }

    fn unsupported<T>(&self, reason: String) -> Result<T, NativeObjectError> {
        Err(NativeObjectError::UnsupportedBody {
            function: self.function.name.clone(),
            reason,
        })
    }
}

fn displacement_for_offset(offset: i32) -> u8 {
    (-(offset as i8)) as u8
}

fn lower_return_expr(
    function: &BytecodeFunction,
    expr: &BytecodeExpr,
) -> Result<Vec<u8>, NativeObjectError> {
    match (&function.return_type.name[..], &expr.kind) {
        ("i64", BytecodeExprKind::Integer(value)) => {
            let mut code = vec![0x48, 0xB8];
            code.extend_from_slice(&(*value as u64).to_le_bytes());
            code.push(0xC3);
            Ok(code)
        }
        ("bool", BytecodeExprKind::Bool(value)) => {
            let mut code = vec![0xB8];
            code.extend_from_slice(&u32::from(*value).to_le_bytes());
            code.push(0xC3);
            Ok(code)
        }
        ("void", _) => Err(NativeObjectError::UnsupportedBody {
            function: function.name.clone(),
            reason: "void entry function cannot return a value".to_string(),
        }),
        (expected, _) => Err(NativeObjectError::UnsupportedBody {
            function: function.name.clone(),
            reason: format!(
                "prototype emitter cannot lower return type `{expected}` from this expression"
            ),
        }),
    }
}

fn write_x86_64_coff_object(symbol: &str, text: &[u8]) -> Result<Vec<u8>, NativeObjectError> {
    if symbol.len() > 8 {
        return Err(NativeObjectError::UnsupportedSymbol {
            symbol: symbol.to_string(),
            reason: "prototype COFF writer only supports short symbol names".to_string(),
        });
    }

    let raw_text_offset = COFF_HEADER_SIZE + SECTION_HEADER_SIZE;
    let symbol_table_offset = raw_text_offset + text.len() as u32;
    let mut bytes = Vec::with_capacity((symbol_table_offset + SYMBOL_RECORD_SIZE + 4) as usize);

    push_u16(&mut bytes, AMD64_MACHINE);
    push_u16(&mut bytes, 1);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, symbol_table_offset);
    push_u32(&mut bytes, 1);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);

    push_fixed_name(&mut bytes, ".text", 8);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, text.len() as u32);
    push_u32(&mut bytes, raw_text_offset);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, TEXT_CHARACTERISTICS);

    bytes.extend_from_slice(text);

    push_fixed_name(&mut bytes, symbol, 8);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 0x20);
    bytes.push(2);
    bytes.push(0);
    push_u32(&mut bytes, 4);

    Ok(bytes)
}

fn push_fixed_name(bytes: &mut Vec<u8>, name: &str, width: usize) {
    let mut buffer = vec![0; width];
    buffer[..name.len()].copy_from_slice(name.as_bytes());
    bytes.extend_from_slice(&buffer);
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

// ===========================================================================
// Extended native program emitter (multi-function, linkable, i64-scalar subset)
// ===========================================================================
//
// The prototype `emit_coff_object` above lowers a single literal-return
// `main`. The emitter below extends the same COFF machinery to the full
// i64-scalar subset the WASM backend targets: every function whose parameters
// and return type are all `i64` (up to four parameters, Win64 register args) is
// compiled to x86-64 machine code, with control flow (`if`/`while`/`loop`/`for`)
// lowered structurally and inter-function calls resolved through COFF
// relocations. An entry stub (`_lullaby_start`) calls `main`, moves its result
// into `ecx`, and calls `ExitProcess` (imported from kernel32) so the process
// exit code is `main`'s result mod 256. Functions using anything outside the
// subset are SKIPPED (they still run on the interpreters).

/// The result of emitting a linkable native program: the COFF object bytes plus
/// the record of which functions compiled and which were skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeProgram {
    /// Target triple (`x86_64-pc-windows-msvc`).
    pub target: NativeTarget,
    /// The COFF object bytes (a real linkable `.obj`).
    pub bytes: Vec<u8>,
    /// The entry-point symbol name the linker should use (`/entry:`).
    pub entry_symbol: String,
    /// Names of functions compiled to native code, in module order.
    pub compiled: Vec<String>,
    /// Functions skipped for the native subset, each with a reason.
    pub skipped: Vec<NativeSkippedFunction>,
    /// C import libraries the linker must also resolve, beyond `kernel32.lib`.
    /// Populated when the program calls `extern fn` C functions (e.g. `ucrt.lib`
    /// for the C runtime). Empty for a program with no extern calls.
    pub import_libs: Vec<String>,
}

/// The C runtime import library that provides the standard C library symbols
/// (e.g. `llabs`) an `extern fn` may name. Discovered like `kernel32.lib` via
/// the MSVC `LIB` environment variable.
pub const C_RUNTIME_IMPORT_LIB: &str = "ucrt.lib";

/// A function that was not eligible for the native i64-scalar subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSkippedFunction {
    pub name: String,
    pub reason: String,
}

/// A hard failure while emitting the native program. The only hard error is "no
/// i64-scalar function was eligible", surfaced by the CLI as diagnostic `L0339`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeProgramError {
    pub code: &'static str,
    pub message: String,
    /// Functions skipped, so the CLI can still report why nothing compiled.
    pub skipped: Vec<NativeSkippedFunction>,
}

/// Diagnostic code for "no i64-scalar functions eligible for native codegen".
/// Kept inline (like the WASM backend's `L0338`) rather than in the shared
/// diagnostic registry, which only carries frontend/semantic codes.
pub const NATIVE_NO_ELIGIBLE_CODE: &str = "L0339";

/// The entry-stub symbol name. The linker is invoked with `/entry:` set to this.
pub const NATIVE_ENTRY_SYMBOL: &str = "_lullaby_start";

/// The imported process-exit function (from kernel32). Referenced by the entry
/// stub through a REL32 relocation; the linker binds it to the import thunk.
const EXIT_PROCESS_SYMBOL: &str = "ExitProcess";

/// The bump-allocator helper emitted in `.text`. Signature: the requested byte
/// count is passed in `rcx`, the allocated heap pointer is returned in `rax`.
/// See `emit_heap_alloc_helper` for the body.
const HEAP_ALLOC_SYMBOL: &str = "__lullaby_alloc";

/// The string-length helper emitted in `.text`. Signature: a pointer to a
/// NUL-terminated byte string in `.rdata` is passed in `rcx`; the byte length of
/// a fresh heap copy of that string is returned in `rax`. This exercises the
/// full first heap step: it bump-allocates via `__lullaby_alloc`, copies the
/// `.rdata` bytes into the heap, then scans the heap copy for the terminator.
const HEAP_STRLEN_SYMBOL: &str = "__lullaby_strlen_copy";

/// The bump-pointer cell symbol in `.bss` (an 8-byte pointer, zero-initialized).
/// A zero value means "not yet initialized"; the allocator lazily seeds it to
/// the base of the heap region on first use.
const HEAP_NEXT_SYMBOL: &str = "__lullaby_heap_next";

/// The free-list head cell symbol in `.bss` (an 8-byte pointer, zero-initialized).
/// Reference-counted blocks freed by `__lullaby_rc_free` are pushed onto this LIFO
/// free list and reused by `__lullaby_alloc` (first-fit). `0` means the list is
/// empty. Until scope-based drop insertion (RC stage 2) emits `rc_dec`/`rc_free`
/// calls, nothing frees, so the list stays empty and the allocator behaves exactly
/// like the old bump allocator (plus a per-block RC header).
const HEAP_FREE_HEAD_SYMBOL: &str = "__lullaby_free_head";

/// The heap region base symbol in `.bss` — a fixed reserved bump region.
const HEAP_BASE_SYMBOL: &str = "__lullaby_heap_base";

/// `__lullaby_rc_dec(payload ptr in rcx)`: decrement the block's refcount at
/// `[rcx - 8]` and, if it reached zero, tail-call `__lullaby_rc_free` to return the
/// block to the free list. The drop primitive scope-based drop insertion (RC stage
/// 2) emits at each scope-exit edge of a uniquely-owned heap local.
const RC_DEC_SYMBOL: &str = "__lullaby_rc_dec";

/// `__lullaby_rc_free(payload ptr in rcx)`: push the block (whose base is
/// `rcx - 16`) onto the LIFO free list `__lullaby_free_head`, threading the "next"
/// link through the freed block's refcount slot (`[base + 8]`). The allocator
/// first-fit-reuses it on a later allocation.
const RC_FREE_SYMBOL: &str = "__lullaby_rc_free";

/// `__lullaby_drop_string_array(block ptr in rcx)`: a RECURSIVE drop for a
/// `list<string>`-layout block (an `array<string>` / `list<string>`): `rc_dec` each
/// of the `len` shared string element pointers, then `rc_dec` the block itself.
/// Used to reclaim a uniquely-owned `array<string>` loop temporary (a `split`
/// result) whose string elements are owned solely by the block.
const DROP_STRING_ARRAY_SYMBOL: &str = "__lullaby_drop_string_array";

/// Size of the per-allocation reference-counting header, in bytes: `[size i64]
/// [refcount i64]` sitting immediately BEFORE the payload the allocator returns.
/// The returned pointer names the payload (offset 0 = the first payload word), so
/// every existing record offset (string/list/map/struct/enum) is unchanged; the
/// header is addressed at negative offsets (`refcount` at `[ptr - 8]`, `size` at
/// `[ptr - 16]`). Storing the block size lets `rc_free` return a block to the
/// free list and `alloc` first-fit-reuse it.
const RC_HEADER_SIZE: i32 = 16;

/// Size in bytes of the fixed reserved native heap region. Growable lists
/// capacity-double and orphan their old backing blocks in this no-reclaim bump
/// heap, so the region is sized generously (1 MiB) to give list-building
/// programs headroom. It lives in zero-initialized `.bss`, so it costs no object
/// file bytes.
const HEAP_REGION_SIZE: u32 = 1024 * 1024;

// -- Growable list layout (native) -------------------------------------------
//
// A growable `list<T>` (scalar `T`) is a heap pointer (one 8-byte word / register
// value) to a header `[len: i64][cap: i64][elem slots...]`: the current element
// count, the allocated capacity, then `cap` 8-byte element slots. Every field is
// an 8-byte word so the whole block is naturally `i64`-aligned and element `i`
// lives at `LIST_DATA_OFF + i * 8`, letting a scalar element (i64/fixed-width/
// bool/char/byte, or an f64/f32 stored bit-for-bit in its low bytes) be moved as
// a flat 8-byte word.
//
// Value semantics: Lullaby lists are value-semantic (`l = push(l, x)` returns a
// NEW list). Every mutating op (`push`/`set`/`pop`) deep-copies the source list
// first and mutates the fresh copy, so mutating one binding is never observable
// through another (`let b = a` then `set(b, ...)` leaves `a` untouched because
// `set` copies `b`). Read ops (`get`/`len`) never mutate, so sharing a list
// pointer across a binding or a call boundary is safe without an extra copy —
// exactly matching the interpreters bit-for-bit. The bump allocator never
// reclaims, so a grown or copied list orphans its old block, like the existing
// string-constant heap growth.

/// Byte offset of a list's `len` header (element count), an `i64` word.
const LIST_LEN_OFF: i32 = 0;

/// Byte offset of a list's `cap` header (allocated capacity in elements), an
/// `i64` word.
const LIST_CAP_OFF: i32 = 8;

/// Byte offset of a list's first element slot, past the two `i64` headers.
const LIST_DATA_OFF: i32 = 16;

/// Bytes per list element slot — one 8-byte word, like struct/array/enum slots.
const LIST_SLOT_SIZE: i32 = 8;

/// Initial capacity a `list_new()` header (and the first growth of an empty list)
/// allocates, so a handful of pushes do not each trigger a realloc. Mirrors the
/// WASM backend's `LIST_INITIAL_CAP`.
const LIST_INITIAL_CAP: i64 = 4;

/// The list-runtime helper emitted in `.text`. Signature: no arguments; returns a
/// fresh `[len=0][cap=LIST_INITIAL_CAP][slots]` heap block pointer in `rax`.
const LIST_NEW_SYMBOL: &str = "__lullaby_list_new";

/// The list deep-copy helper emitted in `.text`. Signature: the source list
/// pointer in `rcx`; returns a fresh independent copy's pointer in `rax`.
const LIST_COPY_SYMBOL: &str = "__lullaby_list_copy";

/// The list-grow helper emitted in `.text`. Signature: the list pointer in `rcx`;
/// returns a (possibly reallocated) list pointer in `rax` guaranteed to have
/// `cap > len` (room for one more push). Doubles capacity (or seeds
/// `LIST_INITIAL_CAP` from an empty list) and copies the live elements.
const LIST_GROW_SYMBOL: &str = "__lullaby_list_grow";

// -- Heap struct layout (native, collection-element representation) -----------
//
// A struct used as a MUTABLE-heap collection element/value/enum payload
// (`list<struct>`, `map<K, struct>`, `option<struct>`) cannot be the stack-
// flattened `NativeType::Struct` (that occupies many words); it must fit a single
// element slot. So it is laid out on the HEAP: a header word `[nwords i64]`
// followed by `nwords` field words (one 8-byte word per flattened field, in
// declared order). The value in the element slot is a pointer to FIELD 0 — i.e.
// `alloc_base + 8` — so field `k` lives at `[ptr + 8*k]` and the word count is at
// `[ptr - STRUCT_HEADER_SIZE]`. Storing the count in the block lets a single
// type-agnostic `__lullaby_struct_copy` helper deep-copy any heap struct (its
// fields are always scalars/immutable strings at the one-level nesting bound, so a
// flat word copy IS an exact deep copy). Because the header sits *below* field 0,
// heap-struct field access (`p.x`) and the heap→stack bridge address fields at
// `[ptr + 8*k]` with no header adjustment.

/// Bytes of the heap-struct header (the `[nwords]` word), stored just below the
/// field-0 pointer. The allocation is `STRUCT_HEADER_SIZE + nwords * 8` bytes and
/// the value pointer is `alloc_base + STRUCT_HEADER_SIZE`.
const STRUCT_HEADER_SIZE: i32 = 8;

/// The generic heap-struct deep-copy helper emitted in `.text`. Signature: the
/// source heap-struct pointer (to field 0) in `rcx`; returns in `rax` a fresh
/// independent block's field-0 pointer. Reads the `[rcx - STRUCT_HEADER_SIZE]`
/// word count, allocates `STRUCT_HEADER_SIZE + nwords * 8`, copies the header and
/// every field word (a flat copy — heap-struct fields are scalars or shared
/// immutable strings at the one-level nesting bound), and returns field-0 pointer.
const STRUCT_COPY_SYMBOL: &str = "__lullaby_struct_copy";

/// Builtins that construct or read growable lists, matched by name in call
/// lowering (arity / element type are validated there against the IR types).
const LIST_NEW_BUILTIN: &str = "list_new";
const LIST_PUSH_BUILTIN: &str = "push";
const LIST_GET_BUILTIN: &str = "get";
const LIST_SET_BUILTIN: &str = "set";
const LIST_POP_BUILTIN: &str = "pop";

/// Whether a growable-collection element/value/payload type occupies a single
/// 8-byte native slot copied by a flat word — a native scalar (`i64`/fixed-width/
/// `bool`/`char`/`byte`/`f64`/`f32`) or a `string` (an immutable heap pointer). A
/// `string` occupies one word exactly like a scalar and, because strings are
/// immutable, is copied by SHARING its pointer on a value-semantic deep copy
/// (never deep-recursed into the string record) — so the flat word copy the list/
/// map/enum copy paths already emit is an exact deep copy and needs no per-slot
/// type dispatch. This mirrors the WASM backend's `scalar_or_string_slot_type`.
/// Other (mutable) heap types (`struct`/`array`/`list`/`map`) are NOT single-slot
/// copyable — they would need a recursive per-element deep copy — so they return
/// `false` and the enclosing function skips gracefully.
fn is_scalar_or_string_slot(ty: &TypeRef) -> bool {
    ty.name == "i64"
        || fixed_int_kind(&ty.name).is_some()
        || matches!(ty.name.as_str(), "bool" | "char" | "byte" | "f64" | "f32")
        || ty.name == "string"
}

/// The maximum depth of MUTABLE-aggregate nesting a growable collection
/// element/value/enum payload may reach before the native backend defers it.
/// Depth 0 is the collection's own element/value slot; a struct field or a nested
/// list element consumes one level. One level of mutable nesting (`list<struct>`,
/// `list<list<scalar>>`, `map<K, struct>`, `option<struct>`) is supported; deeper
/// cases (`list<list<list<…>>>`, `list<map<…>>`, a struct field that is itself a
/// list/map/struct) are skipped gracefully (still run on the interpreters) rather
/// than miscompiled. Mirrors the WASM backend's `MAX_COLLECTION_NEST_DEPTH`.
const MAX_COLLECTION_NEST_DEPTH: u32 = 1;

/// The native slot layout of a growable-collection element/value/enum payload at
/// nesting `depth`, or `None` if the native backend cannot lay it out (so the
/// enclosing function skips gracefully). This is the native mirror of the WASM
/// backend's `collection_slot_type`, bounded to one mutable-aggregate level.
/// Accepts, in order:
///
/// - a **scalar** — its own single-word layout, flat-copied on a deep copy;
/// - a **`string`** — a single pointer word to the immutable record, SHARED on a
///   deep copy (never deep-recursed) since strings are immutable;
/// - a **mutable aggregate** (a named `struct` → [`NativeType::HeapStruct`], or a
///   supported nested growable `list<T>` → [`NativeType::List`]) at
///   `depth < MAX_COLLECTION_NEST_DEPTH` — a single pointer word that is itself
///   DEEP-COPIED per element on the collection's value-semantic copy (see
///   [`emit_heap_slot_deep_copy`]), matching the interpreters' recursive
///   `Value::clone`. The nested aggregate's own fields/elements are classified one
///   level deeper, so `list<list<scalar>>` is accepted but `list<list<list<…>>>`
///   is deferred.
///
/// A nested `map` element/value (`list<map<…>>`, `map<K, map<…>>`), a fixed
/// `array` element, and an `enum` element are DEFERRED (return `None`).
fn native_collection_slot(
    ty: &TypeRef,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    depth: u32,
) -> Option<NativeType> {
    if is_scalar_or_string_slot(ty) {
        // A scalar or immutable-string element occupies one flat word.
        return resolve_native_type(ty, structs, enums).ok();
    }
    if depth >= MAX_COLLECTION_NEST_DEPTH {
        return None;
    }
    // A struct element/value: every field must itself be layable-out (a scalar, a
    // `string`, or a mutable aggregate one level deeper). Laid out on the heap as a
    // single pointer word (`HeapStruct`), deep-copied per element.
    if let Some(def) = structs.iter().find(|s| s.name == ty.name) {
        let mut fields = Vec::with_capacity(def.fields.len());
        for (field_name, field_ty) in &def.fields {
            let native = native_collection_slot(field_ty, structs, enums, depth + 1)?;
            fields.push((field_name.clone(), native));
        }
        return Some(NativeType::HeapStruct {
            name: ty.name.clone(),
            fields,
        });
    }
    // A nested growable `list<T>` element/value: its own element must be layable-out
    // one level deeper (so `list<list<scalar>>` works, `list<list<list<…>>>` does
    // not). The nested list is deep-copied per element on the outer copy.
    if let Some(elem) = ty.list_element() {
        let elem_native = native_collection_slot(&elem, structs, enums, depth + 1)?;
        return Some(NativeType::List {
            elem: Box::new(elem_native),
        });
    }
    None
}

/// Whether a resolved `NativeType`, when it occupies a collection element / map
/// value / enum payload slot, must be DEEP-COPIED (recursively) on a value-semantic
/// copy rather than flat-word-copied. A scalar or immutable `string` is flat-copied
/// (a `string`'s shared pointer IS its value-semantic copy); a `HeapStruct` or a
/// nested `List`/`Map` element must be recursively deep-copied so mutating one copy
/// is never observable through another (the interpreters' recursive `Value::clone`).
/// Mirrors the WASM backend's `is_mutable_aggregate` for the element context.
fn native_slot_needs_deep_copy(ty: &NativeType) -> bool {
    matches!(
        ty,
        NativeType::HeapStruct { .. } | NativeType::List { .. } | NativeType::Map { .. }
    )
}

/// The element type of a supported growable `list<T>`, or `None` if `ty` is not a
/// list or its element is neither a native scalar nor a `string`. A `string`
/// element is an immutable heap pointer stored in one slot and shared (not
/// deep-recursed) on the value-semantic deep copy, so `list<string>` is supported.
/// Lists of MUTABLE heap elements (`list<struct>`/`list<list<…>>`/`list<map<…>>`)
/// are DEFERRED — the native backend does not yet recursively deep-copy mutable
/// heap elements — so such a list is unsupported and its enclosing function skips
/// (still runs on the interpreters).
///
/// A **one-level MUTABLE-aggregate element** — a named `struct` or a nested
/// `list<scalar|string>` — is now also accepted (`list<struct>`,
/// `list<list<i64>>`): such an element occupies one pointer word and is
/// DEEP-COPIED per element on the value-semantic copy (see [`native_collection_slot`]
/// and [`emit_heap_slot_deep_copy`]), matching the WASM backend and the
/// interpreters' recursive `Value::clone`. Deeper nesting, a `map`/`array` element,
/// or an `enum` element stays DEFERRED. This function is a **structural** accept
/// (it does not re-validate a named element against the struct/enum tables — the
/// eligibility gate's `resolve_native_type` already rejected an unresolvable element
/// before any lowering guard consults this), so it takes only the `TypeRef`.
fn supported_list_element(ty: &TypeRef) -> Option<TypeRef> {
    let elem = ty.list_element()?;
    if is_native_collection_element_shape(&elem) {
        Some(elem)
    } else {
        None
    }
}

/// A heap-backed `array<string>` — the `split`/`words` result and `array<string>`
/// literals. It is represented natively exactly like a `list<string>`: a pointer
/// to a `[len][cap][slot…]` block of shared immutable string pointers. Returns the
/// element type (`string`) when `ty` is such an array, else `None`. Only `string`
/// elements are heap-backed here; `array<i64>`/`array<f64>` stay stack-flattened
/// with a statically-inferred length, so they are excluded.
fn heap_string_array_element(ty: &TypeRef) -> Option<TypeRef> {
    let elem = ty.name.strip_prefix("array<")?.strip_suffix('>')?;
    (elem == "string").then(|| TypeRef::new("string"))
}

/// Structural shape test for a collection element / map value / enum payload: a
/// scalar, a `string`, a named type (a struct — validated for real by
/// `native_collection_slot`/`resolve_native_type`), or a nested `list<…>` whose own
/// element is again a plausible shape. Bounded to one nested `list` level here (a
/// `list<list<list<…>>>` element is a `list` whose element is a `list`, which this
/// rejects) so the structural guard already excludes over-deep nesting; the full
/// depth-and-table validation is `native_collection_slot`. A `map`/`array` element
/// is rejected (deferred).
fn is_native_collection_element_shape(ty: &TypeRef) -> bool {
    if is_scalar_or_string_slot(ty) {
        return true;
    }
    if let Some(inner) = ty.list_element() {
        // A nested list: accept when its own element is a scalar or string (one
        // mutable level). `list<list<list<…>>>` (inner is itself a list) is rejected.
        return is_scalar_or_string_slot(&inner);
    }
    if ty.name.starts_with("map<") || ty.name.starts_with("array<") {
        return false;
    }
    // A bare named type is a candidate struct; `native_collection_slot` validates it
    // against the struct table and bounds its field nesting.
    ty.list_element().is_none() && ty.option_element().is_none() && ty.result_args().is_none()
}

// -- Growable map layout (native) --------------------------------------------
//
// A growable `map<K, V>` (scalar `K`/`V`) is a heap pointer (one 8-byte word,
// like a list) to a header `[len: i64][cap: i64][entries...]`: the current entry
// count, the allocated capacity in entries, then `cap` two-word entries. Each
// entry is a `(key, value)` pair of 8-byte words — the key at `+0`, the value at
// `+MAP_VALUE_OFF` — so entry `i` lives at `MAP_DATA_OFF + i * MAP_ENTRY_SIZE`.
// Every field is an 8-byte word (uniform with native list/struct/enum slots).
//
// This mirrors the interpreters' `Value::Map`: an INSERTION-ORDERED association
// list scanned linearly with `Value` equality. `map_set` overwrites the value of
// an existing key in place (preserving its position) or appends a new entry at
// the end, growing with capacity doubling like a list; `map_get`/`map_has` scan
// entries front-to-back so the FIRST matching key wins; `map_len` reads the
// header. Ordering and lookup therefore match the interpreters bit-for-bit.
//
// Value semantics: like native lists, maps are value-semantic. Every mutating op
// (`map_set`) deep-copies the source map first and mutates the fresh copy, and a
// map crossing a call boundary shares its pointer safely because the only
// mutator copies. Read ops (`map_get`/`map_has`/`map_len`) never mutate. The bump
// allocator never reclaims, so a grown/copied map orphans its old block.
//
// Key equality is a raw 8-byte word compare (`cmp`), exact for the integer-cell
// key types (`i64`/fixed-width/`bool`/`char`/`byte`, all stored as normalized
// `i64` cells). FLOAT keys are DEFERRED here: their word compare would treat
// `+0.0`/`-0.0` and NaNs differently from the interpreters' value equality (and
// from the WASM backend's ordered `f*.eq`), so a `map<f64, V>`/`map<f32, V>` is
// unsupported and its function skips to the interpreters. Float VALUES are fine:
// they are stored/loaded bit-for-bit and never compared.

/// Byte offset of a map's `len` header (entry count), an `i64` word.
const MAP_LEN_OFF: i32 = 0;

/// Byte offset of a map's `cap` header (allocated capacity in entries), an `i64`
/// word.
const MAP_CAP_OFF: i32 = 8;

/// Byte offset of a map's first entry, past the two `i64` headers.
const MAP_DATA_OFF: i32 = 16;

/// Byte offset of the value word within an entry (past the key word).
const MAP_VALUE_OFF: i32 = 8;

/// Bytes per map entry — a `(key, value)` pair of two 8-byte words.
const MAP_ENTRY_SIZE: i32 = 16;

/// Initial capacity a `map_new()` header (and the first growth of an empty map)
/// allocates. Mirrors the list `LIST_INITIAL_CAP` and the WASM `MAP_INITIAL_CAP`.
const MAP_INITIAL_CAP: i64 = 4;

/// The map-runtime helper emitted in `.text`. Signature: no arguments; returns a
/// fresh `[len=0][cap=MAP_INITIAL_CAP][entries]` heap block pointer in `rax`.
const MAP_NEW_SYMBOL: &str = "__lullaby_map_new";

/// The map deep-copy helper emitted in `.text`. Signature: the source map pointer
/// in `rcx`; returns a fresh independent copy's pointer in `rax`.
const MAP_COPY_SYMBOL: &str = "__lullaby_map_copy";

/// The map-grow helper emitted in `.text`. Signature: the map pointer in `rcx`;
/// returns a (possibly reallocated) map pointer in `rax` guaranteed to have
/// `cap > len` (room for one more entry). Doubles capacity (or seeds
/// `MAP_INITIAL_CAP` from an empty map) and copies the live entries.
const MAP_GROW_SYMBOL: &str = "__lullaby_map_grow";

/// The map linear-scan helper emitted in `.text`. Signature: the map pointer in
/// `rcx`, the key word in `rdx`; returns in `rax` the index of the first entry
/// whose key equals `rdx`, or the map's `len` if no key matched (the "found index
/// else len" convention shared by `map_set`/`map_get`/`map_has`).
const MAP_FIND_SYMBOL: &str = "__lullaby_map_find";

/// Builtins that construct or read growable maps, matched by name in call
/// lowering (arity / key / value types are validated there).
const MAP_NEW_BUILTIN: &str = "map_new";
const MAP_SET_BUILTIN: &str = "map_set";
const MAP_GET_BUILTIN: &str = "map_get";
const MAP_HAS_BUILTIN: &str = "map_has";
const MAP_LEN_BUILTIN: &str = "map_len";

/// The `(key, value)` element types of a supported growable `map<K, V>`, or
/// `None` if `ty` is not a map, its key is not a supported native scalar, or its
/// value is neither a native scalar nor a `string`. Keys are restricted to the
/// integer-cell scalar types (`i64`, fixed-width integers, `bool`/`char`/`byte`)
/// so that key equality is an exact 8-byte word compare. Values may be any native
/// scalar including a float (`f64`/`f32`, stored bit-for-bit, never compared) or a
/// `string` (an immutable heap pointer stored in one slot, shared on the flat
/// two-word entry copy since strings are immutable — `map<K, string>` is
/// supported). Heap KEYS (`map<string, V>`, …), MUTABLE heap values
/// (`map<K, list<…>>`, `map<K, struct>`), and float keys are DEFERRED — such a map
/// is unsupported and its enclosing function skips (still runs on the
/// interpreters), matching the WASM map's increment. A string key stays deferred
/// because the interpreters compare keys by content equality (decoded bytes), not
/// by the interned pointer — separate work.
fn supported_map_kv(ty: &TypeRef) -> Option<(TypeRef, TypeRef)> {
    let (key, value) = ty.map_args()?;
    // Key must be an integer-cell scalar (word-compare equality).
    if !(key.name == "i64"
        || fixed_int_kind(&key.name).is_some()
        || matches!(key.name.as_str(), "bool" | "char" | "byte"))
    {
        return None;
    }
    // Value may be any native scalar (including a float), a `string` pointer, or a
    // one-level MUTABLE aggregate (a `struct` or a nested `list<scalar|string>`),
    // which is deep-copied per value on the map's value-semantic copy
    // (`map<K, struct>` is now supported). A `map`/`array` value is DEFERRED.
    if !is_native_collection_element_shape(&value) {
        return None;
    }
    Some((key, value))
}

// -- Heap string layout (native) ---------------------------------------------
//
// A first-class `string` value is a heap pointer (one 8-byte word / register
// value) to a record `[char_len: i64][byte_len: i64][utf8 bytes]`: the Unicode
// scalar (char) count, the UTF-8 byte length, then the encoded bytes. This
// mirrors the WASM backend's string record (which uses i32 headers); native uses
// i64 headers so every field is a uniform 8-byte word, matching the native
// list/map/struct slot discipline.
//
// Strings are IMMUTABLE, so — unlike lists and maps — a string value needs no
// deep-copy when bound (`let b = a`), passed as an argument, or returned: sharing
// the pointer is already value-equivalent (exactly the interpreters' behavior and
// the WASM backend's, which also never copies a string argument). A string
// therefore crosses a function boundary as a plain pointer word in an integer
// register, never as a by-pointer aggregate.
//
// `len(s)` reads the `char_len` header for ANY string value. Runtime `+`
// concatenation allocates a fresh record, sums the headers, and byte-copies both
// operands' UTF-8 ranges. `to_string` builds a fresh record from an integer/
// bool/char/byte (identity on a string). All records are bump-allocated (no
// reclamation), like every other native heap value.

/// Byte offset of a string record's `char_len` header (Unicode scalar count).
const STR_CHAR_LEN_OFF: i32 = 0;

/// Byte offset of a string record's `byte_len` header (UTF-8 byte length).
const STR_BYTE_LEN_OFF: i32 = 8;

/// Byte offset of a string record's first UTF-8 byte, past the two i64 headers.
const STR_DATA_OFF: i32 = 16;

/// The string-literal materialization helper emitted in `.text`. Signature: a
/// pointer to a NUL-terminated `.rdata` byte string in `rcx`; returns in `rax` a
/// fresh heap string record `[char_len][byte_len][utf8 bytes]` copied from those
/// bytes (the byte length scanned to the terminator, the char count computed by
/// decoding UTF-8 lead bytes). The `.rdata` layout is unchanged (raw
/// NUL-terminated bytes, shared with the `len("literal")` path), so a string
/// literal used as a value materializes through this helper at runtime.
const STR_LIT_SYMBOL: &str = "__lullaby_str_lit";

/// The `string`→`cstr` FFI materialization helper emitted in `.text`. Signature: a
/// heap string record pointer (`[char_len][byte_len][utf8]`) in `rcx`; returns in
/// `rax` a freshly bump-allocated `byte_len + 1` buffer holding the record's UTF-8
/// bytes followed by a NUL terminator — a `const char*` a C function borrows for
/// the duration of the call. Used to pass a Lullaby `string` to an extern's `cstr`
/// parameter. An interior NUL is copied verbatim, so a C reader sees the truncated
/// C-string prefix (standard `char*` semantics), matching the ffi_design contract.
const TO_CSTR_SYMBOL: &str = "__lullaby_to_cstr";

/// The string-concatenation helper emitted in `.text`. Signature: the left
/// string record pointer in `rcx`, the right in `rdx`; returns in `rax` a fresh
/// record whose char/byte headers are the summed operands' headers and whose
/// bytes are the two operands' UTF-8 ranges concatenated.
const STR_CONCAT_SYMBOL: &str = "__lullaby_str_concat";

/// The ownership-aware concat helper: left in `rcx`, right in `rdx`, and a
/// compile-time ownership mask in `r8` (bit 0 = the left operand is a
/// uniquely-owned fresh temporary, bit 1 = the right is). It concatenates (via
/// `__lullaby_str_concat`), then `rc_dec`s each operand the mask marks — reclaiming
/// intermediate string temporaries (e.g. the `to_string(i)` and the literal inside
/// `to_string(i) + "…"`) that would otherwise leak. Emitted only when at least one
/// operand is a fresh temp; a plain `var + var` concat still lowers to the bare
/// `__lullaby_str_concat`.
const STR_CONCAT_OWN_SYMBOL: &str = "__lullaby_str_concat_own";

/// The ownership-aware `len` helper: a string record pointer in `rcx` that is a
/// uniquely-owned fresh temporary. Reads the `char_len` header, then `rc_dec`s the
/// record (reclaiming it), and returns the length in `rax`. Lets `len(<fresh
/// temp>)` — e.g. `len(to_string(i))`, `len(a + b)`, `len(substring(…))` — reclaim
/// the temporary that `len` would otherwise read and leak. A `len` on a borrowed
/// string value keeps the plain header read.
const STR_LEN_OWN_SYMBOL: &str = "__lullaby_str_len_own";

/// The ownership-aware two-string-op helper: left in `rcx`, right in `rdx`, a
/// compile-time ownership mask in `r8` (bit 0 = left is a fresh temp, bit 1 =
/// right), and the target op's address in `r9`. It calls the op (an indirect
/// `call r9`, forwarding `left`/`right`), then `rc_dec`s each operand the mask
/// marks, and returns the op's result — reclaiming fresh-temp arguments to the
/// borrow-only two-string builtins (`find`/`count`/`contains`/`starts_with`/
/// `ends_with`) and to `split`/`join`. Emitted only when an operand is a fresh
/// temp; a `var`/`var` call keeps the bare op with zero overhead.
const STR_BINOP_OWN_SYMBOL: &str = "__lullaby_str_binop_own";

/// The ownership-aware string-read helper: a fresh-temp source string in `rcx`,
/// the op's other (scalar) arguments already in `rdx`/`r8`, and the op's address in
/// `r9`. It calls the op (`call r9`, forwarding `rcx`/`rdx`/`r8`), then `rc_dec`s
/// the source, and returns the op's single result in `rax` — reclaiming a fresh
/// temporary passed to `substring`/`char_at`/`repeat`/`trim` (each reads the source
/// and produces an independent new value, so the source is dead afterwards).
const STR_READ_OWN_SYMBOL: &str = "__lullaby_str_read_own";

/// The integer-to-string helper emitted in `.text`. Signature: a signed 64-bit
/// value in `rcx` and a signedness flag in `rdx` (0 = format as unsigned `u64`,
/// nonzero = format as signed `i64`); returns in `rax` a fresh string record of
/// the decimal digits (a leading `-` for a negative signed value). Matches the
/// interpreters' `Display` for `i64`/fixed-width integers (`byte` uses the
/// unsigned path).
const STR_FROM_INT_SYMBOL: &str = "__lullaby_str_from_int";

/// The bool-to-string helper emitted in `.text`. Signature: a 0/1 flag in `rcx`;
/// returns in `rax` a fresh string record holding `"true"` or `"false"`.
const STR_FROM_BOOL_SYMBOL: &str = "__lullaby_str_from_bool";

/// The char-to-string helper emitted in `.text`. Signature: a Unicode scalar
/// value (code point) in `rcx`; returns in `rax` a fresh single-character string
/// record holding that code point's UTF-8 encoding (1–4 bytes, `char_len = 1`).
const STR_FROM_CHAR_SYMBOL: &str = "__lullaby_str_from_char";

/// The char-indexed `substring` helper emitted in `.text`. Signature: the source
/// string record pointer in `rcx`, the `start` char index (i64) in `rdx`, the
/// `end` char index (i64) in `r8`; returns in `rax` a fresh `[char_len][byte_len]
/// [utf8]` record holding the half-open `[start, end)` char slice. On an
/// out-of-bounds range (`start < 0 || end < 0 || start > end || end > char_count`)
/// it traps with `ud2`, mirroring the interpreters' `L0413`.
const STR_SUBSTRING_SYMBOL: &str = "__lullaby_str_substring";

/// The char-index helper emitted in `.text`. Signature: the source string record
/// pointer in `rcx`, the char index (i64) in `rdx`; returns in `rax` the Unicode
/// code point of the `i`-th character (an `i64` `char` cell). On an out-of-range
/// index (`i < 0 || i >= char_count`) it traps with `ud2`, mirroring `L0413`.
/// Implements `s[i]`.
const STR_CHAR_AT_SYMBOL: &str = "__lullaby_str_char_at";

/// The substring-count helper emitted in `.text`. Signature: the haystack record
/// pointer in `rcx`, the needle record pointer in `rdx`; returns in `rax` the count
/// of NON-overlapping byte-level occurrences of the needle (matching the
/// interpreters' `text.matches(sub).count()`). An empty needle yields `0`.
const STR_COUNT_SYMBOL: &str = "__lullaby_str_count";

/// The string-repeat helper emitted in `.text`. Signature: the source record
/// pointer in `rcx`, the repeat count (i64) in `rdx`; returns in `rax` a fresh
/// record that is the source concatenated `count` times (`count <= 0` yields the
/// empty string), matching the interpreters' `text.repeat(count)`.
const STR_REPEAT_SYMBOL: &str = "__lullaby_str_repeat";

/// The string-trim helper emitted in `.text`. Signature: the source record pointer
/// in `rcx`; returns in `rax` a fresh record with leading/trailing ASCII
/// whitespace removed (matching `trim_matches(is_ascii_whitespace)`). Computes the
/// trimmed byte bounds and delegates to `__lullaby_str_substring` (byte offsets ==
/// char indices for the ASCII strings the native subset builds).
const STR_TRIM_SYMBOL: &str = "__lullaby_str_trim";

/// The `find` helper emitted in `.text`. Signature: the haystack record pointer
/// in `rcx`, the needle record pointer in `rdx`; returns in `rax` the CHAR index
/// (i64) of the first byte-level occurrence of the needle, or `-1` if absent. An
/// empty needle yields `0`. Matches the interpreters' `char_find`.
const STR_FIND_SYMBOL: &str = "__lullaby_str_find";

/// The `contains` helper emitted in `.text`. Signature: the string record pointer
/// in `rcx`, the substring record pointer in `rdx`; returns `0`/`1` (bool) in
/// `rax`. An empty substring is contained. Byte-exact, matching the interpreters.
const STR_CONTAINS_SYMBOL: &str = "__lullaby_str_contains";

/// The `starts_with` helper emitted in `.text`. Signature: the string record
/// pointer in `rcx`, the prefix record pointer in `rdx`; returns `0`/`1` (bool) in
/// `rax`. An empty prefix matches; a longer-than-haystack prefix does not.
const STR_STARTS_WITH_SYMBOL: &str = "__lullaby_str_starts_with";

/// The `ends_with` helper emitted in `.text`. Signature: the string record
/// pointer in `rcx`, the suffix record pointer in `rdx`; returns `0`/`1` (bool) in
/// `rax`. An empty suffix matches; a longer-than-haystack suffix does not.
const STR_ENDS_WITH_SYMBOL: &str = "__lullaby_str_ends_with";

/// The `parse_i64` helper emitted in `.text`. Signature: the source string record
/// pointer in `rcx`; returns the `result<i64, string>` variant tag in `rax`
/// (`0` = `ok`, `1` = `err`) and the payload in `rdx` (the parsed `i64` on `ok`,
/// or a freshly-allocated error-message string record on `err`). The parse matches
/// Rust's `str::parse::<i64>()` exactly: an optional single leading `+`/`-`, then
/// one or more ASCII digits, no surrounding whitespace, and a checked base-10
/// accumulation so an out-of-range value is an `err`. The error message is the same
/// fixed `` cannot parse `{text}` as i64 `` the interpreters produce.
const PARSE_I64_SYMBOL: &str = "__lullaby_parse_i64";

/// The `split` helper emitted in `.text`. Signature: the text record pointer in
/// `rcx`, the separator record pointer in `rdx`; returns in `rax` a fresh
/// `list<string>`-layout block (`[len][cap][slot…]`) of the fields, matching the
/// interpreters' `text.split(sep)` (leading/trailing/consecutive separators yield
/// empty fields; an empty input yields one empty field). Composed from the tested
/// `__lullaby_str_count`/`_find`/`_substring` helpers. An empty separator traps
/// with `ud2` (the interpreters' `L0417`).
const STR_SPLIT_SYMBOL: &str = "__lullaby_str_split";

/// The `join` helper emitted in `.text`. Signature: an `array<string>`
/// (`list<string>`-layout) block pointer in `rcx`, the separator record pointer in
/// `rdx`; returns in `rax` a fresh record joining the fields with the separator
/// between them, matching the interpreters' `parts.join(sep)`. An empty array
/// yields the empty string.
const STR_JOIN_SYMBOL: &str = "__lullaby_str_join";

/// Whether `ty` is the native heap `string` type. A string value is a single
/// pointer word (like a list/map) but immutable, so it needs no deep copy.
fn is_string_type(ty: &TypeRef) -> bool {
    ty.name == "string"
}

/// `.rdata` section characteristics: initialized, read-only data.
/// `IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ`.
const RDATA_CHARACTERISTICS: u32 = 0x4000_0040;

/// `.bss` section characteristics: uninitialized data, read + write.
/// `IMAGE_SCN_CNT_UNINITIALIZED_DATA | IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE`.
const BSS_CHARACTERISTICS: u32 = 0xC000_0080;

/// COFF relocation type for a 32-bit PC-relative reference to a symbol, used for
/// `call rel32` and `jmp rel32` targeting another symbol (`IMAGE_REL_AMD64_REL32`).
const IMAGE_REL_AMD64_REL32: u16 = 0x0004;

/// COFF relocation type for a 32-bit offset of a symbol from the start of its own
/// section (`IMAGE_REL_AMD64_SECREL`). Used by CodeView `DEBUG_S_LINES`
/// subsections to point at a function's `.text` offset.
const IMAGE_REL_AMD64_SECREL: u16 = 0x000B;

/// COFF relocation type for the 16-bit section index of a symbol
/// (`IMAGE_REL_AMD64_SECTION`). Paired with `SECREL` in CodeView line subsections.
const IMAGE_REL_AMD64_SECTION: u16 = 0x000A;

/// `.debug$S` section characteristics: initialized data, read-only, discardable,
/// 1-byte aligned. `IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ |
/// IMAGE_SCN_MEM_DISCARDABLE | IMAGE_SCN_ALIGN_1BYTES`.
const DEBUG_S_CHARACTERISTICS: u32 = 0x4210_0040;

/// CodeView `.debug$S` section signature (`CV_SIGNATURE_C13`).
const CV_SIGNATURE_C13: u32 = 4;

/// CodeView subsection kind: a symbol subsection (`DEBUG_S_SYMBOLS`).
const DEBUG_S_SYMBOLS: u32 = 0xF1;
/// CodeView subsection kind: a line-number subsection (`DEBUG_S_LINES`).
const DEBUG_S_LINES: u32 = 0xF2;
/// CodeView subsection kind: the source-file checksum table (`DEBUG_S_FILECHKSMS`).
const DEBUG_S_FILECHKSMS: u32 = 0xF4;
/// CodeView subsection kind: the string table (`DEBUG_S_STRINGTABLE`).
const DEBUG_S_STRINGTABLE: u32 = 0xF3;

/// CodeView symbol record kind `S_COMPILE3` (compiler/environment record). A
/// minimal one is emitted so the `.debug$S` stream is a well-formed CodeView
/// symbol subsection.
const S_COMPILE3: u16 = 0x113C;

/// Options for emitting native source-line debug info (`lullaby native --debug`).
///
/// When present, the emitter adds a CodeView `.debug$S` section carrying a
/// per-function line-number table that maps each compiled function's entry code
/// offset to its `.lby` source declaration line, plus the source file name. A
/// debugger (or `llvm-pdbutil`) can then place a breakpoint at a function and
/// show the corresponding source line. Without these options the object bytes are
/// byte-for-byte unchanged (no `.debug$S` section), so existing snapshot and
/// structural tests are unaffected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugOptions {
    /// The `.lby` source file path recorded in the CodeView file table. Shown by a
    /// debugger as the source file for the compiled functions.
    pub source_file: String,
}

/// Emit a linkable COFF object for the i64-scalar-subset functions of `module`.
///
/// Eligible functions (all params + return are `i64`, at most four params, and a
/// body built from the supported subset) are lowered to x86-64. An entry stub
/// calls `main` and forwards its result to `ExitProcess`. Ineligible functions
/// are recorded in `skipped`. If no function is eligible, returns an error with
/// code `L0339`.
///
/// This is the default (no debug info) entry point; see
/// [`emit_native_program_with_debug`] to additionally emit CodeView
/// source-line debug info.
pub fn emit_native_program(module: &BytecodeModule) -> Result<NativeProgram, NativeProgramError> {
    emit_native_program_with_debug(module, None)
}

/// Like [`emit_native_program`], but when `debug` is `Some`, additionally
/// emits a CodeView `.debug$S` section with per-function source-line info (see
/// [`DebugOptions`]). When `debug` is `None` the emitted object bytes are exactly
/// those of [`emit_native_program`].
pub fn emit_native_program_with_debug(
    module: &BytecodeModule,
    debug: Option<&DebugOptions>,
) -> Result<NativeProgram, NativeProgramError> {
    emit_native_program_for_target(module, &x86_64_windows_target(), debug, false)
}

/// Emit a native program for an explicit `target`, selecting the object-file
/// container by the target's [`NativeObjectFormat`]:
///
/// - `x86_64-pc-windows-msvc` → COFF (the default; byte-for-byte unchanged),
/// - `x86_64-unknown-linux-gnu` → ELF64 (System V AMD64), and
/// - `x86_64-apple-darwin` → Mach-O x86-64.
///
/// The x86-64 machine code and the internal calling convention are identical
/// across all three; only the object wrapper and the entry/exit stub differ (a
/// freestanding `exit` syscall on Linux/macOS instead of `kernel32!ExitProcess`).
/// The ELF and Mach-O objects are relocatable objects verified structurally on
/// this host; link-and-run verification is deferred to the Phase 9 cross-platform
/// CI. See `documents/native_backend_contract.md`.
pub fn emit_native_program_for_target(
    module: &BytecodeModule,
    target: &NativeTarget,
    debug: Option<&DebugOptions>,
    fast_math: bool,
) -> Result<NativeProgram, NativeProgramError> {
    let target = target.clone();

    // AArch64 is a distinct instruction set with its own code generator: it
    // consumes the same `BytecodeModule` but emits AArch64 machine code and an
    // aarch64 ELF object (see `crate::aarch64`). The `--debug` CodeView line
    // table is an x86-64/COFF feature and does not apply to the AArch64 core.
    if matches!(target.architecture, NativeArchitecture::Aarch64) {
        return crate::aarch64::emit_aarch64_program(module, &target);
    }

    // First pass: decide signature eligibility. Calls resolve against the set of
    // names we intend to compile.
    let mut skipped: Vec<NativeSkippedFunction> = Vec::new();
    let mut eligible_names: Vec<String> = Vec::new();
    for function in &module.functions {
        match native_signature_eligibility(function, &module.structs, &module.enums) {
            Ok(()) => eligible_names.push(function.name.clone()),
            Err(reason) => skipped.push(NativeSkippedFunction {
                name: function.name.clone(),
                reason,
            }),
        }
    }

    // Second pass: lower each eligible body. A lowering failure demotes the
    // function to skipped and drops it from the callable set, then re-runs (a
    // call to a demoted function must also fail). Converges quickly.
    loop {
        // Compiled functions plus every declared `extern fn` are callable. An
        // extern name resolves to an undefined external symbol (bound by the
        // linker) rather than a compiled `.text` function.
        let mut callable: std::collections::HashSet<&str> =
            eligible_names.iter().map(String::as_str).collect();
        for name in &module.extern_functions {
            callable.insert(name.as_str());
        }
        // C-ABI signatures for the declared externs, keyed by name, so an extern
        // call marshals its arguments/return to the correct C scalar widths.
        let extern_sigs: HashMap<&str, &crate::IrExternSignature> = module
            .extern_signatures
            .iter()
            .map(|sig| (sig.name.as_str(), sig))
            .collect();
        let mut lowered: Vec<LoweredNativeFunction> = Vec::new();
        let mut demoted: Option<NativeSkippedFunction> = None;
        // String constants are interned fresh each attempt so a demotion that
        // drops a function also drops any strings only it referenced.
        let mut strings = StringPool::default();

        // Infer array lengths for every eligible function's array-typed signature
        // slots (fixed arrays carry no length in their `array<T>` type), then
        // compute the native signatures (parameter + return layouts). A function
        // whose array slot cannot be sized consistently — or that would call a
        // function whose signature failed — is demoted and the loop retries.
        let mut array_lengths_by_fn: HashMap<String, ArrayLengths> = HashMap::new();
        let mut signatures: HashMap<String, NativeSignature> = HashMap::new();
        for name in &eligible_names {
            let function = module
                .functions
                .iter()
                .find(|f| &f.name == name)
                .expect("eligible name exists");
            let inference =
                infer_array_lengths(function, module, &eligible_names).and_then(|lengths| {
                    let sig = compute_native_signature(
                        function,
                        &module.structs,
                        &module.enums,
                        &lengths,
                    )?;
                    Ok((lengths, sig))
                });
            match inference {
                Ok((lengths, sig)) => {
                    array_lengths_by_fn.insert(name.clone(), lengths);
                    signatures.insert(name.clone(), sig);
                }
                Err(reason) => {
                    demoted = Some(NativeSkippedFunction {
                        name: name.clone(),
                        reason,
                    });
                    break;
                }
            }
        }
        if let Some(demoted) = demoted {
            eligible_names.retain(|n| n != &demoted.name);
            merge_native_skip(&mut skipped, demoted);
            continue;
        }

        for name in &eligible_names {
            let function = module
                .functions
                .iter()
                .find(|f| &f.name == name)
                .expect("eligible name exists");
            let array_lengths = &array_lengths_by_fn[name];
            match lower_native_function(
                function,
                &callable,
                &extern_sigs,
                &module.structs,
                &module.enums,
                &mut strings,
                &signatures,
                array_lengths,
                fast_math,
            ) {
                Ok(l) => lowered.push(l),
                Err(reason) => {
                    demoted = Some(NativeSkippedFunction {
                        name: name.clone(),
                        reason,
                    });
                    break;
                }
            }
        }

        if let Some(demoted) = demoted {
            eligible_names.retain(|n| n != &demoted.name);
            merge_native_skip(&mut skipped, demoted);
            continue;
        }

        let has_main = lowered.iter().any(|f| f.name == "main");
        // Whether any compiled function is a C-callable export. An export-only
        // program (no `main`) is a *library object*: it has no entry stub, so a C
        // (or other) `main` can link against it and call the exported symbols.
        let has_export = lowered
            .iter()
            .any(|f| module.export_functions.contains(&f.name));

        if lowered.is_empty() || (!has_main && !has_export) {
            // Nothing runnable and nothing exported: there is nothing to emit.
            let reason = if lowered.is_empty() {
                "no functions were eligible for the native i64-scalar subset".to_string()
            } else {
                "neither `main` nor an `export fn` is eligible for the native i64-scalar subset"
                    .to_string()
            };
            return Err(NativeProgramError {
                code: NATIVE_NO_ELIGIBLE_CODE,
                message: reason,
                skipped,
            });
        }

        let compiled: Vec<String> = lowered.iter().map(|f| f.name.clone()).collect();
        // Emit the entry stub only when a `main` is present. A pure export library
        // (no `main`) omits the stub entirely, so it carries no exit dependency
        // and does not collide with a C `main` at link time.
        //
        // The object container is selected by the target format: COFF keeps its
        // own byte-for-byte writer (and `kernel32!ExitProcess` entry stub); ELF
        // and Mach-O flow through the shared neutral object model with a
        // freestanding `exit`-syscall entry stub.
        let (bytes, entry_symbol) = match target.object_format {
            NativeObjectFormat::Coff => {
                let bytes = write_native_program_object(&lowered, &strings, has_main, debug);
                let entry = if has_main {
                    NATIVE_ENTRY_SYMBOL.to_string()
                } else {
                    String::new()
                };
                (bytes, entry)
            }
            NativeObjectFormat::Elf => {
                let model = build_object_model(&lowered, &strings, has_main, PlatformAbi::Linux);
                let entry = model.entry_symbol.clone().unwrap_or_default();
                (elf_object::write_elf64(&model), entry)
            }
            NativeObjectFormat::MachO => {
                let model = build_object_model(&lowered, &strings, has_main, PlatformAbi::MacOs);
                let entry = model.entry_symbol.clone().unwrap_or_default();
                (macho_object::write_macho64(&model), entry)
            }
        };
        // When the program declares any `extern fn`, the C runtime import library
        // must be linked so the external C symbols resolve.
        let import_libs = if module.extern_functions.is_empty() {
            Vec::new()
        } else {
            vec![C_RUNTIME_IMPORT_LIB.to_string()]
        };
        return Ok(NativeProgram {
            target,
            bytes,
            entry_symbol,
            compiled,
            skipped,
            import_libs,
        });
    }
}

fn merge_native_skip(skips: &mut Vec<NativeSkippedFunction>, skip: NativeSkippedFunction) {
    if !skips.iter().any(|s| s.name == skip.name) {
        skips.push(skip);
    }
}

/// Whether a signature type (a parameter type or the return type) is native and
/// whether it is an aggregate. A native **integer** scalar (`i64`/fixed-width/
/// `bool`/`char`/`byte`) passes/returns in an integer register; an aggregate (a
/// scalar-field struct, a fixed array of scalars, or a scalar-payload enum)
/// passes/returns by pointer per the aggregate ABI.
///
/// A top-level **float** (`f64`/`f32`) scalar parameter or return is a register
/// value: it passes/returns in the Win64 SSE registers (`xmm0..3` for arguments,
/// `xmm0` for the return), positionally aligned with the integer registers — so a
/// float at position N consumes `xmm N` while an integer at position N consumes
/// integer register N (never both). Float payloads *inside* a by-pointer aggregate
/// are copied as raw bit-preserving words. A heap-containing aggregate
/// (`string`/`list`/`map`, or an aggregate whose element/field is heap) is not
/// native and skips gracefully.
///
/// Returns `Ok(true)` for an aggregate, `Ok(false)` for an integer scalar, and
/// `Err` for a non-native / deferred type.
fn native_signature_type_is_aggregate(
    ty: &TypeRef,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
) -> Result<bool, String> {
    // A plain integer scalar is a register value, not an aggregate. It is
    // resolvable by `resolve_native_type` but we classify it here directly so an
    // `array<T>` (whose length is unknown from the type) is treated as an
    // aggregate rather than the length error `resolve_native_type` raises.
    if ty.name == "i64"
        || fixed_int_kind(&ty.name).is_some()
        || matches!(ty.name.as_str(), "bool" | "char" | "byte")
    {
        return Ok(false);
    }
    // A top-level float scalar (`f64`/`f32`) is a register value routed through
    // the Win64 SSE argument registers (`xmm0..3`), positionally aligned with the
    // integer registers. It is a scalar, not an aggregate.
    if matches!(ty.name.as_str(), "f64" | "f32") {
        return Ok(false);
    }
    // A heap `string` crosses a boundary as a single immutable pointer word in an
    // integer register (by value; no deep copy, since strings are immutable). It
    // is a scalar for the signature classification, not a by-pointer aggregate.
    if ty.name == "string" {
        return Ok(false);
    }
    // A fixed array parameter/return is an aggregate. Its element layout must be a
    // native (non-heap) type; the length is not needed for the signature check
    // (the callee copies whole words by count derived from the caller's value at
    // the call site — see the call/return ABI), so we only validate the element.
    if let Some(rest) = ty.name.strip_prefix("array<") {
        let elem_name = rest.strip_suffix('>').unwrap_or(rest);
        let elem_ty = TypeRef::new(elem_name);
        // Recurse: the element must itself be a native scalar or native aggregate.
        native_signature_type_is_aggregate(&elem_ty, structs, enums)?;
        return Ok(true);
    }
    // A scalar-element growable `list<T>` crosses a boundary as a single pointer
    // word in an integer register (by value, value-semantic — its mutators copy),
    // so it is a scalar for the signature classification, not a by-pointer
    // aggregate. A heap-element list is rejected by `resolve_native_type` below.
    if ty.name.starts_with("list<") {
        resolve_native_type(ty, structs, enums)?;
        return Ok(false);
    }
    // A scalar-key/value growable `map<K, V>` likewise crosses a boundary as a
    // single pointer word in an integer register (by value, value-semantic — its
    // only mutator copies), so it is a scalar for the signature classification. A
    // heap-key/value or float-key map is rejected by `resolve_native_type` below.
    if ty.name.starts_with("map<") {
        resolve_native_type(ty, structs, enums)?;
        return Ok(false);
    }
    // A struct or scalar-payload enum resolves to an aggregate layout; a heap type
    // (`string`) or a heap-containing aggregate fails to resolve and is rejected
    // here so the function skips gracefully.
    let native = resolve_native_type(ty, structs, enums)?;
    match native {
        NativeType::I64
        | NativeType::F64
        | NativeType::F32
        | NativeType::String
        | NativeType::List { .. }
        | NativeType::Map { .. } => Ok(false),
        // A `HeapStruct` is a collection-element-only representation and never
        // reaches a top-level signature type; treat it as a register pointer word
        // for completeness.
        NativeType::HeapStruct { .. } => Ok(false),
        NativeType::Struct { .. } | NativeType::Array { .. } | NativeType::Enum { .. } => Ok(true),
    }
}

/// Whether a function's signature is native-eligible. Scalars (`i64`, the fixed-
/// width integers, `bool`/`char`/`byte`, `f64`/`f32`) pass/return in a register;
/// scalar-field aggregates (structs, fixed arrays of scalars, scalar-payload
/// enums) pass/return **by pointer** (see the aggregate ABI). An aggregate return
/// consumes one integer register for the hidden result pointer, so the number of
/// *effective* register arguments (params + a hidden return pointer, if any) must
/// be at most four; otherwise, and for any non-native (heap-containing) type, the
/// function skips gracefully and runs on the interpreters.
fn native_signature_eligibility(
    function: &BytecodeFunction,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
) -> Result<(), String> {
    // Does the return type consume a hidden first integer-register argument?
    let returns_aggregate =
        native_signature_type_is_aggregate(&function.return_type, structs, enums).map_err(
            |reason| {
                format!(
                    "return type `{}` is not in the native subset: {reason}",
                    function.return_type.name
                )
            },
        )?;

    for param in &function.params {
        native_signature_type_is_aggregate(&param.ty, structs, enums).map_err(|reason| {
            format!(
                "parameter `{}` type `{}` is not in the native subset: {reason}",
                param.name, param.ty.name
            )
        })?;
    }

    // The hidden return pointer (if any) plus each parameter fill the four Win64
    // register slots (`rcx`/`rdx`/`r8`/`r9`, with floats positionally in
    // `xmm0..3`); the 5th+ effective argument is passed on the stack above the
    // callee's shadow space (see the stack-argument ABI). `main`'s scalar `i64`
    // return path is unaffected (no hidden pointer). There is therefore no
    // fixed arity cap; every effective argument beyond the fourth spills to the
    // stack. `returns_aggregate` is referenced here to document the effective
    // count even though it no longer gates eligibility.
    let _effective_args = function.params.len() + usize::from(returns_aggregate);
    Ok(())
}

// -- Array-length inference for signatures -----------------------------------
//
// A fixed array's length is absent from its `array<T>` type, so a function that
// takes or returns one has its length inferred: a returned array's length comes
// from the function's returned array value; a parameter array's length comes
// from every call site's argument in that position, which must all agree. A
// length that cannot be determined (or that disagrees across callers) demotes
// the function so it runs on the interpreters rather than miscompiling.

/// Compute a function's array-length environment: for each array-typed parameter
/// and (if array-typed) the return slot, the concrete element count. A function
/// with no array signature slots yields an empty map. An unsizable array slot is
/// an error the caller turns into a skip.
fn infer_array_lengths(
    function: &BytecodeFunction,
    module: &BytecodeModule,
    eligible_names: &[String],
) -> Result<ArrayLengths, String> {
    let mut lengths = ArrayLengths::new();

    // Return array: length taken from the function's returned array value(s). A
    // heap-backed `array<string>` return is a pointer word (a `list<string>` block)
    // and needs no length.
    if function.return_type.name.starts_with("array<")
        && heap_string_array_element(&function.return_type).is_none()
    {
        let len = infer_return_array_len(function).ok_or_else(|| {
            format!(
                "return array length of `{}` could not be inferred (return an array literal \
                 or a fixed array local)",
                function.name
            )
        })?;
        lengths.insert(RETURN_ARRAY_KEY.to_string(), len);
    }

    // Parameter arrays: every call site's argument in that position must resolve
    // to the same length. A function that is never called (e.g. an unreferenced
    // helper) has no callers to size its array params, so it is demoted.
    for (index, param) in function.params.iter().enumerate() {
        // A heap-backed `array<string>` param is a pointer word (a `list<string>`
        // block), not a stack array, so it needs no inferred length.
        if !param.ty.name.starts_with("array<") || heap_string_array_element(&param.ty).is_some() {
            continue;
        }
        let mut found: Option<usize> = None;
        let mut saw_call = false;
        for caller in &module.functions {
            if !eligible_names.contains(&caller.name) {
                continue;
            }
            collect_call_arg_lengths(
                &caller.instructions,
                caller,
                &function.name,
                index,
                &mut found,
                &mut saw_call,
            )?;
        }
        if !saw_call {
            return Err(format!(
                "parameter array `{}` of `{}` has no call site to infer its length from",
                param.name, function.name
            ));
        }
        let len = found.ok_or_else(|| {
            format!(
                "parameter array `{}` of `{}` could not be sized from its call sites",
                param.name, function.name
            )
        })?;
        lengths.insert(param.name.clone(), len);
    }

    Ok(lengths)
}

/// Infer the element count of a function's returned array from its returned
/// array values (an explicit `return <arr>`, or a tail array expression). All
/// returned arrays must agree; a disagreement or an unsizable value yields `None`.
fn infer_return_array_len(function: &BytecodeFunction) -> Option<usize> {
    let mut result: Option<usize> = None;
    fn visit(
        body: &[BytecodeInstruction],
        function: &BytecodeFunction,
        result: &mut Option<usize>,
        ok: &mut bool,
    ) {
        for stmt in body {
            match stmt {
                BytecodeInstruction::Return(Some(expr)) | BytecodeInstruction::Expr(expr) => {
                    if let Some(len) = array_len_of_expr(expr, function) {
                        match result {
                            Some(existing) if *existing != len => *ok = false,
                            _ => *result = Some(len),
                        }
                    } else if matches!(
                        &expr.kind,
                        BytecodeExprKind::Array(_) | BytecodeExprKind::Variable(_)
                    ) {
                        // An array-valued return whose length we cannot read.
                        *ok = false;
                    }
                }
                BytecodeInstruction::If {
                    branches,
                    else_body,
                    ..
                } => {
                    for branch in branches {
                        visit(&branch.body, function, result, ok);
                    }
                    visit(else_body, function, result, ok);
                }
                BytecodeInstruction::While { body, .. }
                | BytecodeInstruction::Loop { body, .. }
                | BytecodeInstruction::For { body, .. } => visit(body, function, result, ok),
                BytecodeInstruction::Match { arms, .. } => {
                    for arm in arms {
                        visit(&arm.body, function, result, ok);
                    }
                }
                _ => {}
            }
        }
    }
    let mut ok = true;
    visit(&function.instructions, function, &mut result, &mut ok);
    if ok { result } else { None }
}

/// The element count of an array-valued expression within `function`'s context:
/// a direct array literal, or a variable bound to a fixed array local (its `let`
/// initializer array literal). Returns `None` for anything else.
fn array_len_of_expr(expr: &BytecodeExpr, function: &BytecodeFunction) -> Option<usize> {
    match &expr.kind {
        BytecodeExprKind::Array(elements) => Some(elements.len()),
        BytecodeExprKind::Variable(name) => local_array_len(&function.instructions, name),
        _ => None,
    }
}

/// Find the array length of a local `name` bound by a `let name array<...> = [..]`
/// anywhere in a body (including nested blocks). Returns `None` if not an array
/// local with a literal initializer.
fn local_array_len(body: &[BytecodeInstruction], name: &str) -> Option<usize> {
    for stmt in body {
        match stmt {
            BytecodeInstruction::Let {
                name: n, ty, value, ..
            } if n == name && ty.name.starts_with("array<") => {
                if let BytecodeExprKind::Array(elements) = &value.kind {
                    return Some(elements.len());
                }
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    if let Some(len) = local_array_len(&branch.body, name) {
                        return Some(len);
                    }
                }
                if let Some(len) = local_array_len(else_body, name) {
                    return Some(len);
                }
            }
            BytecodeInstruction::While { body, .. }
            | BytecodeInstruction::Loop { body, .. }
            | BytecodeInstruction::For { body, .. } => {
                if let Some(len) = local_array_len(body, name) {
                    return Some(len);
                }
            }
            BytecodeInstruction::Match { arms, .. } => {
                for arm in arms {
                    if let Some(len) = local_array_len(&arm.body, name) {
                        return Some(len);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Scan a caller body for calls to `callee`, reading the array length of the
/// argument at `arg_index`. Every such call must agree on the length; a
/// disagreement or an unsizable argument is an error the caller turns into a skip.
fn collect_call_arg_lengths(
    body: &[BytecodeInstruction],
    caller: &BytecodeFunction,
    callee: &str,
    arg_index: usize,
    found: &mut Option<usize>,
    saw_call: &mut bool,
) -> Result<(), String> {
    fn visit_expr(
        expr: &BytecodeExpr,
        caller: &BytecodeFunction,
        callee: &str,
        arg_index: usize,
        found: &mut Option<usize>,
        saw_call: &mut bool,
    ) -> Result<(), String> {
        if let BytecodeExprKind::Call { name, args } = &expr.kind
            && name == callee
        {
            *saw_call = true;
            let arg = args
                .get(arg_index)
                .ok_or_else(|| format!("call to `{callee}` is missing argument {arg_index}"))?;
            let len = array_len_of_expr(arg, caller).ok_or_else(|| {
                format!(
                    "call to `{callee}` passes an array argument whose length is not \
                     statically known"
                )
            })?;
            match found {
                Some(existing) if *existing != len => {
                    return Err(format!(
                        "call sites of `{callee}` disagree on array argument {arg_index} length \
                         ({existing} vs {len})"
                    ));
                }
                _ => *found = Some(len),
            }
        }
        for child in expr_children(expr) {
            visit_expr(child, caller, callee, arg_index, found, saw_call)?;
        }
        Ok(())
    }
    for stmt in body {
        match stmt {
            BytecodeInstruction::Let { value, .. }
            | BytecodeInstruction::Assign { value, .. }
            | BytecodeInstruction::Return(Some(value))
            | BytecodeInstruction::Expr(value) => {
                visit_expr(value, caller, callee, arg_index, found, saw_call)?;
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    visit_expr(
                        &branch.condition,
                        caller,
                        callee,
                        arg_index,
                        found,
                        saw_call,
                    )?;
                    collect_call_arg_lengths(
                        &branch.body,
                        caller,
                        callee,
                        arg_index,
                        found,
                        saw_call,
                    )?;
                }
                collect_call_arg_lengths(else_body, caller, callee, arg_index, found, saw_call)?;
            }
            BytecodeInstruction::While {
                condition, body, ..
            } => {
                visit_expr(condition, caller, callee, arg_index, found, saw_call)?;
                collect_call_arg_lengths(body, caller, callee, arg_index, found, saw_call)?;
            }
            BytecodeInstruction::Loop { body, .. } | BytecodeInstruction::For { body, .. } => {
                collect_call_arg_lengths(body, caller, callee, arg_index, found, saw_call)?;
            }
            BytecodeInstruction::Match {
                scrutinee, arms, ..
            } => {
                visit_expr(scrutinee, caller, callee, arg_index, found, saw_call)?;
                for arm in arms {
                    collect_call_arg_lengths(
                        &arm.body, caller, callee, arg_index, found, saw_call,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Compute a function's native signature (parameter + return layouts) using the
/// inferred array lengths for its array-typed signature slots.
fn compute_native_signature(
    function: &BytecodeFunction,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    array_lengths: &ArrayLengths,
) -> Result<NativeSignature, String> {
    let mut params = Vec::with_capacity(function.params.len());
    for param in &function.params {
        params.push(resolve_signature_native_type(
            &param.ty,
            structs,
            enums,
            array_lengths,
            &param.name,
        )?);
    }
    let ret = resolve_signature_native_type(
        &function.return_type,
        structs,
        enums,
        array_lengths,
        RETURN_ARRAY_KEY,
    )?;
    Ok(NativeSignature { params, ret })
}

// -- Stack aggregate layout (all-i64 structs and fixed i64 arrays) -----------
//
// Locals in the extended subset may be an `i64` scalar, an all-i64 (possibly
// nested) struct, or a fixed-length array of any supported element type. Each
// such value is laid out contiguously in the function's stack frame as a run of
// 8-byte words: scalars occupy one word, a struct the concatenation of its
// (recursively flattened) field words, and an array `len` copies of its element
// layout. Aggregates never live in a register; instead operations resolve the
// `[rbp - slot]` displacement of an individual scalar word and load/store it.

/// The stack layout of a native local value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NativeType {
    /// A single 8-byte integer word.
    I64,
    /// A single 8-byte word holding an IEEE-754 `f64` (double). Lives in an XMM
    /// register as a `double` while live; spilled to its stack word as 8 bytes.
    F64,
    /// A single 8-byte word holding an IEEE-754 `f32` (single). Only the low four
    /// bytes are meaningful; the value is kept rounded to single precision after
    /// every operation, matching the interpreter's real `f32` storage.
    F32,
    /// A heap `string` value: a single 8-byte word holding a pointer to a
    /// `[char_len i64][byte_len i64][utf8 bytes]` record. It passes/returns in an
    /// integer register (by value, like a pointer). Unlike a list/map it is
    /// IMMUTABLE, so it needs no deep copy on a binding, argument, or return —
    /// sharing the pointer is value-equivalent, matching the interpreters and the
    /// WASM backend.
    String,
    /// A named struct whose fields are all supported native types, in order.
    Struct {
        name: String,
        fields: Vec<(String, NativeType)>,
    },
    /// A named struct laid out **on the heap**: a single 8-byte word holding a
    /// pointer to a `[field words...]` block (one 8-byte word per flattened field,
    /// in declared order). This is the representation a struct takes when it is a
    /// MUTABLE-heap collection element/value/enum payload (`list<struct>`,
    /// `map<K, struct>`, `option<struct>`) — the element slot is one word, so the
    /// struct value must be a pointer. Distinct from [`NativeType::Struct`], which
    /// is the stack-flattened representation used for struct locals/params/returns.
    /// The collection copy paths DEEP-COPY a `HeapStruct` element per value-semantic
    /// copy (mirroring the interpreters' recursive `Value::clone`), and `get`/`match`
    /// bridge a `HeapStruct` back into the stack-flattened `Struct` layout that field
    /// access and the by-pointer call ABI expect. `fields` are the flattened field
    /// (name, scalar/string/heap-aggregate layout) pairs, one word each.
    HeapStruct {
        name: String,
        fields: Vec<(String, NativeType)>,
    },
    /// A fixed-length array of a supported element type.
    Array { elem: Box<NativeType>, len: usize },
    /// A growable `list<T>` with a scalar element type. Represented as a single
    /// 8-byte word holding a heap pointer to a `[len i64][cap i64][slots]` block;
    /// it passes/returns in an integer register (by value, like a pointer) and is
    /// value-semantic because its mutators deep-copy their source (see the
    /// "Growable list layout (native)" comment). `elem` is the (scalar) element
    /// layout, used to keep the element word count exact and mirror the WASM
    /// backend.
    List { elem: Box<NativeType> },
    /// A growable `map<K, V>` with scalar key/value types. Represented as a single
    /// 8-byte word holding a heap pointer to a `[len i64][cap i64][entries]`
    /// block (each entry a `(key, value)` word pair); it passes/returns in an
    /// integer register (by value, like a pointer) and is value-semantic because
    /// its only mutator (`map_set`) deep-copies its source. `key`/`value` are the
    /// scalar element layouts, used to keep the value slot exact and mirror the
    /// WASM backend.
    Map {
        key: Box<NativeType>,
        value: Box<NativeType>,
    },
    /// A tagged enum whose variants all carry scalar payloads. Laid out as one
    /// tag word (the variant's discriminant index) followed by
    /// `payload_words` payload words (the maximum payload width across the
    /// variants). Each variant records its ordered scalar payload words for
    /// construction and `match` binding. The discriminant of a variant is its
    /// index in `variants`, matching the order the IR/interpreters use for that
    /// enum (declared order for a user enum; `some,none` / `ok,err` for the
    /// built-in generics).
    Enum {
        name: String,
        variants: Vec<NativeEnumVariant>,
        /// Max payload words across all variants (the payload region size).
        payload_words: usize,
    },
}

/// One variant of a native enum layout: its name, its discriminant index (the
/// tag value), and its ordered scalar payload word layouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeEnumVariant {
    name: String,
    tag: i64,
    payload: Vec<NativeType>,
}

impl NativeType {
    /// Whether this is an aggregate (struct / fixed array / enum) — a value that
    /// crosses a native function boundary **by pointer** — rather than a scalar
    /// (`i64`/fixed-width/`bool`/`char`/`byte`/`f64`/`f32`) that passes in a
    /// register.
    fn is_aggregate(&self) -> bool {
        matches!(
            self,
            NativeType::Struct { .. } | NativeType::Array { .. } | NativeType::Enum { .. }
        )
    }

    /// The number of 8-byte words this value occupies on the stack.
    fn words(&self) -> usize {
        match self {
            // A string, list, map, or heap struct is a single pointer word, like a
            // scalar.
            NativeType::I64
            | NativeType::F64
            | NativeType::F32
            | NativeType::String
            | NativeType::List { .. }
            | NativeType::Map { .. }
            | NativeType::HeapStruct { .. } => 1,
            NativeType::Struct { fields, .. } => fields.iter().map(|(_, t)| t.words()).sum(),
            NativeType::Array { elem, len } => elem.words() * len,
            // One tag word plus the shared payload region.
            NativeType::Enum { payload_words, .. } => 1 + payload_words,
        }
    }
}

/// The precision of a float value kept in an XMM register: an f64 `double` or an
/// f32 `single`. Selects the scalar SSE opcode family (`*sd` vs `*ss`) and drives
/// f32 single-precision rounding after each op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FloatWidth {
    F64,
    F32,
}

impl FloatWidth {
    /// The `FloatWidth` named by a Lullaby type name, or `None` for a non-float
    /// type. `f64`/`f32` are the only float types in the language.
    fn from_type_name(name: &str) -> Option<FloatWidth> {
        match name {
            "f64" => Some(FloatWidth::F64),
            "f32" => Some(FloatWidth::F32),
            _ => None,
        }
    }
}

/// Resolve a declared `TypeRef` into a `NativeType`. Arrays are not resolvable
/// from the type alone (their length is not encoded in `array<T>`); array
/// locals derive their layout from their initializer instead, so a bare
/// `array<...>` type reaching here is an error the caller turns into a skip.
fn resolve_native_type(
    ty: &TypeRef,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
) -> Result<NativeType, String> {
    match ty.name.as_str() {
        "i64" => Ok(NativeType::I64),
        // A fixed-width integer (`i8`…`usize`) is stored as its normalized `i64`
        // cell, so it occupies exactly one 8-byte word like `i64`.
        name if fixed_int_kind(name).is_some() => Ok(NativeType::I64),
        // `bool`/`char`/`byte` are single normalized `i64` cells (0/1 for bool,
        // the code point for char, the byte value for byte), so each occupies one
        // 8-byte word. These reach here only as enum payload types; a scalar local
        // of these types never selects this path (the frontend types them
        // directly), but sizing them here keeps the enum payload word count exact.
        "bool" | "char" | "byte" => Ok(NativeType::I64),
        // `f64`/`f32` each occupy one 8-byte word. An f32 keeps only its low four
        // bytes meaningful but is stored in a full word for uniform layout.
        "f64" => Ok(NativeType::F64),
        "f32" => Ok(NativeType::F32),
        // A heap `string`: a single pointer word to a `[char_len][byte_len][utf8]`
        // record. Immutable, so it passes/returns by value (pointer) with no copy.
        "string" => Ok(NativeType::String),
        // A raw pointer `ptr<T>` (or the legacy `ptr_T` spelling that `alloc`
        // produces) is a single 64-bit machine-address word. It flows through the
        // native backend exactly like an `i64` scalar — one stack word, passed and
        // returned in a GPR — so modeling it as `I64` reuses every scalar path
        // unchanged. Its only distinguished behavior is at the FFI boundary, where
        // it marshals to a C `T*` (see `emit_extern_call`).
        name if is_raw_pointer_type_name(name) => Ok(NativeType::I64),
        // A heap-backed `array<string>` — the `split`/`words` result and
        // `array<string>` literals — is a single pointer word to a `[len][cap]
        // [slot…]` block of shared immutable string pointers, laid out and
        // deep-copied exactly like a `list<string>`. (Scalar `array<i64>`/
        // `array<f64>` stay stack-flattened with a statically-inferred length,
        // handled by the `array<` arm below / the signature length path.)
        _ if heap_string_array_element(ty).is_some() => Ok(NativeType::List {
            elem: Box::new(NativeType::String),
        }),
        name if name.starts_with("array<") => Err(format!(
            "array length for `{name}` is unknown from its type"
        )),
        // A growable `list<T>` (scalar `T`): a single pointer word. The element
        // must be a native scalar; a heap element (`list<string>` etc.) is
        // deferred and rejected so the enclosing function skips gracefully.
        name if name.starts_with("list<") => {
            let elem = supported_list_element(ty).ok_or_else(|| {
                format!(
                    "list element of `{name}` is not a native scalar, `string`, \
                     one-level struct, or one-level nested list \
                     (deeper nesting and map/array elements are deferred)"
                )
            })?;
            // Depth-0 element classification: a scalar/string flat word, a struct
            // (`HeapStruct`), or a nested list — bounded to one mutable level.
            let elem_native = native_collection_slot(&elem, structs, enums, 0)
                .ok_or_else(|| format!("list element of `{name}` is not layable-out (deferred)"))?;
            Ok(NativeType::List {
                elem: Box::new(elem_native),
            })
        }
        // A growable `map<K, V>` (scalar key/value): a single pointer word. The key
        // must be an integer-cell scalar and the value a native scalar; a heap
        // key/value or float key is deferred and rejected so the enclosing
        // function skips gracefully.
        name if name.starts_with("map<") => {
            let (key, value) = supported_map_kv(ty).ok_or_else(|| {
                format!(
                    "map `{name}` key/value is not a supported native type \
                     (heap keys and float keys are deferred)"
                )
            })?;
            let key_native = resolve_native_type(&key, structs, enums)?;
            // Value classification: a scalar/string flat word, a struct
            // (`HeapStruct`), or a nested list — bounded to one mutable level.
            let value_native = native_collection_slot(&value, structs, enums, 0)
                .ok_or_else(|| format!("map value of `{name}` is not layable-out (deferred)"))?;
            Ok(NativeType::Map {
                key: Box::new(key_native),
                value: Box::new(value_native),
            })
        }
        // The built-in generic enums and user enums with scalar payloads.
        name if is_enum_type_name(name, enums) => resolve_enum_type(ty, structs, enums),
        name => {
            let def = structs
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| format!("type `{name}` is not in the native stack subset"))?;
            let mut fields = Vec::with_capacity(def.fields.len());
            for (field_name, field_ty) in &def.fields {
                let native = resolve_native_type(field_ty, structs, enums).map_err(|_| {
                    format!("struct `{name}` field `{field_name}` is not an all-i64 native type")
                })?;
                // An `f32` field is out of scope: the aggregate copy/pass paths move
                // whole 8-byte words through a GPR, which would not keep a 4-byte
                // f32 rounded. An `f64` is a full 8-byte word, so a GPR round-trip is
                // bit-lossless — f64 fields ARE supported (read/store route through
                // the float lowerer via `resolve_*_typed` + `float_width_of_expr`'s
                // Field arm; init/copy move the word unchanged). Reject only f32.
                if matches!(native, NativeType::F32) {
                    return Err(format!(
                        "struct `{name}` field `{field_name}` is an f32; f32 struct fields are not in the native subset (an f64 field is fine)"
                    ));
                }
                // A heap-value field (`string`/`list`/`map`) inside an aggregate is
                // deferred: the aggregate copy/pass paths move flat words and would
                // share (not deep-copy) the referenced heap block, breaking value
                // semantics for a mutable list/map field. A string field is
                // immutable but still out of this increment's scope (aggregates of
                // heap values are deferred), so reject it too — the function skips.
                if matches!(
                    native,
                    NativeType::String | NativeType::List { .. } | NativeType::Map { .. }
                ) {
                    return Err(format!(
                        "struct `{name}` field `{field_name}` is a heap value \
                         (string/list/map); heap-value struct fields are not in the native subset"
                    ));
                }
                fields.push((field_name.clone(), native));
            }
            Ok(NativeType::Struct {
                name: name.to_string(),
                fields,
            })
        }
    }
}

/// The base enum-constructor name of a type spelling: `option` for `option<i64>`
/// or a bare `option`, `result` for `result<i64, i64>`, and the enum name for a
/// user enum spelling (which never carries generic arguments).
fn enum_ctor_name(name: &str) -> &str {
    match name.split_once('<') {
        Some((ctor, _)) => ctor,
        None => name,
    }
}

/// Whether a type spelling names an enum: the built-in `option`/`result`
/// generics (with or without arguments) or a declared user enum.
fn is_enum_type_name(name: &str, enums: &[IrEnumDef]) -> bool {
    let ctor = enum_ctor_name(name);
    ctor == "option" || ctor == "result" || enums.iter().any(|e| e.name == ctor)
}

/// Resolve an enum type spelling into its native layout: the ordered variants
/// (each with its discriminant tag and scalar payload word layouts) and the
/// shared payload region width. The tag of a variant is its index in the
/// interpreter/IR variant order: declared order for a user enum, and
/// `some`(0)/`none`(1) for `option`, `ok`(0)/`err`(1) for `result`.
fn resolve_enum_type(
    ty: &TypeRef,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
) -> Result<NativeType, String> {
    let ctor = enum_ctor_name(&ty.name);
    // The ordered (variant name, payload types) pairs for this enum.
    let variant_specs: Vec<(String, Vec<TypeRef>)> = match ctor {
        "option" => {
            // `option<T>`: `some(T)` then `none`. The payload type is only known
            // when the spelling carries its argument (`option<i64>`); a bare
            // `option` is not sizable and is rejected so the function skips.
            let elem = ty.option_element().ok_or_else(|| {
                "native enum layout needs the concrete `option<T>` element type".to_string()
            })?;
            vec![
                ("some".to_string(), vec![elem]),
                ("none".to_string(), vec![]),
            ]
        }
        "result" => {
            // `result<T, E>`: `ok(T)` then `err(E)`. Both arguments must be
            // present and scalar; a heap `E` (e.g. `string`) is rejected below.
            let (ok_ty, err_ty) = ty.result_args().ok_or_else(|| {
                "native enum layout needs the concrete `result<T, E>` argument types".to_string()
            })?;
            vec![
                ("ok".to_string(), vec![ok_ty]),
                ("err".to_string(), vec![err_ty]),
            ]
        }
        _ => {
            let def = enums
                .iter()
                .find(|e| e.name == ctor)
                .ok_or_else(|| format!("enum `{ctor}` is not declared"))?;
            def.variants
                .iter()
                .map(|v| (v.name.clone(), v.payload.clone()))
                .collect()
        }
    };

    let mut variants = Vec::with_capacity(variant_specs.len());
    let mut payload_words = 0usize;
    for (tag, (name, payload_types)) in variant_specs.into_iter().enumerate() {
        let mut payload = Vec::with_capacity(payload_types.len());
        for payload_ty in &payload_types {
            // A scalar or `string` payload is supported: an `i64`/fixed-width/bool/
            // char/byte cell (`NativeType::I64`), a float (`F64`/`F32`), or a
            // `string` (`NativeType::String`, an immutable heap pointer in one
            // slot, shared on the flat word-copy deep copy since strings are
            // immutable — so `option<string>`, `result<i64, string>`, and user
            // enums with a string payload are supported). A one-level MUTABLE
            // aggregate payload — a `struct` (`HeapStruct`) or a nested
            // `list<scalar|string>` (`List`) — is now also supported: it occupies
            // one payload pointer word and is DEEP-COPIED on the enum's
            // value-semantic copy (so `option<struct>` — the `map_get` result on a
            // `map<K, struct>` — and `result<i64, list<i64>>` lay out). Deeper
            // nesting and a `map`/`array` payload stay deferred.
            let native = if is_scalar_or_string_slot(payload_ty) {
                resolve_native_type(payload_ty, structs, enums).map_err(|_| {
                    format!(
                        "enum `{ctor}` variant `{name}` payload type `{}` is not a native scalar",
                        payload_ty.name
                    )
                })?
            } else {
                native_collection_slot(payload_ty, structs, enums, 0).ok_or_else(|| {
                    format!(
                        "enum `{ctor}` variant `{name}` payload type `{}` is not a native scalar, \
                         `string`, or one-level mutable aggregate (deeper payloads are deferred)",
                        payload_ty.name
                    )
                })?
            };
            match native {
                NativeType::I64
                | NativeType::F64
                | NativeType::F32
                | NativeType::String
                | NativeType::HeapStruct { .. }
                | NativeType::List { .. } => payload.push(native),
                _ => {
                    return Err(format!(
                        "enum `{ctor}` variant `{name}` has an unsupported payload type"
                    ));
                }
            }
        }
        let this_words: usize = payload.iter().map(NativeType::words).sum();
        payload_words = payload_words.max(this_words);
        variants.push(NativeEnumVariant {
            name,
            tag: tag as i64,
            payload,
        });
    }

    Ok(NativeType::Enum {
        name: ctor.to_string(),
        variants,
        payload_words,
    })
}

/// Infer the `NativeType` of an initializer expression, using its static type
/// plus (for array literals) the literal element count. This is how array
/// lengths enter the layout, since `array<T>` carries no length.
fn native_type_of_init(
    expr: &BytecodeExpr,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    signatures: &HashMap<String, NativeSignature>,
) -> Result<NativeType, String> {
    if let BytecodeExprKind::Array(elements) = &expr.kind {
        let first = elements
            .first()
            .ok_or("empty array literals are not in the native stack subset")?;
        let elem = native_type_of_init(first, structs, enums, signatures)?;
        for other in &elements[1..] {
            let other_ty = native_type_of_init(other, structs, enums, signatures)?;
            if other_ty != elem {
                return Err("array literal elements have differing native layouts".to_string());
            }
        }
        return Ok(NativeType::Array {
            elem: Box::new(elem),
            len: elements.len(),
        });
    }
    // An array local bound from a call takes its length from the callee's inferred
    // return layout (the `array<T>` type alone carries no length).
    if expr.ty.name.starts_with("array<")
        && let BytecodeExprKind::Call { name, .. } = &expr.kind
        && let Some(sig) = signatures.get(name)
        && matches!(sig.ret, NativeType::Array { .. })
    {
        return Ok(sig.ret.clone());
    }
    resolve_native_type(&expr.ty, structs, enums)
}

/// The concrete word length of an array-typed signature slot, keyed by parameter
/// name; the return array (if any) uses the reserved key `RETURN_ARRAY_KEY`.
/// Fixed arrays carry no length in their `array<T>` type, so a function that
/// passes or returns one has its length inferred (see [`infer_array_lengths`])
/// and pinned here so the callee's copy-in / hidden-return-write knows the count.
type ArrayLengths = HashMap<String, usize>;

/// Reserved [`ArrayLengths`] key for a function's return-array length.
const RETURN_ARRAY_KEY: &str = "\0return";

/// Resolve a **signature** type (a parameter or return type) into its
/// `NativeType`. Identical to [`resolve_native_type`] except a fixed-array type
/// (`array<T>`), whose length is absent from the type, takes its length from
/// `array_lengths[key]` (populated by [`infer_array_lengths`]). A bare array with
/// no inferred length is rejected so the function skips gracefully.
fn resolve_signature_native_type(
    ty: &TypeRef,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    array_lengths: &ArrayLengths,
    key: &str,
) -> Result<NativeType, String> {
    // A heap-backed `array<string>` param/return is a pointer word (a `list<string>`
    // block), not a stack-flattened array, so it needs no inferred length.
    if heap_string_array_element(ty).is_some() {
        return Ok(NativeType::List {
            elem: Box::new(NativeType::String),
        });
    }
    if let Some(rest) = ty.name.strip_prefix("array<") {
        let elem_name = rest.strip_suffix('>').unwrap_or(rest);
        let elem = resolve_signature_native_type(
            &TypeRef::new(elem_name),
            structs,
            enums,
            array_lengths,
            key,
        )?;
        let len = *array_lengths.get(key).ok_or_else(|| {
            format!("array length for signature slot `{key}` could not be inferred")
        })?;
        return Ok(NativeType::Array {
            elem: Box::new(elem),
            len,
        });
    }
    resolve_native_type(ty, structs, enums)
}

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
/// displacement of its first word. Additional words follow at `slot - 8`,
/// `slot - 16`, ... (i.e. lower displacements — the frame grows downward but we
/// key by positive displacement from `rbp`, so word `k` is at `slot - 8*k`).
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
    ) -> Result<Self, String> {
        let mut locals: HashMap<String, NativeLocal> = HashMap::new();
        let mut next_slot: i32 = 0;

        // Return classification: an aggregate return is written through a hidden
        // pointer passed in the first integer-argument register (Win64 `rcx`),
        // shifting the visible parameters to the following registers.
        let return_ty = resolve_signature_native_type(
            &function.return_type,
            structs,
            enums,
            array_lengths,
            RETURN_ARRAY_KEY,
        )?;
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
            locals.insert(
                param.name.clone(),
                NativeLocal {
                    slot: next_slot - (words - 1) * 8,
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
        )?;

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
        let (promoted, saved_regs) = plan_register_promotion(function, &locals);
        let mut saved_reg_slots = Vec::new();
        for reg in saved_regs {
            next_slot += 8;
            saved_reg_slots.push((reg, next_slot));
        }

        let has_call = body_has_call(&function.instructions);
        // Reserve local slots plus (if calling) 32 bytes of shadow space, plus an
        // outgoing stack-argument area for any call passing more than four
        // effective register arguments. The area lives at the bottom of the frame
        // (lowest addresses, where `rsp` points at a `call`): `[rsp .. rsp+32]` is
        // the shadow, `[rsp+32 .. rsp+32+8*out]` holds the 5th+ arguments.
        let out_words = max_outgoing_stack_words(&function.instructions, signatures);
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
        })
    }

    /// The register a local slot is promoted into, if any.
    fn promoted_reg(&self, slot: i32) -> Option<PReg> {
        self.promoted.get(&slot).copied()
    }

    /// Allocate `words` contiguous scratch words, returning the base slot of the
    /// first word. Used to spill a temporary enum scrutinee. The cursor advances;
    /// callers restore it after the match via the returned saved cursor.
    fn alloc_scratch(&mut self, words: usize) -> i32 {
        let base = self.scratch_next;
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

/// A resolved scalar destination inside an aggregate. A scalar word lives at
/// `[rbp - disp]` where `disp = base_slot + 8 * (const_words + dynamic_words)`;
/// `dynamic_words` is `elem_words * index` computed at runtime when the path
/// crossed a runtime array index, else zero.
pub(crate) enum ScalarPlace {
    /// A fully static scalar word at `[rbp - slot]`.
    Const { slot: i32 },
    /// A dynamic scalar word. `base_slot` is the enclosing local's first word;
    /// `const_words` accumulates the static word offset from field hops and
    /// constant indices; `elem_words` is the per-element word stride of the
    /// dynamic array; `index_len` is the element count of the array the runtime
    /// index selects into (its static length), used to emit a bounds check; and
    /// the runtime index expression selects the element.
    Dynamic {
        base_slot: i32,
        const_words: i64,
        elem_words: i64,
        index_len: i64,
        index: BytecodeExpr,
    },
}

/// Recursively collect `let`/`for` locals, assigning each contiguous 8-byte-word
/// slots sized by its `NativeType`. `let` locals with an aggregate layout reserve
/// one word per flattened scalar; `for` counters and their hidden bound/step are
/// single i64 words.
fn collect_native_locals(
    body: &[BytecodeInstruction],
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    signatures: &HashMap<String, NativeSignature>,
    locals: &mut HashMap<String, NativeLocal>,
    next_slot: &mut i32,
) -> Result<(), String> {
    for instruction in body {
        match instruction {
            BytecodeInstruction::Let {
                name, ty, value, ..
            } => {
                if !locals.contains_key(name) {
                    let native = if ty.name.starts_with("array<") {
                        native_type_of_init(value, structs, enums, signatures)?
                    } else {
                        resolve_native_type(ty, structs, enums)?
                    };
                    let words = native.words() as i32;
                    *next_slot += words * 8;
                    locals.insert(
                        name.clone(),
                        NativeLocal {
                            slot: *next_slot - (words - 1) * 8,
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
                collect_native_locals(body, structs, enums, signatures, locals, next_slot)?;
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
                    )?;
                }
                collect_native_locals(else_body, structs, enums, signatures, locals, next_slot)?;
            }
            BytecodeInstruction::While { body, .. } | BytecodeInstruction::Loop { body, .. } => {
                collect_native_locals(body, structs, enums, signatures, locals, next_slot)?;
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
                                locals.insert(
                                    binding.clone(),
                                    NativeLocal {
                                        slot: *next_slot - (words - 1) * 8,
                                        ty: bound_ty,
                                    },
                                );
                            }
                        }
                    }
                    collect_native_locals(
                        &arm.body, structs, enums, signatures, locals, next_slot,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The maximum scratch words a `match` in this body needs for a temporary
/// (non-plain-local) enum scrutinee. A match whose scrutinee is a plain local
/// dispatches in place and needs no scratch; any other scrutinee (a call
/// result, a freshly-constructed enum, an aggregate access) is spilled to a
/// scratch region sized to its enum layout. Recurses through nested bodies so
/// the single shared scratch region is sized to the widest such scrutinee.
fn max_match_scratch_words(
    body: &[BytecodeInstruction],
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
) -> Result<usize, String> {
    let mut max = 0usize;
    for instruction in body {
        let nested = match instruction {
            BytecodeInstruction::Match {
                scrutinee, arms, ..
            } => {
                let mut here = 0usize;
                if !matches!(scrutinee.kind, BytecodeExprKind::Variable(_)) {
                    let layout = resolve_native_type(&scrutinee.ty, structs, enums)?;
                    here = layout.words();
                }
                for arm in arms {
                    here = here.max(max_match_scratch_words(&arm.body, structs, enums)?);
                }
                here
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                let mut here = max_match_scratch_words(else_body, structs, enums)?;
                for branch in branches {
                    here = here.max(max_match_scratch_words(&branch.body, structs, enums)?);
                }
                here
            }
            BytecodeInstruction::While { body, .. }
            | BytecodeInstruction::Loop { body, .. }
            | BytecodeInstruction::For { body, .. } => {
                max_match_scratch_words(body, structs, enums)?
            }
            _ => 0,
        };
        max = max.max(nested);
    }
    Ok(max)
}

/// The maximum scratch words any single call in this body needs for its
/// by-pointer aggregate arguments. Each aggregate argument of a call is
/// materialized into scratch before its address is passed, so a call's scratch
/// need is the sum of its aggregate arguments' words; the shared scratch region
/// is sized to the widest single call across the function (and nested calls in
/// argument position, handled recursively). Non-aggregate arguments need none.
fn max_call_arg_scratch_words(
    body: &[BytecodeInstruction],
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    signatures: &HashMap<String, NativeSignature>,
    array_lengths: &ArrayLengths,
) -> Result<usize, String> {
    fn expr_scratch(expr: &BytecodeExpr, signatures: &HashMap<String, NativeSignature>) -> usize {
        let mut here = 0usize;
        if let BytecodeExprKind::Call { name, args } = &expr.kind {
            if let Some(sig) = signatures.get(name) {
                let mut sum = 0usize;
                for param_ty in &sig.params {
                    if param_ty.is_aggregate() {
                        sum += param_ty.words();
                    }
                }
                here = sum;
            }
            // A nested call in argument position materializes independently.
            for arg in args {
                here = here.max(expr_scratch(arg, signatures));
            }
        } else {
            for child in expr_children(expr) {
                here = here.max(expr_scratch(child, signatures));
            }
        }
        here
    }
    // `structs`/`enums`/`array_lengths` are accepted for symmetry with the other
    // scratch sizers and to keep the call site uniform; layout comes from the
    // precomputed signatures.
    let _ = (structs, enums, array_lengths);
    let mut max = 0usize;
    for instruction in body {
        let here = match instruction {
            BytecodeInstruction::Let { value, .. }
            | BytecodeInstruction::Assign { value, .. }
            | BytecodeInstruction::Return(Some(value))
            | BytecodeInstruction::Expr(value) => expr_scratch(value, signatures),
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                let mut h = max_call_arg_scratch_words(
                    else_body,
                    structs,
                    enums,
                    signatures,
                    array_lengths,
                )?;
                for branch in branches {
                    h = h.max(expr_scratch(&branch.condition, signatures)).max(
                        max_call_arg_scratch_words(
                            &branch.body,
                            structs,
                            enums,
                            signatures,
                            array_lengths,
                        )?,
                    );
                }
                h
            }
            BytecodeInstruction::While {
                condition, body, ..
            } => expr_scratch(condition, signatures).max(max_call_arg_scratch_words(
                body,
                structs,
                enums,
                signatures,
                array_lengths,
            )?),
            BytecodeInstruction::Loop { body, .. } | BytecodeInstruction::For { body, .. } => {
                max_call_arg_scratch_words(body, structs, enums, signatures, array_lengths)?
            }
            BytecodeInstruction::Match {
                scrutinee, arms, ..
            } => {
                let mut h = expr_scratch(scrutinee, signatures);
                for arm in arms {
                    h = h.max(max_call_arg_scratch_words(
                        &arm.body,
                        structs,
                        enums,
                        signatures,
                        array_lengths,
                    )?);
                }
                h
            }
            _ => 0,
        };
        max = max.max(here);
    }
    Ok(max)
}

/// The maximum number of **outgoing stack-argument words** any single call in
/// this body needs. The first four Win64 register slots (`rcx`/`rdx`/`r8`/`r9`,
/// including a hidden aggregate-return pointer when the callee returns an
/// aggregate) are passed in registers; arguments 5, 6, … spill onto the stack
/// above the 32-byte shadow space. The caller must reserve `8 * this` bytes of
/// outgoing space (plus 32 bytes shadow) in its frame so those stack words have a
/// home at each `call`. An extern (C-ABI) call spills its 5th+ arguments into the
/// same outgoing area (see `emit_extern_call`), so it is counted here too, using
/// its raw argument count (an extern has no internal signature).
fn max_outgoing_stack_words(
    body: &[BytecodeInstruction],
    signatures: &HashMap<String, NativeSignature>,
) -> usize {
    fn call_stack_words(
        name: &str,
        args: usize,
        signatures: &HashMap<String, NativeSignature>,
    ) -> usize {
        // A compiled callee that returns an aggregate consumes one register slot
        // for its hidden result pointer, shifting the visible args down by one.
        let hidden = signatures
            .get(name)
            .map(|sig| usize::from(sig.returns_aggregate()))
            .unwrap_or(0);
        (args + hidden).saturating_sub(4)
    }
    fn expr_words(expr: &BytecodeExpr, signatures: &HashMap<String, NativeSignature>) -> usize {
        let mut here = 0usize;
        if let BytecodeExprKind::Call { name, args } = &expr.kind {
            // A compiled (internal) callee uses the stack-spill convention with a
            // possible hidden aggregate-return pointer. An extern (C-ABI) call also
            // spills its 5th+ arguments into the same outgoing area, so it must be
            // counted too; it has no native signature, so use its raw argument
            // count. (Native builtins never exceed four arguments, so this
            // over-reserves nothing in practice.)
            here = if signatures.contains_key(name.as_str()) {
                call_stack_words(name, args.len(), signatures)
            } else {
                args.len().saturating_sub(4)
            };
            for arg in args {
                here = here.max(expr_words(arg, signatures));
            }
        } else {
            for child in expr_children(expr) {
                here = here.max(expr_words(child, signatures));
            }
        }
        here
    }
    let mut max = 0usize;
    for instruction in body {
        let here = match instruction {
            BytecodeInstruction::Let { value, .. }
            | BytecodeInstruction::Assign { value, .. }
            | BytecodeInstruction::Return(Some(value))
            | BytecodeInstruction::Expr(value) => expr_words(value, signatures),
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                let mut h = max_outgoing_stack_words(else_body, signatures);
                for branch in branches {
                    h = h
                        .max(expr_words(&branch.condition, signatures))
                        .max(max_outgoing_stack_words(&branch.body, signatures));
                }
                h
            }
            BytecodeInstruction::While {
                condition, body, ..
            } => expr_words(condition, signatures).max(max_outgoing_stack_words(body, signatures)),
            BytecodeInstruction::For {
                start,
                end,
                step,
                body,
                ..
            } => expr_words(start, signatures)
                .max(expr_words(end, signatures))
                .max(
                    step.as_ref()
                        .map(|s| expr_words(s, signatures))
                        .unwrap_or(0),
                )
                .max(max_outgoing_stack_words(body, signatures)),
            BytecodeInstruction::Loop { body, .. } => max_outgoing_stack_words(body, signatures),
            BytecodeInstruction::Match {
                scrutinee, arms, ..
            } => {
                let mut h = expr_words(scrutinee, signatures);
                for arm in arms {
                    h = h.max(max_outgoing_stack_words(&arm.body, signatures));
                }
                h
            }
            _ => 0,
        };
        max = max.max(here);
    }
    max
}

/// The immediate sub-expressions of an expression (for recursive scans).
fn expr_children(expr: &BytecodeExpr) -> Vec<&BytecodeExpr> {
    match &expr.kind {
        BytecodeExprKind::Binary { left, right, .. } => vec![left, right],
        BytecodeExprKind::Unary { expr, .. } => vec![expr],
        BytecodeExprKind::Call { args, .. } => args.iter().collect(),
        BytecodeExprKind::Array(elements) => elements.iter().collect(),
        BytecodeExprKind::Field { target, .. } => vec![target],
        BytecodeExprKind::Index { target, index } => vec![target, index],
        _ => Vec::new(),
    }
}

/// Whether any instruction in a body issues a call (so the frame needs shadow
/// space). Conservatively scans nested bodies and expressions.
fn body_has_call(body: &[BytecodeInstruction]) -> bool {
    body.iter().any(instruction_has_call)
}

fn instruction_has_call(instruction: &BytecodeInstruction) -> bool {
    match instruction {
        BytecodeInstruction::Let { value, .. } => expr_has_call(value),
        BytecodeInstruction::Assign { value, .. } => expr_has_call(value),
        BytecodeInstruction::Return(Some(expr)) | BytecodeInstruction::Expr(expr) => {
            expr_has_call(expr)
        }
        BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_) => false,
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches
                .iter()
                .any(|b| expr_has_call(&b.condition) || body_has_call(&b.body))
                || body_has_call(else_body)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => expr_has_call(condition) || body_has_call(body),
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_has_call(start)
                || expr_has_call(end)
                || step.as_ref().is_some_and(expr_has_call)
                || body_has_call(body)
        }
        BytecodeInstruction::Loop { body, .. } => body_has_call(body),
        // A `match` needs shadow space if its scrutinee (often a call) or any arm
        // body issues a call.
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => expr_has_call(scrutinee) || arms.iter().any(|arm| body_has_call(&arm.body)),
        BytecodeInstruction::Throw { .. }
        | BytecodeInstruction::Try { .. }
        | BytecodeInstruction::Asm { .. } => false,
    }
}

fn expr_has_call(expr: &BytecodeExpr) -> bool {
    match &expr.kind {
        BytecodeExprKind::Call { .. } => true,
        // A string literal used as a value materializes through the
        // `__lullaby_str_lit` runtime helper (a `call`), so it needs shadow space.
        BytecodeExprKind::String(_) => true,
        // A `string + string` concatenation calls `__lullaby_str_concat`, so a
        // Binary whose result type is `string` issues a call even if neither
        // operand does. (Any other Binary just recurses into its operands.)
        BytecodeExprKind::Binary { left, right, .. } => {
            is_string_type(&expr.ty) || expr_has_call(left) || expr_has_call(right)
        }
        BytecodeExprKind::Unary { expr, .. } => expr_has_call(expr),
        _ => false,
    }
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
}

#[path = "native_object_stmt.rs"]
mod stmt_lowering;
pub(crate) use stmt_lowering::*;

#[path = "native_object_expr.rs"]
mod expr_lowering;
pub(crate) use expr_lowering::*;

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
