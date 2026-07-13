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
enum OverflowOp {
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
enum NativeType {
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
struct NativeEnumVariant {
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
enum FloatWidth {
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
struct LoweredNativeFunction {
    name: String,
    code: Vec<u8>,
    relocations: Vec<CodeRelocation>,
    /// 1-based source line of the function's declaration (from `BytecodeFunction.span`).
    /// Used only when `--debug` line info is requested; otherwise ignored.
    line: u32,
}

/// A relocation inside a function body: patch a 4-byte REL32 field at `offset`
/// (relative to the function's own code start) to reference `symbol`.
struct CodeRelocation {
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
struct StringPool {
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
struct NativeLocal {
    slot: i32,
    ty: NativeType,
}

/// Per-function native lowering state (a stack-machine codegen over `rax`).
struct NativeCtx<'a> {
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
struct NativeSignature {
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
enum ScalarPlace {
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
struct NativeLoop {
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

#[allow(clippy::too_many_arguments)]
// -- Scalar local register promotion -----------------------------------------
//
// A purely-i64-scalar function's lowering only ever touches the caller-saved
// scratch registers (rax/rcx/rdx/r8/r9); the callee-saved rbx/rsi are used only
// by the shared `.text` string/aggregate helpers, which save and restore them.
// So for such a function we can keep a couple of its hot `i64` locals in rbx/rsi
// for the whole body instead of the stack — and because they are callee-saved,
// they survive every `call` (each callee that uses them saves/restores them),
// exactly as a C compiler keeps a hot local in a register across recursion.
//
// This is deliberately conservative: any construct that could stray outside the
// scalar register set (strings, floats, aggregates, arrays, indexing, `for`/
// `match`, non-i64 params) disqualifies the whole function, which then keeps its
// existing, unchanged codegen. Correctness never depends on the analysis being
// generous — only on it never promoting a function that isn't purely scalar.

/// A callee-saved register a scalar `i64` local can be promoted into.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PReg {
    Rbx,
    Rsi,
}

impl PReg {
    /// `mov rax, <reg>`.
    fn to_rax(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x89, 0xD8],
            PReg::Rsi => &[0x48, 0x89, 0xF0],
        });
    }
    /// `mov <reg>, rax`.
    fn from_rax(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x89, 0xC3],
            PReg::Rsi => &[0x48, 0x89, 0xC6],
        });
    }
    /// `mov <reg>, <arg-register>` where arg is the Win64 integer arg index
    /// (0..3 = rcx/rdx/r8/r9). Used to seat a promoted parameter on entry.
    fn from_arg(self, code: &mut Vec<u8>, arg: usize) {
        let bytes: &[u8] = match (self, arg) {
            (PReg::Rbx, 0) => &[0x48, 0x89, 0xCB], // mov rbx, rcx
            (PReg::Rbx, 1) => &[0x48, 0x89, 0xD3], // mov rbx, rdx
            (PReg::Rbx, 2) => &[0x4C, 0x89, 0xC3], // mov rbx, r8
            (PReg::Rbx, 3) => &[0x4C, 0x89, 0xCB], // mov rbx, r9
            (PReg::Rsi, 0) => &[0x48, 0x89, 0xCE], // mov rsi, rcx
            (PReg::Rsi, 1) => &[0x48, 0x89, 0xD6], // mov rsi, rdx
            (PReg::Rsi, 2) => &[0x4C, 0x89, 0xC6], // mov rsi, r8
            (PReg::Rsi, 3) => &[0x4C, 0x89, 0xCE], // mov rsi, r9
            _ => unreachable!("promoted parameters are among the first four (register) args"),
        };
        code.extend_from_slice(bytes);
    }
    /// `mov <reg>, [rbp - slot]` (save the incoming callee-saved value into its
    /// spill slot) and its inverse `mov [rbp - slot], <reg>`.
    fn spill_to_slot(self, code: &mut Vec<u8>, slot: i32) {
        // mov [rbp + disp32], <reg>
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x89, 0x9D],
            PReg::Rsi => &[0x48, 0x89, 0xB5],
        });
        code.extend_from_slice(&(-slot).to_le_bytes());
    }
    fn restore_from_slot(self, code: &mut Vec<u8>, slot: i32) {
        // mov <reg>, [rbp + disp32]
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x8B, 0x9D],
            PReg::Rsi => &[0x48, 0x8B, 0xB5],
        });
        code.extend_from_slice(&(-slot).to_le_bytes());
    }
    /// `add/sub <reg>, imm32` and `add/sub <reg>, rax` for the memory-destination
    /// self-assign fast path when the target is promoted.
    fn add_imm(self, code: &mut Vec<u8>, imm: i32) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x81, 0xC3],
            PReg::Rsi => &[0x48, 0x81, 0xC6],
        });
        code.extend_from_slice(&imm.to_le_bytes());
    }
    fn sub_imm(self, code: &mut Vec<u8>, imm: i32) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x81, 0xEB],
            PReg::Rsi => &[0x48, 0x81, 0xEE],
        });
        code.extend_from_slice(&imm.to_le_bytes());
    }
    fn add_rax(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x01, 0xC3],
            PReg::Rsi => &[0x48, 0x01, 0xC6],
        });
    }
    fn sub_rax(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x29, 0xC3],
            PReg::Rsi => &[0x48, 0x29, 0xC6],
        });
    }
    /// `add <self>, <src>` / `sub <self>, <src>` (both promoted registers), for
    /// `acc = acc + i` where the right operand is itself a promoted-register local
    /// — skips the `mov rax, <src>` round-trip through the scratch register.
    /// `add/sub r/m64, r64` is REX.W 01/29 /r; ModRM = 11 <src> <self>.
    fn add_reg(self, code: &mut Vec<u8>, src: PReg) {
        code.extend_from_slice(&[0x48, 0x01, modrm_reg_reg(src, self)]);
    }
    fn sub_reg(self, code: &mut Vec<u8>, src: PReg) {
        code.extend_from_slice(&[0x48, 0x29, modrm_reg_reg(src, self)]);
    }
    /// `cmp <self>, imm32` (sign-extended) — a fused-comparison left operand that
    /// is a promoted register compares directly instead of `mov rax, <reg>` first.
    fn cmp_imm(self, code: &mut Vec<u8>, imm: i32) {
        // cmp r/m64, imm32 -> REX.W 81 /7 id ; ModRM = 11 111 <reg>.
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x81, 0xFB],
            PReg::Rsi => &[0x48, 0x81, 0xFE],
        });
        code.extend_from_slice(&imm.to_le_bytes());
    }
    /// The register's 3-bit encoding in a ModRM field.
    fn code3(self) -> u8 {
        match self {
            PReg::Rbx => 3,
            PReg::Rsi => 6,
        }
    }
}

/// ModRM byte for a register-direct `op r/m64, r64`: mod=11, reg=source (the
/// `/r` field), rm=destination.
fn modrm_reg_reg(src: PReg, dst: PReg) -> u8 {
    0xC0 | (src.code3() << 3) | dst.code3()
}

/// If `expr` is a bare local variable currently promoted into a callee-saved
/// register, return that register so a consumer can read it directly instead of
/// materializing it into `rax` first. Any non-variable, unresolvable, or
/// stack-resident local returns `None` (the unchanged `mov rax, …` path).
fn promoted_var_reg(ctx: &NativeCtx, expr: &BytecodeExpr) -> Option<PReg> {
    if let BytecodeExprKind::Variable(name) = &expr.kind {
        let slot = ctx.local_slot(name).ok()?;
        return ctx.promoted_reg(slot);
    }
    None
}

/// Whether an expression lowers entirely within the scalar register set (never
/// touching rbx/rsi). Conservative: only plain `i64` integer arithmetic/
/// comparison over `i64` operands, `i64` variables/literals, and all-`i64` calls.
fn expr_reg_promotable(expr: &BytecodeExpr) -> bool {
    match &expr.kind {
        BytecodeExprKind::Integer(_) | BytecodeExprKind::Bool(_) | BytecodeExprKind::Char(_) => {
            true
        }
        BytecodeExprKind::Variable(_) => expr.ty.name == "i64",
        BytecodeExprKind::Unary { expr: inner, .. } => {
            inner.ty.name == "i64" && expr_reg_promotable(inner)
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            left.ty.name == "i64"
                && right.ty.name == "i64"
                && expr_reg_promotable(left)
                && expr_reg_promotable(right)
        }
        BytecodeExprKind::Call { args, .. } => {
            expr.ty.name == "i64"
                && args
                    .iter()
                    .all(|a| a.ty.name == "i64" && expr_reg_promotable(a))
        }
        _ => false,
    }
}

/// Whether an instruction lowers entirely within the scalar register set.
fn instr_reg_promotable(instr: &BytecodeInstruction) -> bool {
    match instr {
        BytecodeInstruction::Let { ty, value, .. } => {
            ty.name == "i64" && expr_reg_promotable(value)
        }
        // A path-less assignment to a scalar local (no field/index hop).
        BytecodeInstruction::Assign { path, value, .. } => {
            path.is_empty() && expr_reg_promotable(value)
        }
        BytecodeInstruction::Return(Some(e)) => expr_reg_promotable(e),
        BytecodeInstruction::Return(None) => true,
        BytecodeInstruction::Expr(e) => expr_reg_promotable(e),
        BytecodeInstruction::Break(_) | BytecodeInstruction::Continue(_) => true,
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().all(|b| {
                expr_reg_promotable(&b.condition) && b.body.iter().all(instr_reg_promotable)
            }) && else_body.iter().all(instr_reg_promotable)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => expr_reg_promotable(condition) && body.iter().all(instr_reg_promotable),
        // A range `for` is promotable when its bounds/step and body are scalar.
        // The counter and its hidden `__end`/`__step` slots stay on the stack
        // (see `for_counter_slots`), because `lower_native_for` accesses them
        // directly; the body's other scalar locals (accumulators) get registers.
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_reg_promotable(start)
                && expr_reg_promotable(end)
                && step.as_ref().is_none_or(expr_reg_promotable)
                && body.iter().all(instr_reg_promotable)
        }
        // Loop / Match / Asm / Throw / Try are conservatively excluded.
        _ => false,
    }
}

/// Collect the stack slots that a range `for` needs to keep off registers: each
/// loop's hidden `{name}__end` / `{name}__step` bound/step slots, which
/// `lower_native_for` reads as stack memory operands. The counter itself may be
/// promoted — `lower_native_for` honors `promoted_reg` for it.
fn for_counter_slots(
    instrs: &[BytecodeInstruction],
    locals: &HashMap<String, NativeLocal>,
    out: &mut std::collections::HashSet<i32>,
) {
    for instr in instrs {
        match instr {
            BytecodeInstruction::For { name, body, .. } => {
                for key in [format!("{name}__end"), format!("{name}__step")] {
                    if let Some(local) = locals.get(&key) {
                        out.insert(local.slot);
                    }
                }
                for_counter_slots(body, locals, out);
            }
            BytecodeInstruction::While { body, .. } | BytecodeInstruction::Loop { body, .. } => {
                for_counter_slots(body, locals, out)
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    for_counter_slots(&branch.body, locals, out);
                }
                for_counter_slots(else_body, locals, out);
            }
            BytecodeInstruction::Match { arms, .. } => {
                for arm in arms {
                    for_counter_slots(&arm.body, locals, out);
                }
            }
            _ => {}
        }
    }
}

/// Decide which of a purely-scalar function's `i64` locals to keep in callee-saved
/// registers. Returns (local-slot -> register) and the ordered registers to
/// preserve. Empty (no promotion) unless the whole function is scalar-only with
/// `i64` params and an `i64` return.
fn plan_register_promotion(
    function: &BytecodeFunction,
    locals: &HashMap<String, NativeLocal>,
) -> (HashMap<i32, PReg>, Vec<PReg>) {
    let none = (HashMap::new(), Vec::new());
    if function.return_type.name != "i64" {
        return none;
    }
    if !function.params.iter().all(|p| p.ty.name == "i64") {
        return none;
    }
    if !function.instructions.iter().all(instr_reg_promotable) {
        return none;
    }
    // A range `for`'s counter and its hidden bound/step slots must stay on the
    // stack (`lower_native_for` addresses them as memory), so exclude them.
    let mut excluded = std::collections::HashSet::new();
    for_counter_slots(&function.instructions, locals, &mut excluded);
    // Every remaining local is a single-word `i64`. Keep the first couple (lowest
    // slots — parameters precede body locals) in registers; the rest stay on the
    // stack.
    let mut slots: Vec<i32> = locals
        .values()
        .filter(|l| matches!(l.ty, NativeType::I64))
        .map(|l| l.slot)
        .filter(|slot| !excluded.contains(slot))
        .collect();
    slots.sort_unstable();
    let regs = [PReg::Rbx, PReg::Rsi];
    let mut promoted = HashMap::new();
    let mut saved = Vec::new();
    for (slot, reg) in slots.into_iter().zip(regs) {
        promoted.insert(slot, reg);
        saved.push(reg);
    }
    (promoted, saved)
}

fn lower_native_function(
    function: &BytecodeFunction,
    callable: &std::collections::HashSet<&str>,
    extern_sigs: &HashMap<&str, &crate::IrExternSignature>,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    strings: &mut StringPool,
    signatures: &HashMap<String, NativeSignature>,
    array_lengths: &ArrayLengths,
    fast_math: bool,
) -> Result<LoweredNativeFunction, String> {
    let mut ctx = NativeCtx::plan(
        function,
        callable,
        extern_sigs,
        structs,
        enums,
        strings,
        signatures,
        array_lengths,
    )?;
    ctx.fast_math = fast_math;
    let mut code = Vec::new();

    // Prologue: push rbp; mov rbp, rsp; sub rsp, frame_size.
    code.extend_from_slice(&[0x55, 0x48, 0x89, 0xE5]);
    emit_sub_rsp(&mut code, ctx.frame_size);

    // Preserve the callee-saved registers used to hold promoted locals, spilling
    // each caller value into its reserved frame slot (restored in the epilogue).
    // Done before parameters are seated so a promoted parameter can overwrite its
    // register next.
    for (reg, slot) in &ctx.saved_reg_slots {
        reg.spill_to_slot(&mut code, *slot);
    }

    // Register argument order: `mov [rbp - slot], reg`. When the function returns
    // an aggregate, the hidden result pointer consumes the first register (rcx),
    // shifting the visible parameters down by one.
    const PARAM_STORE: [&[u8]; 4] = [
        &[0x48, 0x89, 0x8D], // mov [rbp+disp32], rcx
        &[0x48, 0x89, 0x95], // mov [rbp+disp32], rdx
        &[0x4C, 0x89, 0x85], // mov [rbp+disp32], r8
        &[0x4C, 0x89, 0x8D], // mov [rbp+disp32], r9
    ];
    // Load an integer argument register (by index) into rax: `mov rax, reg`.
    const ARG_TO_RAX: [&[u8]; 4] = [
        &[0x48, 0x89, 0xC8], // mov rax, rcx
        &[0x48, 0x89, 0xD0], // mov rax, rdx
        &[0x4C, 0x89, 0xC0], // mov rax, r8
        &[0x4C, 0x89, 0xC8], // mov rax, r9
    ];

    // The hidden return pointer (if any) is register 0; parameters follow.
    let mut reg = 0usize;
    if let Some(sret_slot) = ctx.sret_slot {
        // Spill the caller-provided result pointer into its frame slot.
        code.extend_from_slice(PARAM_STORE[reg]);
        code.extend_from_slice(&(-sret_slot).to_le_bytes());
        reg += 1;
    }
    for param in &function.params {
        let local = ctx.local(&param.name)?.clone();
        // Arguments 5, 6, … (register slots 4, 5, … already consumed) arrive on the
        // stack above the caller's 32-byte shadow space. On entry the callee sees
        // the return address at `[rsp]`; after `push rbp` + `mov rbp, rsp` the saved
        // rbp is at `[rbp]`, the return address at `[rbp+8]`, the caller's shadow at
        // `[rbp+16 .. rbp+48]`, and the first stack argument at `[rbp+48]`. So the
        // Nth stack argument (0-indexed `reg-4`) sits at `[rbp + 48 + 8*(reg-4)]`.
        // The first four (`reg < 4`) arrive in registers.
        let on_stack = reg >= 4;
        let stack_disp = 48 + (reg as i32 - 4) * 8;
        match local.ty {
            NativeType::F64 | NativeType::F32 => {
                if on_stack {
                    // A float stack argument is already a raw 8-byte word; copy it
                    // bit-for-bit into the parameter's slot (the slot holds the raw
                    // float bits, so no XMM round-trip is needed).
                    emit_mov_rax_from_rbp_pos(&mut code, stack_disp);
                    store_local(&mut code, local.slot);
                } else {
                    // A float register parameter arrives in the SSE register at this
                    // position (`xmm N`, positionally aligned with the integer
                    // registers). Spill it into the parameter's slot.
                    let width = match local.ty {
                        NativeType::F64 => FloatWidth::F64,
                        NativeType::F32 => FloatWidth::F32,
                        _ => unreachable!("guarded by the match arm"),
                    };
                    emit_store_xmm_to_slot(&mut code, reg as u8, local.slot, width);
                }
            }
            NativeType::Struct { .. } | NativeType::Array { .. } | NativeType::Enum { .. } => {
                // The argument holds a pointer to the caller's copy (in a register
                // for `reg < 4`, on the stack otherwise). Copy the aggregate words
                // into the parameter's frame slots (value semantics: the callee owns
                // an independent snapshot and never mutates the caller's copy). rax =
                // source pointer (addresses word 0, the aggregate's highest stack
                // address). Words descend in memory, so word k is at `[rax - 8*k]`,
                // matching the caller's `[rbp - (base + 8*k)]` layout.
                if on_stack {
                    emit_mov_rax_from_rbp_pos(&mut code, stack_disp);
                } else {
                    code.extend_from_slice(ARG_TO_RAX[reg]);
                }
                for word in 0..local.ty.words() as i32 {
                    // mov rcx, [rax - 8*word]
                    emit_mov_rcx_from_rax_disp(&mut code, -word * 8);
                    // mov [rbp - (slot + 8*word)], rcx
                    emit_mov_slot_from_rcx(&mut code, local.slot + word * 8);
                }
            }
            NativeType::I64
            | NativeType::String
            | NativeType::List { .. }
            | NativeType::Map { .. }
            | NativeType::HeapStruct { .. } => {
                // An integer/pointer scalar parameter — or a string/list/map/heap
                // struct (a heap pointer word) — spills its register (or its incoming
                // stack word)
                // directly into its slot. A string parameter shares the caller's
                // record by pointer, which is safe because strings are immutable. A
                // list/map parameter also shares by pointer safely: their mutators
                // (`push`/`set`/`pop`, `map_set`) deep-copy their source, so the
                // callee cannot alter the caller's value through the shared pointer.
                // A promoted parameter is seated in its callee-saved register
                // instead of a stack slot (promotion only picks i64 params, which
                // are among the first register args, so `from_arg` always applies;
                // the on-stack arm is defensive).
                match (on_stack, ctx.promoted_reg(local.slot)) {
                    (false, Some(preg)) => preg.from_arg(&mut code, reg),
                    (true, Some(preg)) => {
                        emit_mov_rax_from_rbp_pos(&mut code, stack_disp);
                        preg.from_rax(&mut code);
                    }
                    (false, None) => {
                        code.extend_from_slice(PARAM_STORE[reg]);
                        code.extend_from_slice(&(-local.slot).to_le_bytes());
                    }
                    (true, None) => {
                        emit_mov_rax_from_rbp_pos(&mut code, stack_disp);
                        store_local(&mut code, local.slot);
                    }
                }
            }
        }
        reg += 1;
    }

    let mut loops: Vec<NativeLoop> = Vec::new();

    // A function whose last statement is a value-producing tail expression (e.g.
    // a body of just `a + b`) returns that value. Lower the leading statements
    // normally, then lower the tail expression and emit the return epilogue so
    // the result in rax is returned rather than being clobbered by the
    // fallthrough safety epilogue below.
    let instructions = &function.instructions;
    let tail_is_value_expr = matches!(
        instructions.last(),
        Some(BytecodeInstruction::Expr(expr)) if !expr.ty.is_void()
    );
    // A function whose last statement is an `asm` block is trusted to leave the
    // return value in `rax`. Lower the head, emit the asm bytes, then emit the
    // normal epilogue so `rax` is returned intact rather than being clobbered by
    // the fallthrough `xor eax,eax` below. (The programmer must not emit their
    // own `ret` — the epilogue restores `rbp` and `rsp` and returns.)
    let tail_is_asm = matches!(instructions.last(), Some(BytecodeInstruction::Asm { .. }));
    // A function whose last statement is a `match` producing the function's value:
    // each arm leaves its result in `rax`; after the whole match, the epilogue
    // returns it. (An arm that itself ends in an explicit `return` emits its own
    // epilogue and never reaches the shared match end.)
    let tail_is_value_match = !function.return_type.is_void()
        && matches!(instructions.last(), Some(BytecodeInstruction::Match { .. }));
    // A function whose last statement is an `if`/`elif`/`else` producing the
    // function's value (e.g. a body ending in `if c\n a\n else\n b`): each branch
    // leaves its value in `rax` and converges after the chain, where the epilogue
    // returns it. Without this the tail `if` lowers as a plain statement and the
    // fallthrough `xor rax,rax` below overwrites the branch result (returning 0).
    // Restricted to a scalar-register return (not float/aggregate — those tail
    // `if`s stay deferred): a value-producing `if` is exhaustive (has an `else`),
    // so control always reaches the epilogue with `rax` set.
    let tail_is_value_if = !function.return_type.is_void()
        && ctx.sret_slot.is_none()
        && !matches!(ctx.return_ty, NativeType::F64 | NativeType::F32)
        && matches!(instructions.last(), Some(BytecodeInstruction::If { .. }));
    if tail_is_asm {
        let (head, tail) = instructions.split_at(instructions.len() - 1);
        lower_native_stmts(&mut ctx, head, &mut code, &mut loops)?;
        if let BytecodeInstruction::Asm { bytes, .. } = &tail[0] {
            code.extend_from_slice(bytes);
        }
        emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);
    } else if tail_is_value_expr {
        let (head, tail) = instructions.split_at(instructions.len() - 1);
        lower_native_stmts(&mut ctx, head, &mut code, &mut loops)?;
        if let BytecodeInstruction::Expr(expr) = &tail[0] {
            // An aggregate-valued tail expression is the function's by-pointer
            // result: materialize it through the hidden return pointer. A float
            // tail expression leaves its value in `xmm0` (the Win64 SSE return
            // register). A scalar tail expression leaves its value in rax.
            if ctx.sret_slot.is_some() {
                lower_aggregate_return(&mut ctx, expr, &mut code)?;
            } else if matches!(ctx.return_ty, NativeType::F64 | NativeType::F32) {
                lower_native_float_expr(&mut ctx, expr, &mut code)?;
            } else {
                lower_native_expr(&mut ctx, expr, &mut code)?;
            }
        }
        emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);
    } else if tail_is_value_match {
        let (head, tail) = instructions.split_at(instructions.len() - 1);
        lower_native_stmts(&mut ctx, head, &mut code, &mut loops)?;
        if let BytecodeInstruction::Match {
            scrutinee, arms, ..
        } = &tail[0]
        {
            lower_native_match(&mut ctx, scrutinee, arms, true, &mut code, &mut loops)?;
        }
        emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);
    } else if tail_is_value_if {
        // The tail `if` lowers as a statement; each branch leaves the function's
        // value in rax and jumps to the convergence point right before this
        // epilogue, which returns it. Emitting the epilogue here makes the
        // fallthrough `xor rax,rax` below unreachable (dead safety code).
        lower_native_stmts(&mut ctx, instructions, &mut code, &mut loops)?;
        emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);
    } else {
        lower_native_stmts(&mut ctx, instructions, &mut code, &mut loops)?;
    }

    // Fallthrough epilogue: functions in this subset are non-void and expected to
    // return on every path, but emit a safe `xor eax,eax` + epilogue so a missing
    // tail return cannot run off the end of the section.
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax
    emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);

    Ok(LoweredNativeFunction {
        name: function.name.clone(),
        code,
        relocations: ctx.relocations,
        line: function.span.line as u32,
    })
}

/// Emit `sub rsp, imm` (imm >= 0). Uses imm8 form when it fits, else imm32.
fn emit_sub_rsp(code: &mut Vec<u8>, amount: i32) {
    if amount == 0 {
        return;
    }
    if (0..=127).contains(&amount) {
        code.extend_from_slice(&[0x48, 0x83, 0xEC, amount as u8]);
    } else {
        code.extend_from_slice(&[0x48, 0x81, 0xEC]);
        code.extend_from_slice(&amount.to_le_bytes());
    }
}

/// Emit the function epilogue: restore any promoted callee-saved registers from
/// their spill slots (rbp-relative, still valid), then `add rsp, imm; pop rbp;
/// ret`. `saved_reg_slots` is empty for functions without register promotion.
fn emit_native_epilogue(code: &mut Vec<u8>, frame_size: i32, saved_reg_slots: &[(PReg, i32)]) {
    for (reg, slot) in saved_reg_slots {
        reg.restore_from_slot(code, *slot);
    }
    if frame_size != 0 {
        if (0..=127).contains(&frame_size) {
            code.extend_from_slice(&[0x48, 0x83, 0xC4, frame_size as u8]);
        } else {
            code.extend_from_slice(&[0x48, 0x81, 0xC4]);
            code.extend_from_slice(&frame_size.to_le_bytes());
        }
    }
    code.extend_from_slice(&[0x5D, 0xC3]); // pop rbp; ret
}

fn lower_native_stmts(
    ctx: &mut NativeCtx,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    for stmt in body {
        lower_native_stmt(ctx, stmt, code, loops)?;
    }
    Ok(())
}

fn lower_native_stmt(
    ctx: &mut NativeCtx,
    stmt: &BytecodeInstruction,
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    match stmt {
        BytecodeInstruction::Let { name, value, .. } => {
            // A scalar `let` uses the register path; a float `let` evaluates into
            // xmm0 and stores the whole word; an aggregate `let` materializes each
            // flattened scalar word directly into its slots.
            match ctx.local(name)?.ty {
                // An `i64` scalar or a string/list/map/heap-struct (a pointer word)
                // uses the register path: evaluate into `rax` and store the whole
                // word. (A `HeapStruct` never appears as a top-level local; kept in
                // this arm for match exhaustiveness.)
                NativeType::I64
                | NativeType::String
                | NativeType::List { .. }
                | NativeType::Map { .. }
                | NativeType::HeapStruct { .. } => {
                    lower_native_expr(ctx, value, code)?;
                    let slot = ctx.local_slot(name)?;
                    match ctx.promoted_reg(slot) {
                        Some(reg) => reg.from_rax(code), // mov <reg>, rax
                        None => store_local(code, slot), // mov [rbp - slot], rax
                    }
                }
                NativeType::F64 | NativeType::F32 => {
                    let slot = ctx.local_slot(name)?;
                    let width = lower_native_float_expr(ctx, value, code)?;
                    store_float_local(code, slot, width); // movs[sd] [rbp - slot], xmm0
                }
                NativeType::Struct { .. } | NativeType::Array { .. } | NativeType::Enum { .. } => {
                    let base = ctx.local(name)?.slot;
                    let ty = ctx.local(name)?.ty.clone();
                    lower_aggregate_init(ctx, base, &ty, value, code)?;
                }
            }
            Ok(())
        }
        BytecodeInstruction::Assign {
            name,
            path,
            op,
            value,
            ..
        } => {
            // A float local assigned through no path (a plain `f = ...`) uses the
            // XMM path. A float target reached through a struct/array path is out
            // of scope (float aggregate members are rejected at layout time), so
            // only the pathless case can be a float here.
            if path.is_empty()
                && let NativeType::F64 | NativeType::F32 = ctx.local(name)?.ty
            {
                let slot = ctx.local_slot(name)?;
                let store_width = match ctx.local(name)?.ty {
                    NativeType::F64 => FloatWidth::F64,
                    NativeType::F32 => FloatWidth::F32,
                    _ => unreachable!("guarded above"),
                };
                match op {
                    AssignOp::Replace => {
                        let width = lower_native_float_expr(ctx, value, code)?;
                        debug_assert_eq!(width, store_width, "float assign width mismatch");
                        store_float_local(code, slot, store_width);
                    }
                    AssignOp::Add | AssignOp::Subtract | AssignOp::Multiply | AssignOp::Divide => {
                        // `f op= rhs`: load current into xmm0, rhs into xmm1, apply
                        // the scalar op, re-store. `op` maps to the same arithmetic
                        // as the binary form.
                        let bin = match op {
                            AssignOp::Add => BinaryOp::Add,
                            AssignOp::Subtract => BinaryOp::Subtract,
                            AssignOp::Multiply => BinaryOp::Multiply,
                            AssignOp::Divide => BinaryOp::Divide,
                            AssignOp::Replace | AssignOp::Remainder => unreachable!(),
                        };
                        // Compute the RHS into xmm0, spill it, load current into
                        // xmm0, restore RHS into xmm1, then apply left <op> right.
                        let rhs_width = lower_native_float_expr(ctx, value, code)?;
                        debug_assert_eq!(rhs_width, store_width, "float assign width mismatch");
                        push_xmm0(code); // save RHS
                        load_float_local(code, slot, store_width); // xmm0 = current (left)
                        pop_xmm1(code); // xmm1 = RHS (right)
                        emit_float_arith(code, bin, store_width);
                        store_float_local(code, slot, store_width);
                    }
                    AssignOp::Remainder => {
                        unreachable!("`%=` requires integer operands (rejected by semantics)")
                    }
                }
                return Ok(());
            }
            // A path-less whole-value assignment to an enum (or other aggregate)
            // local re-materializes the value into the local's words. Only
            // `Replace` is meaningful for an aggregate (there is no `+=` on a
            // struct/array/enum), so a compound op on one is rejected as a skip.
            if path.is_empty()
                && matches!(
                    ctx.local(name)?.ty,
                    NativeType::Struct { .. } | NativeType::Array { .. } | NativeType::Enum { .. }
                )
            {
                if !matches!(op, AssignOp::Replace) {
                    return Err("compound assignment on an aggregate is not supported".to_string());
                }
                let base = ctx.local(name)?.slot;
                let ty = ctx.local(name)?.ty.clone();
                return lower_aggregate_init(ctx, base, &ty, value, code);
            }
            // A path-less whole-value assignment to a string, list, or map local
            // (`s = a + b`, `l = push(l, x)`, `l = list_new()`,
            // `m = map_set(m, k, v)`, `m = map_new()`, …) re-stores the pointer word
            // through the register path. Only `Replace` is meaningful for such a
            // pointer value; a compound op is a skip. (String `+` is concatenation,
            // which yields a fresh record — a whole-value `Replace`, never a `+=`.)
            if path.is_empty()
                && matches!(
                    ctx.local(name)?.ty,
                    NativeType::String | NativeType::List { .. } | NativeType::Map { .. }
                )
            {
                if !matches!(op, AssignOp::Replace) {
                    return Err(
                        "compound assignment on a string, list, or map is not supported"
                            .to_string(),
                    );
                }
                lower_native_expr(ctx, value, code)?;
                let slot = ctx.local_slot(name)?;
                store_local(code, slot);
                return Ok(());
            }
            // A float array element / float struct field store (`a[i] = <f64>`):
            // resolve permitting a float element and store through xmm0. Only a
            // plain `Replace` is supported (a float compound `a[i] += ...` is
            // deferred, mirroring the string/list rejection above).
            let (typed_place, elem_ty) = resolve_scalar_place_typed(ctx, name, path)?;
            if matches!(elem_ty, NativeType::F64 | NativeType::F32) {
                if !matches!(op, AssignOp::Replace) {
                    return Err(
                        "compound assignment on a float array element is not supported".to_string(),
                    );
                }
                let width = match elem_ty {
                    NativeType::F32 => FloatWidth::F32,
                    _ => FloatWidth::F64,
                };
                match typed_place {
                    ScalarPlace::Const { slot } => {
                        lower_native_float_expr(ctx, value, code)?; // xmm0 = value
                        store_float_local(code, slot, width);
                    }
                    ScalarPlace::Dynamic { .. } => {
                        lower_native_float_expr(ctx, value, code)?; // xmm0 = value
                        push_xmm0(code); // spill (address calc clobbers gprs)
                        emit_dynamic_addr_into_rcx(ctx, &typed_place, code)?; // rcx = &elem
                        pop_xmm0(code); // xmm0 = value
                        store_float_from_rcx(code, width); // movsd [rcx], xmm0
                    }
                }
                return Ok(());
            }
            let place = resolve_scalar_place(ctx, name, path)?;
            match op {
                AssignOp::Replace => {
                    // Evaluate the RHS, then store into the resolved scalar slot.
                    match place {
                        ScalarPlace::Const { slot } => {
                            // `x = x + rhs` / `x = x - rhs`, where the assigned
                            // local is also the left operand, folds the update into
                            // the destination: a memory-destination `add`/`sub
                            // [rbp-slot], …`, or `add`/`sub <reg>, …` when the target
                            // is a promoted register — skipping the load of the
                            // target and the store back (the dominant per-iteration
                            // cost in a counting loop). Plain i64 only (fixed-width
                            // kinds need width re-normalization; floats/aggregates
                            // handled above), and only when the left operand
                            // resolves to this exact slot. `add`/`sub` keep the low
                            // 64 bits, matching the interpreters' wrapping add/sub.
                            if let BytecodeExprKind::Binary {
                                left,
                                op: bop,
                                right,
                            } = &value.kind
                                && matches!(bop, BinaryOp::Add | BinaryOp::Subtract)
                                && left.ty.name == "i64"
                                && right.ty.name == "i64"
                                && let BytecodeExprKind::Variable(lname) = &left.kind
                                && ctx.local_slot(lname).ok() == Some(slot)
                            {
                                let is_add = matches!(bop, BinaryOp::Add);
                                let imm = match &right.kind {
                                    BytecodeExprKind::Integer(rhs) => i32::try_from(*rhs).ok(),
                                    _ => None,
                                };
                                match ctx.promoted_reg(slot) {
                                    Some(reg) => match imm {
                                        Some(imm) if is_add => reg.add_imm(code, imm),
                                        Some(imm) => reg.sub_imm(code, imm),
                                        None => {
                                            // `acc = acc + rhs`: if `rhs` is itself a
                                            // promoted-register local, add/sub the two
                                            // registers directly, skipping the `mov rax,
                                            // <rhs>` round-trip.
                                            match promoted_var_reg(ctx, right) {
                                                Some(src) if is_add => reg.add_reg(code, src),
                                                Some(src) => reg.sub_reg(code, src),
                                                None => {
                                                    lower_native_expr(ctx, right, code)?; // rhs → rax
                                                    if is_add {
                                                        reg.add_rax(code)
                                                    } else {
                                                        reg.sub_rax(code)
                                                    }
                                                }
                                            }
                                        }
                                    },
                                    None => {
                                        let disp = (-slot).to_le_bytes();
                                        match imm {
                                            Some(imm) => {
                                                // add/sub qword ptr [rbp-slot], imm32
                                                let modrm = if is_add { 0x85 } else { 0xAD };
                                                code.extend_from_slice(&[0x48, 0x81, modrm]);
                                                code.extend_from_slice(&disp);
                                                code.extend_from_slice(&imm.to_le_bytes());
                                            }
                                            None => {
                                                lower_native_expr(ctx, right, code)?; // rhs → rax
                                                // add/sub qword ptr [rbp-slot], rax
                                                let opcode = if is_add { 0x01 } else { 0x29 };
                                                code.extend_from_slice(&[0x48, opcode, 0x85]);
                                                code.extend_from_slice(&disp);
                                            }
                                        }
                                    }
                                }
                                return Ok(());
                            }
                            lower_native_expr(ctx, value, code)?;
                            match ctx.promoted_reg(slot) {
                                Some(reg) => reg.from_rax(code),
                                None => store_local(code, slot),
                            }
                        }
                        ScalarPlace::Dynamic { .. } => {
                            lower_native_expr(ctx, value, code)?;
                            code.push(0x50); // push rax (value)
                            emit_dynamic_addr_into_rcx(ctx, &place, code)?; // rcx = &slot
                            code.push(0x58); // pop rax (value)
                            code.extend_from_slice(&[0x48, 0x89, 0x01]); // mov [rcx], rax
                        }
                    }
                }
                other => {
                    let bin = match other {
                        AssignOp::Add => BinaryOp::Add,
                        AssignOp::Subtract => BinaryOp::Subtract,
                        AssignOp::Multiply => BinaryOp::Multiply,
                        AssignOp::Divide => BinaryOp::Divide,
                        AssignOp::Remainder => BinaryOp::Remainder,
                        AssignOp::Replace => unreachable!(),
                    };
                    match place {
                        ScalarPlace::Const { slot } => {
                            match ctx.promoted_reg(slot) {
                                Some(reg) => reg.to_rax(code), // rax = current
                                None => load_local(code, slot),
                            }
                            code.push(0x50); // push rax (left)
                            lower_native_expr(ctx, value, code)?; // rax = right
                            emit_i64_binop_from_stack(code, bin)?;
                            match ctx.promoted_reg(slot) {
                                Some(reg) => reg.from_rax(code),
                                None => store_local(code, slot),
                            }
                        }
                        ScalarPlace::Dynamic { .. } => {
                            // Compute &slot into rcx and keep it across the op.
                            emit_dynamic_addr_into_rcx(ctx, &place, code)?;
                            code.push(0x51); // push rcx (address)
                            code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx] (left)
                            code.push(0x50); // push rax (left)
                            lower_native_expr(ctx, value, code)?; // rax = right
                            emit_i64_binop_from_stack(code, bin)?; // rax = left <op> right
                            code.push(0x59); // pop rcx (address)
                            code.extend_from_slice(&[0x48, 0x89, 0x01]); // mov [rcx], rax
                        }
                    }
                }
            }
            Ok(())
        }
        BytecodeInstruction::Return(Some(expr)) => {
            if ctx.sret_slot.is_some() {
                lower_aggregate_return(ctx, expr, code)?;
            } else if matches!(ctx.return_ty, NativeType::F64 | NativeType::F32) {
                // A float return leaves its value in `xmm0` (the Win64 SSE return
                // register).
                lower_native_float_expr(ctx, expr, code)?;
            } else {
                lower_native_expr(ctx, expr, code)?;
            }
            emit_native_epilogue(code, ctx.frame_size, &ctx.saved_reg_slots);
            Ok(())
        }
        BytecodeInstruction::Return(None) => {
            Err("native subset functions must return an i64 value".to_string())
        }
        BytecodeInstruction::Expr(expr) => {
            // A tail expression is the function result; a non-tail call result is
            // discarded. Either way, evaluate it (leaving the value in rax).
            lower_native_expr(ctx, expr, code)?;
            Ok(())
        }
        BytecodeInstruction::Break(_) => {
            let loop_ctx = loops.last_mut().ok_or("`break` outside a loop")?;
            // jmp rel32 (target patched at loop end).
            code.push(0xE9);
            let site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            loop_ctx.break_sites.push(site);
            Ok(())
        }
        BytecodeInstruction::Continue(_) => {
            let loop_ctx = loops.last_mut().ok_or("`continue` outside a loop")?;
            match loop_ctx.continue_target {
                Some(target) => emit_jmp_to(code, target),
                None => {
                    // Forward jump to the (not-yet-emitted) step block.
                    code.push(0xE9);
                    let site = code.len();
                    code.extend_from_slice(&[0, 0, 0, 0]);
                    loop_ctx.continue_sites.push(site);
                }
            }
            Ok(())
        }
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => lower_native_if(ctx, branches, else_body, code, loops),
        BytecodeInstruction::While {
            condition, body, ..
        } => lower_native_while(ctx, condition, body, code, loops),
        BytecodeInstruction::Loop { body, .. } => lower_native_loop(ctx, body, code, loops),
        BytecodeInstruction::For {
            name,
            start,
            end,
            step,
            body,
            ..
        } => lower_native_for(ctx, name, start, end, step.as_ref(), body, code, loops),
        // Inline assembly: emit the raw x86-64 machine-code bytes verbatim into
        // the current function's `.text` at this point. The programmer is trusted
        // (this is `unsafe`); the bytes are not decoded, relocated, or validated
        // beyond the 0..=255 range check already done in semantics. A block that
        // leaves a value in `rax` (e.g. `mov rax, imm32`) returns it, since the
        // Win64 epilogue returns `rax`.
        BytecodeInstruction::Asm { bytes, .. } => {
            code.extend_from_slice(bytes);
            Ok(())
        }
        BytecodeInstruction::Throw { .. } | BytecodeInstruction::Try { .. } => {
            Err("throw/try is not in the native subset".to_string())
        }
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => lower_native_match(ctx, scrutinee, arms, false, code, loops),
    }
}

// -- Stack aggregate lowering (init, place resolution, addressing) -----------

/// Materialize an aggregate value into the contiguous stack words beginning at
/// `base_slot`. The supported initializer shapes mirror how the IR lowerer
/// represents construction:
///   * an array literal `[e0, e1, ...]` -> each element materialized in turn;
///   * a struct constructor `Call { name: StructName, args }` -> each field in
///     declared order (the IR already reorders named fields);
///   * an enum constructor `Call { name: variant, args }`;
///   * a call to an aggregate-returning function -> the callee writes the result
///     through a hidden pointer; the returned pointer is copied word-by-word;
///   * an aggregate variable `x` -> a word-by-word copy of another local.
fn lower_aggregate_init(
    ctx: &mut NativeCtx,
    base_slot: i32,
    ty: &NativeType,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // `map_get(m, k) -> option<V>` is a builtin producing an aggregate: materialize
    // the `some(v)`/`none` option directly into `base_slot`. It is matched before
    // the compiled-function case because `map_get` is a builtin, not a `signatures`
    // entry.
    if let BytecodeExprKind::Call { name, args } = &value.kind
        && name == MAP_GET_BUILTIN
        && args.len() == 2
        && supported_map_kv(&args[0].ty).is_some()
    {
        return lower_map_get_into(ctx, base_slot, &value.ty, &args[0], &args[1], code);
    }
    // `checked_<op>(a, b) -> option<T>` is a builtin producing an aggregate:
    // materialize the `some(result)`/`none` option directly into `base_slot`.
    if let BytecodeExprKind::Call { name, args } = &value.kind
        && let Some((ovf_op, OverflowMode::Checked)) = overflow_builtin(name)
        && args.len() == 2
        && let Some(kind) = fixed_int_kind(args[0].ty.name.as_str())
    {
        return lower_native_checked_into(
            ctx, base_slot, &value.ty, ovf_op, kind, &args[0], &args[1], code,
        );
    }
    // `parse_i64(s) -> result<i64, string>` is a builtin producing an aggregate:
    // materialize the `ok(n)`/`err(message)` result directly into `base_slot`.
    if let BytecodeExprKind::Call { name, args } = &value.kind
        && name == "parse_i64"
        && args.len() == 1
        && is_string_type(&args[0].ty)
    {
        return lower_parse_i64_into(ctx, base_slot, &args[0], code);
    }
    // A `get(list<struct>, i)` initializing a stack `Struct` local/scratch: the
    // element is a HEAP struct; `lower_list_get` deep-copies it and leaves the fresh
    // heap pointer in `rax`. Bridge it into the stack-flattened `Struct` layout by
    // flat-copying each field word `[heap + 8*k]` -> `[base + 8*k]` (fields are one
    // word each at the one-level bound). This is the heap->stack seam that lets field
    // access (`p.x`) and the by-pointer call ABI consume a mutable-heap element.
    if let (NativeType::Struct { fields, .. }, BytecodeExprKind::Call { name, args }) =
        (ty, &value.kind)
        && name == LIST_GET_BUILTIN
        && args.len() == 2
        && matches!(
            native_collection_slot(
                &args[0]
                    .ty
                    .list_element()
                    .unwrap_or_else(|| args[0].ty.clone()),
                ctx.structs,
                ctx.enums,
                0,
            ),
            Some(NativeType::HeapStruct { .. })
        )
    {
        lower_list_get(ctx, &args[0], &args[1], code)?; // rax = fresh heap-struct ptr
        code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (heap ptr)
        for word in 0..fields.len() as i32 {
            // rax = [rcx + 8*word] ; [rbp - (base + 8*word)] = rax.
            code.extend_from_slice(&[0x48, 0x8B, 0x81]); // mov rax, [rcx + disp32]
            code.extend_from_slice(&(word * 8).to_le_bytes());
            store_local(code, base_slot + word * 8);
        }
        return Ok(());
    }
    // A call to a compiled function that returns this aggregate: the callee writes
    // its result through a hidden pointer we supply. We could pass `base_slot`'s
    // address as that pointer directly, but the address must be computed relative
    // to `rbp`; do so and let the call fill it, avoiding a second copy.
    if let BytecodeExprKind::Call { name, .. } = &value.kind
        && ctx.signatures.contains_key(name.as_str())
    {
        // Materialize the call, directing its aggregate result into `base_slot`.
        return lower_aggregate_returning_call(ctx, base_slot, ty, value, code);
    }
    match (&value.kind, ty) {
        (BytecodeExprKind::Array(elements), NativeType::Array { elem, len }) => {
            if elements.len() != *len {
                return Err("array literal length does not match layout".to_string());
            }
            let stride = elem.words() as i32;
            for (index, element) in elements.iter().enumerate() {
                let word = base_slot + index as i32 * stride * 8;
                lower_value_into(ctx, word, elem, element, code)?;
            }
            Ok(())
        }
        (
            BytecodeExprKind::Call { name, args },
            NativeType::Struct {
                name: sname,
                fields,
            },
        ) => {
            if name != sname {
                return Err(format!(
                    "constructor `{name}` does not match struct layout `{sname}`"
                ));
            }
            if args.len() != fields.len() {
                return Err(format!("constructor `{name}` has wrong field count"));
            }
            let mut word = base_slot;
            for (arg, (_, field_ty)) in args.iter().zip(fields.iter()) {
                lower_value_into(ctx, word, field_ty, arg, code)?;
                word += field_ty.words() as i32 * 8;
            }
            Ok(())
        }
        (
            BytecodeExprKind::Call { name, args },
            NativeType::Enum {
                variants,
                payload_words,
                ..
            },
        ) => lower_enum_construction(ctx, base_slot, variants, *payload_words, name, args, code),
        (BytecodeExprKind::Variable(source), _) => {
            // Aggregate copy: duplicate the source local word-by-word.
            let src = ctx.local(source)?;
            if &src.ty != ty {
                return Err("aggregate copy between differing layouts".to_string());
            }
            let src_slot = src.slot;
            for word in 0..ty.words() as i32 {
                load_local(code, src_slot + word * 8);
                store_local(code, base_slot + word * 8);
            }
            Ok(())
        }
        _ => Err("initializer is not a native aggregate constructor".to_string()),
    }
}

/// Materialize an enum value into the words at `base_slot`: word 0 = the
/// variant's discriminant tag, words 1.. = its payload. `payload_words` is the
/// enum's shared payload region width; unused trailing payload words (for a
/// narrower variant) are left untouched — `match` only reads the words the
/// matched variant defines, so stale bytes are never observed. `name` is the
/// constructed variant; `args` its positional payload expressions.
fn lower_enum_construction(
    ctx: &mut NativeCtx,
    base_slot: i32,
    variants: &[NativeEnumVariant],
    _payload_words: usize,
    name: &str,
    args: &[BytecodeExpr],
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let variant = variants
        .iter()
        .find(|v| v.name == name)
        .ok_or_else(|| format!("enum constructor `{name}` is not a variant of the target enum"))?;
    if args.len() != variant.payload.len() {
        return Err(format!(
            "enum constructor `{name}` expects {} payload field(s), got {}",
            variant.payload.len(),
            args.len()
        ));
    }
    // Tag word: mov the discriminant into rax and store it at word 0.
    emit_mov_rax_imm(code, variant.tag);
    store_local(code, base_slot);
    // Payload words follow at base_slot + 8, +16, ... in field order. A float
    // payload word is materialized through xmm0; a scalar through rax.
    let mut word = base_slot + 8;
    for (arg, field_ty) in args.iter().zip(variant.payload.iter()) {
        match field_ty {
            // An integer-cell scalar OR a `string` payload is a single flat word:
            // `lower_native_expr` leaves the value (or the immutable string pointer)
            // in `rax`, stored into the payload word. A string is shared, never
            // deep-copied, so this is its exact value-semantic copy.
            NativeType::I64 | NativeType::String => {
                lower_native_expr(ctx, arg, code)?;
                store_local(code, word);
            }
            NativeType::F64 | NativeType::F32 => {
                let width = lower_native_float_expr(ctx, arg, code)?;
                store_float_local(code, word, width);
            }
            // A one-level MUTABLE-aggregate payload (`HeapStruct`/nested `List`/
            // `Map`): build/deep-copy an INDEPENDENT value pointer (so the enum owns
            // its own snapshot) and store it as the payload word.
            NativeType::HeapStruct { .. } | NativeType::List { .. } | NativeType::Map { .. } => {
                lower_heap_slot_value(ctx, field_ty, arg, code)?;
                store_local(code, word);
            }
            _ => {
                return Err(
                    "enum payload must be a native scalar, `string`, or one-level \
                     mutable aggregate"
                        .to_string(),
                );
            }
        }
        word += field_ty.words() as i32 * 8;
    }
    Ok(())
}

/// Materialize `value` (of layout `ty`) into the stack word(s) at `word_slot`.
/// Scalars go through the register path; nested aggregates recurse.
fn lower_value_into(
    ctx: &mut NativeCtx,
    word_slot: i32,
    ty: &NativeType,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    match ty {
        NativeType::I64 => {
            lower_native_expr(ctx, value, code)?;
            store_local(code, word_slot);
            Ok(())
        }
        // A float array/aggregate element: evaluate into xmm0 and store the whole
        // 8-byte (f64) or 4-byte (f32) word — mirrors the enum-payload float path.
        NativeType::F64 | NativeType::F32 => {
            let width = lower_native_float_expr(ctx, value, code)?;
            store_float_local(code, word_slot, width);
            Ok(())
        }
        _ => lower_aggregate_init(ctx, word_slot, ty, value, code),
    }
}

/// One hop of an aggregate access path: a struct field name or an array index
/// expression. Shared between statement-side (`Assign` path) and read-side
/// (`Field`/`Index` expression) resolution.
enum PathStep<'a> {
    Field(&'a str),
    Index(&'a BytecodeExpr),
}

/// Walk a root local plus a list of field/index steps down to a single scalar
/// word, accumulating a constant word offset and, if a runtime index is
/// encountered, deferring to a `Dynamic` place. A constant integer-literal index
/// folds into the constant offset (so `xs[2]` stays static); any other index
/// expression makes the place dynamic. The final layout must be `i64`.
fn resolve_place_steps(
    ctx: &NativeCtx,
    root: &str,
    steps: &[PathStep],
) -> Result<ScalarPlace, String> {
    // The strict i64-only resolver: every existing scalar/SIMD caller relies on
    // this rejecting a float element, so float arrays never reach the integer
    // load/store or the packed-integer SIMD detectors.
    let (place, ty) = resolve_place_steps_typed(ctx, root, steps)?;
    if ty != NativeType::I64 {
        return Err("native access must resolve to an i64 scalar".to_string());
    }
    Ok(place)
}

/// Like [`resolve_place_steps`] but also accepts an `f64`/`f32` final element and
/// returns the resolved element type, so the float read/store paths can pick
/// `movsd`/`movss`. Kept separate from the strict i64 resolver so the SIMD
/// detectors (which call the strict one) never fire on a float array.
fn resolve_place_steps_typed(
    ctx: &NativeCtx,
    root: &str,
    steps: &[PathStep],
) -> Result<(ScalarPlace, NativeType), String> {
    let local = ctx.local(root)?;
    let base_slot = local.slot;
    let mut ty = local.ty.clone();
    let mut const_words: i64 = 0;
    let mut dynamic: Option<(i64, i64, BytecodeExpr)> = None;

    for step in steps {
        match (step, &ty) {
            (PathStep::Field(field), NativeType::Struct { fields, .. }) => {
                let mut offset = 0i64;
                let mut found = None;
                for (fname, fty) in fields {
                    if fname == *field {
                        found = Some(fty.clone());
                        break;
                    }
                    offset += fty.words() as i64;
                }
                let fty = found.ok_or_else(|| format!("unknown field `{field}`"))?;
                const_words += offset;
                ty = fty;
            }
            (PathStep::Index(index), NativeType::Array { elem, len }) => {
                let stride = elem.words() as i64;
                if let BytecodeExprKind::Integer(literal) = index.kind {
                    // A constant index is bounds-checked at compile time: an
                    // out-of-range literal is rejected so the function skips
                    // gracefully rather than emitting an out-of-bounds access.
                    if literal < 0 || literal >= *len as i64 {
                        return Err(format!(
                            "array index `{literal}` is out of bounds for length {len}"
                        ));
                    }
                    const_words += literal * stride;
                } else if dynamic.is_none() {
                    dynamic = Some((stride, *len as i64, (*index).clone()));
                } else {
                    return Err(
                        "at most one runtime array index is supported per access".to_string()
                    );
                }
                ty = (**elem).clone();
            }
            (PathStep::Field(_), _) => {
                return Err("field access on a non-struct native value".to_string());
            }
            (PathStep::Index(_), _) => {
                return Err("index access on a non-array native value".to_string());
            }
        }
    }

    if !matches!(ty, NativeType::I64 | NativeType::F64 | NativeType::F32) {
        return Err("native access must resolve to an i64 or f64 scalar".to_string());
    }

    let place = match dynamic {
        None => ScalarPlace::Const {
            slot: base_slot + const_words as i32 * 8,
        },
        Some((elem_words, index_len, index)) => ScalarPlace::Dynamic {
            base_slot,
            const_words,
            elem_words,
            index_len,
            index,
        },
    };
    Ok((place, ty))
}

/// Read-place decomposition (like [`resolve_read_place`]) that also permits a
/// float element and returns its type — for the float `Index`/`Field` read path.
fn resolve_read_place_typed(
    ctx: &NativeCtx,
    expr: &BytecodeExpr,
) -> Result<(ScalarPlace, NativeType), String> {
    let mut steps: Vec<PathStep> = Vec::new();
    let mut cursor = expr;
    let root = loop {
        match &cursor.kind {
            BytecodeExprKind::Variable(name) => break name.as_str(),
            BytecodeExprKind::Field { target, field } => {
                steps.push(PathStep::Field(field.as_str()));
                cursor = target;
            }
            BytecodeExprKind::Index { target, index } => {
                steps.push(PathStep::Index(index));
                cursor = target;
            }
            _ => return Err("native access must be rooted at a local variable".to_string()),
        }
    };
    steps.reverse();
    resolve_place_steps_typed(ctx, root, &steps)
}

/// Resolve an assignment target `(name, path)` to a scalar place.
fn resolve_scalar_place(
    ctx: &NativeCtx,
    name: &str,
    path: &[BytecodePlace],
) -> Result<ScalarPlace, String> {
    let steps: Vec<PathStep> = path
        .iter()
        .map(|place| match place {
            BytecodePlace::Field(field) => PathStep::Field(field.as_str()),
            BytecodePlace::Index(index) => PathStep::Index(index),
        })
        .collect();
    resolve_place_steps(ctx, name, &steps)
}

/// Like [`resolve_scalar_place`] but permits a float element and returns its type
/// — for the float array-element store path (`a[i] = <f64>`).
fn resolve_scalar_place_typed(
    ctx: &NativeCtx,
    name: &str,
    path: &[BytecodePlace],
) -> Result<(ScalarPlace, NativeType), String> {
    let steps: Vec<PathStep> = path
        .iter()
        .map(|place| match place {
            BytecodePlace::Field(field) => PathStep::Field(field.as_str()),
            BytecodePlace::Index(index) => PathStep::Index(index),
        })
        .collect();
    resolve_place_steps_typed(ctx, name, &steps)
}

/// Decompose a nested `Field`/`Index` read expression into a root variable and
/// an ordered list of steps, then resolve it to a scalar place. Returns `None`
/// (as an `Err`) if the expression is not an aggregate-rooted lvalue.
fn resolve_read_place(ctx: &NativeCtx, expr: &BytecodeExpr) -> Result<ScalarPlace, String> {
    let mut steps: Vec<PathStep> = Vec::new();
    let mut cursor = expr;
    let root = loop {
        match &cursor.kind {
            BytecodeExprKind::Variable(name) => break name.as_str(),
            BytecodeExprKind::Field { target, field } => {
                steps.push(PathStep::Field(field.as_str()));
                cursor = target;
            }
            BytecodeExprKind::Index { target, index } => {
                steps.push(PathStep::Index(index));
                cursor = target;
            }
            _ => return Err("native access must be rooted at a local variable".to_string()),
        }
    };
    steps.reverse();
    resolve_place_steps(ctx, root, &steps)
}

/// Load the i64 scalar at a resolved place into `rax`.
fn emit_load_place(
    ctx: &mut NativeCtx,
    place: &ScalarPlace,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    match place {
        ScalarPlace::Const { slot } => {
            load_local(code, *slot);
            Ok(())
        }
        ScalarPlace::Dynamic { .. } => {
            emit_dynamic_addr_into_rcx(ctx, place, code)?; // rcx = &word
            code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
            Ok(())
        }
    }
}

/// Compute the effective address of a dynamic scalar word into `rcx`:
/// `rcx = rbp - (base_slot + 8*const_words) - 8*elem_words*index`.
/// Leaves the stack balanced.
fn emit_dynamic_addr_into_rcx(
    ctx: &mut NativeCtx,
    place: &ScalarPlace,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len,
        index,
    } = place
    else {
        return Err("expected a dynamic place".to_string());
    };
    // rax = index
    lower_native_expr(ctx, index, code)?;
    // Bounds check: trap on out-of-range, mirroring the interpreters' L0413.
    // One UNSIGNED compare catches both `index < 0` (a huge unsigned value) and
    // `index >= len`, so a negative or over-large index faults deterministically
    // (`ud2`) instead of reading adjacent stack memory.
    emit_bounds_check_rax(code, *index_len);
    // rax = index * elem_words   (imul rax, rax, imm32)
    emit_imul_rax_imm(code, *elem_words);
    // rax = rax * 8  -> byte stride  (shl rax, 3)
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]);
    // rcx = rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    // rcx = rcx - rax  (subtract the dynamic byte offset)
    code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
    // rcx = rcx - (base_slot + 8*const_words)  (the static displacement)
    let static_disp = *base_slot + (*const_words as i32) * 8;
    emit_sub_rcx_imm(code, static_disp);
    Ok(())
}

/// `imul rax, rax, imm32`.
fn emit_imul_rax_imm(code: &mut Vec<u8>, imm: i64) {
    code.extend_from_slice(&[0x48, 0x69, 0xC0]);
    code.extend_from_slice(&(imm as i32).to_le_bytes());
}

/// Emit an array-index bounds check on the index already in `rax`: trap with
/// `ud2` unless `0 <= rax < len`. A single UNSIGNED comparison (`cmp`+`jb`) covers
/// both ends — a negative index is a huge unsigned value, so it is `>= len` too.
/// Matches the interpreters' `L0413` (fail, don't read out of bounds); `ud2` is
/// the same deterministic trap the string-slice helper already uses. `len` is a
/// static array length that always fits `imm32`.
fn emit_bounds_check_rax(code: &mut Vec<u8>, len: i64) {
    code.extend_from_slice(&[0x48, 0x3D]); // cmp rax, imm32
    code.extend_from_slice(&(len as i32).to_le_bytes());
    code.extend_from_slice(&[0x72, 0x02]); // jb +2  (in bounds -> skip the trap)
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2    (out of bounds -> fault)
}

/// Emit a hoisted bounds guard for an auto-vectorized `for` loop over an array of
/// `len` elements, given the counter's start slot and inclusive-end slot. The
/// vectorized loop bodies address the array inline (bypassing the per-access
/// [`emit_bounds_check_rax`]), so this one-time guard at loop entry keeps them
/// memory-safe: if the loop is NON-EMPTY (`start <= end`) it traps (`ud2`) unless
/// `start >= 0` and `end < len`. The emptiness guard means an empty range (e.g.
/// `for i from 0 to n-1` with `n == 0`, i.e. `end == -1`) never false-traps.
fn emit_loop_bounds_guard(code: &mut Vec<u8>, i_slot: i32, end_slot: i32, len: i64) {
    load_local(code, i_slot); // rax = start
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg skip (start > end -> empty, no access)
    let skip_a = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // Non-empty: start >= 0 (rax still holds start) ...
    code.extend_from_slice(&[0x48, 0x83, 0xF8, 0x00]); // cmp rax, 0
    code.extend_from_slice(&[0x0F, 0x8C]); // jl trap
    let trap_a = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // ... and end < len.
    load_local(code, end_slot); // rax = end
    code.extend_from_slice(&[0x48, 0x3D]); // cmp rax, imm32 (len)
    code.extend_from_slice(&(len as i32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x83]); // jae trap (end >= len)
    let trap_b = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.push(0xE9); // jmp skip (in bounds)
    let skip_b = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // trap:
    patch_rel32(code, trap_a);
    patch_rel32(code, trap_b);
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2
    // skip:
    patch_rel32(code, skip_a);
    patch_rel32(code, skip_b);
}

// -- SSE2 integer-SIMD encoders (auto-vectorization) -------------------------
//
// x86-64 always provides SSE2, so these need no feature check. They operate on
// `xmm0` (the packed accumulator) and `xmm1` (a loaded pair), which are free in
// the i64-scalar functions that carry vectorizable array loops.

/// `movdqu xmm1, [rcx]` — load 16 unaligned bytes (two `i64`s) into `xmm1`.
fn emit_movdqu_xmm1_from_rcx(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0xF3, 0x0F, 0x6F, 0x09]);
}

/// `movdqu xmm0, [rcx]` — load 16 unaligned bytes (two `i64`s) into `xmm0`.
fn emit_movdqu_xmm0_from_rcx(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0xF3, 0x0F, 0x6F, 0x01]);
}

/// `movdqu [rcx], xmm0` — store the two packed `i64` lanes of `xmm0` to `[rcx]`.
fn emit_movdqu_rcx_from_xmm0(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0xF3, 0x0F, 0x7F, 0x01]);
}

/// Horizontally fold the two lanes of `xmm0` into `rax` with `op`: `movq rax,
/// xmm0` (low lane), `psrldq xmm0, 8` (bring the high lane low), `movq rcx,
/// xmm0`, then `rax = rax <op> rcx`. Leaves the packed reduction's scalar total
/// in `rax`.
fn emit_hfold_xmm0_into_rax(op: ReduceOp, code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]); // movq rax, xmm0 (low lane)
    code.extend_from_slice(&[0x66, 0x0F, 0x73, 0xD8, 0x08]); // psrldq xmm0, 8
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC1]); // movq rcx, xmm0 (high lane)
    op.emit_rax_rcx(code); // rax = rax <op> rcx
}

// -- SSE4.2 min/max vectorization with runtime CPUID dispatch -----------------
//
// 64-bit packed integer min/max needs a 64-bit packed compare (`pcmpgtq`), which
// is SSE4.2 — NOT in the SSE2 baseline every x86-64 CPU guarantees. So a min/max
// reduction emits BOTH a packed SSE4.2 path and a scalar fallback, and a one-time
// `cpuid` at loop entry picks between them at runtime: the produced binary uses
// the vector path on SSE4.2 hardware and the scalar path on an older CPU, staying
// correct everywhere. (`cpuid` runs once per loop entry, never per iteration, so
// its cost is amortized to nothing over the array.)

/// Emit a one-time CPUID SSE4.2 probe and a `jz` taken when SSE4.2 is ABSENT.
/// Returns the rel32 patch site of that jump for the caller to point at its scalar
/// fallback. `cpuid` clobbers eax/ebx/ecx/edx; rbx is callee-saved and may hold a
/// promoted local, so it is preserved around the probe. The SSE4.2 feature bit is
/// CPUID leaf 1, ECX bit 20; the ZF from `test` survives the `pop` (pop leaves
/// flags untouched).
fn emit_cpuid_sse42_probe(code: &mut Vec<u8>) -> usize {
    code.push(0x53); // push rbx (preserve a possibly-promoted local across cpuid)
    emit_mov_rax_imm(code, 1); // eax = 1 (feature leaf)
    code.extend_from_slice(&[0x0F, 0xA2]); // cpuid
    code.extend_from_slice(&[0x89, 0xC8]); // mov eax, ecx (feature bits -> scratch eax)
    code.push(0x5B); // pop rbx (restores the local; leaves ZF untouched)
    // test eax, 1<<20 (SSE4.2) ; jz fallback.
    code.extend_from_slice(&[0xA9]); // test eax, imm32
    code.extend_from_slice(&(1u32 << 20).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x84]); // jz rel32 (patched to the scalar fallback)
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    site
}

/// A vectorizable integer min/max reduction (`acc = max(acc, a[i])` /
/// `min(acc, a[i])`). Both are associative and commutative, so the two-lane packed
/// fold matches the scalar fold exactly.
#[derive(Clone, Copy, PartialEq)]
enum MinMaxOp {
    Min,
    Max,
}

impl MinMaxOp {
    /// The reduction identity, broadcast into both lanes of `xmm0` as the packed
    /// seed: `i64::MIN` for max, `i64::MAX` for min (so any real element wins).
    fn emit_packed_seed(self, code: &mut Vec<u8>) {
        let ident = match self {
            MinMaxOp::Max => i64::MIN,
            MinMaxOp::Min => i64::MAX,
        };
        emit_mov_rax_imm(code, ident);
        code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // movq xmm0, rax
        code.extend_from_slice(&[0x66, 0x0F, 0x6C, 0xC0]); // punpcklqdq xmm0,xmm0 (broadcast)
    }

    /// `xmm0 = minmax(xmm0, xmm1)` per lane, via the SSE4.2 `pcmpgtq` mask-blend.
    /// mask = (xmm0 > xmm1). Max keeps xmm0 where mask, xmm1 elsewhere; min is the
    /// mirror. Uses xmm2/xmm3 as scratch (free — these functions have no float
    /// locals).
    fn emit_packed(self, code: &mut Vec<u8>) {
        code.extend_from_slice(&[0x66, 0x0F, 0x6F, 0xD0]); // movdqa xmm2, xmm0
        code.extend_from_slice(&[0x66, 0x0F, 0x38, 0x37, 0xD1]); // pcmpgtq xmm2, xmm1 (mask = xmm0>xmm1)
        code.extend_from_slice(&[0x66, 0x0F, 0x6F, 0xDA]); // movdqa xmm3, xmm2 (copy mask)
        match self {
            MinMaxOp::Max => {
                code.extend_from_slice(&[0x66, 0x0F, 0xDB, 0xD8]); // pand  xmm3, xmm0 (mask & xmm0)
                code.extend_from_slice(&[0x66, 0x0F, 0xDF, 0xD1]); // pandn xmm2, xmm1 (~mask & xmm1)
            }
            MinMaxOp::Min => {
                code.extend_from_slice(&[0x66, 0x0F, 0xDB, 0xD9]); // pand  xmm3, xmm1 (mask & xmm1)
                code.extend_from_slice(&[0x66, 0x0F, 0xDF, 0xD0]); // pandn xmm2, xmm0 (~mask & xmm0)
            }
        }
        code.extend_from_slice(&[0x66, 0x0F, 0xEB, 0xDA]); // por xmm3, xmm2
        code.extend_from_slice(&[0x66, 0x0F, 0x6F, 0xC3]); // movdqa xmm0, xmm3 (result)
    }

    /// `rax = minmax(rax, rcx)` via `cmp`+`cmov` (branchless, exact for signed i64).
    fn emit_scalar_rax_rcx(self, code: &mut Vec<u8>) {
        code.extend_from_slice(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
        match self {
            // max: if rax < rcx, take rcx  -> cmovl rax, rcx
            MinMaxOp::Max => code.extend_from_slice(&[0x48, 0x0F, 0x4C, 0xC1]),
            // min: if rax > rcx, take rcx  -> cmovg rax, rcx
            MinMaxOp::Min => code.extend_from_slice(&[0x48, 0x0F, 0x4F, 0xC1]),
        }
    }

    /// `acc = minmax(acc, rax)`, honoring register promotion of `acc`. Preserves
    /// the loaded element (in rax) by moving it to rcx first.
    fn emit_reduce_into_acc(self, ctx: &NativeCtx, acc_slot: i32, code: &mut Vec<u8>) {
        code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (element)
        match ctx.promoted_reg(acc_slot) {
            Some(reg) => reg.to_rax(code),
            None => load_local(code, acc_slot),
        }
        self.emit_scalar_rax_rcx(code); // rax = minmax(acc, element)
        match ctx.promoted_reg(acc_slot) {
            Some(reg) => reg.from_rax(code),
            None => store_local(code, acc_slot),
        }
    }
}

/// The associative-and-commutative reductions that vectorize into an SSE2 packed
/// loop. Each is exact on `i64` — `+` is associative mod 2^64, and bitwise
/// `& | ^` are associative and commutative bit-for-bit — so the packed result is
/// identical to the scalar fold regardless of pairing order. (Multiplication is
/// also associative mod 2^64, but SSE2 has no 64-bit packed multiply, so it is
/// not offered here.)
#[derive(Clone, Copy, PartialEq)]
enum ReduceOp {
    Add,
    And,
    Or,
    Xor,
}

impl ReduceOp {
    /// Seed the packed accumulator (`xmm0`) with this operator's identity: all
    /// ones for `AND` (`pcmpeqd`), zero for `+`/`OR`/`XOR` (`pxor`).
    fn emit_packed_identity(self, code: &mut Vec<u8>) {
        match self {
            ReduceOp::And => code.extend_from_slice(&[0x66, 0x0F, 0x76, 0xC0]), // pcmpeqd xmm0,xmm0
            _ => code.extend_from_slice(&[0x66, 0x0F, 0xEF, 0xC0]),             // pxor xmm0,xmm0
        }
    }

    /// Combine the loaded pair (`xmm1`) into the packed accumulator: `xmm0 <op>= xmm1`.
    fn emit_packed(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            ReduceOp::Add => &[0x66, 0x0F, 0xD4, 0xC1], // paddq
            ReduceOp::And => &[0x66, 0x0F, 0xDB, 0xC1], // pand
            ReduceOp::Or => &[0x66, 0x0F, 0xEB, 0xC1],  // por
            ReduceOp::Xor => &[0x66, 0x0F, 0xEF, 0xC1], // pxor
        });
    }

    /// `rax = rax <op> rcx`.
    fn emit_rax_rcx(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            ReduceOp::Add => &[0x48, 0x01, 0xC8], // add rax, rcx
            ReduceOp::And => &[0x48, 0x21, 0xC8], // and rax, rcx
            ReduceOp::Or => &[0x48, 0x09, 0xC8],  // or  rax, rcx
            ReduceOp::Xor => &[0x48, 0x31, 0xC8], // xor rax, rcx
        });
    }

    /// `rax = rax <op> rdx`.
    fn emit_rax_rdx(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            ReduceOp::Add => &[0x48, 0x01, 0xD0], // add rax, rdx
            ReduceOp::And => &[0x48, 0x21, 0xD0], // and rax, rdx
            ReduceOp::Or => &[0x48, 0x09, 0xD0],  // or  rax, rdx
            ReduceOp::Xor => &[0x48, 0x31, 0xD0], // xor rax, rdx
        });
    }
}

/// The element-wise map operators that vectorize into an SSE2 packed loop. `+`/`-`
/// are exact mod 2^64; `& | ^` are exact bit-for-bit. All are per-lane, so the
/// packed store is identical to the scalar loop (including under destination
/// aliasing). Multiplication is excluded (no 64-bit packed multiply in SSE2).
#[derive(Clone, Copy, PartialEq)]
enum MapOp {
    Add,
    Sub,
    And,
    Or,
    Xor,
}

impl MapOp {
    /// Combine the two loaded pairs: `xmm0 <op>= xmm1` (with `xmm0` = lhs, `xmm1` = rhs).
    fn emit_packed(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            MapOp::Add => &[0x66, 0x0F, 0xD4, 0xC1], // paddq
            MapOp::Sub => &[0x66, 0x0F, 0xFB, 0xC1], // psubq
            MapOp::And => &[0x66, 0x0F, 0xDB, 0xC1], // pand
            MapOp::Or => &[0x66, 0x0F, 0xEB, 0xC1],  // por
            MapOp::Xor => &[0x66, 0x0F, 0xEF, 0xC1], // pxor
        });
    }

    /// Scalar-tail combine `lhs <op> rhs` given `rcx` = lhs, `rax` = rhs, leaving
    /// the result in `rax`. The commutative ops fold in place; `-` (non-commutative)
    /// computes `rcx - rax` then moves it into `rax`.
    fn emit_scalar_tail(self, code: &mut Vec<u8>) {
        match self {
            MapOp::Add => code.extend_from_slice(&[0x48, 0x01, 0xC8]), // add rax, rcx
            MapOp::And => code.extend_from_slice(&[0x48, 0x21, 0xC8]), // and rax, rcx
            MapOp::Or => code.extend_from_slice(&[0x48, 0x09, 0xC8]),  // or  rax, rcx
            MapOp::Xor => code.extend_from_slice(&[0x48, 0x31, 0xC8]), // xor rax, rcx
            MapOp::Sub => {
                code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax (lhs - rhs)
                code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
            }
        }
    }
}

/// The element-wise map operators over `array<f64>`: `+ - *`. Each lane is an
/// independent IEEE-754 double op, so the packed store is bit-for-bit identical to
/// the scalar loop (element-wise maps do NOT reorder, so unlike an f64 *reduction*
/// they stay parity-exact and need no fast-math opt-in).
#[derive(Clone, Copy, PartialEq)]
enum FloatMapOp {
    Add,
    Sub,
    Mul,
}

impl FloatMapOp {
    /// Packed: `xmm0 <op>= xmm1` (two f64 lanes). addpd/subpd/mulpd.
    fn emit_packed(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            FloatMapOp::Add => &[0x66, 0x0F, 0x58, 0xC1], // addpd
            FloatMapOp::Sub => &[0x66, 0x0F, 0x5C, 0xC1], // subpd
            FloatMapOp::Mul => &[0x66, 0x0F, 0x59, 0xC1], // mulpd
        });
    }
    /// Scalar tail: `xmm0 <op>= xmm1` (single f64). addsd/subsd/mulsd.
    fn emit_scalar(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            FloatMapOp::Add => &[0xF2, 0x0F, 0x58, 0xC1], // addsd
            FloatMapOp::Sub => &[0xF2, 0x0F, 0x5C, 0xC1], // subsd
            FloatMapOp::Mul => &[0xF2, 0x0F, 0x59, 0xC1], // mulsd
        });
    }
}

/// An element-wise map's element type + operator: integer (`paddq`…) or float
/// (`addpd`…). Selected by the operand type at detection; the emitter branches on
/// it for the packed op and the scalar tail.
#[derive(Clone, Copy)]
enum MapKind {
    Int(MapOp),
    Float(FloatMapOp),
}

/// `movsd xmm1, [rcx]` — load a single f64 into xmm1 (scalar-tail rhs).
fn emit_movsd_xmm1_from_rcx(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x09]);
}

/// `sub rcx, imm32` (imm may be any i32; encodes the 32-bit immediate form).
fn emit_sub_rcx_imm(code: &mut Vec<u8>, imm: i32) {
    code.extend_from_slice(&[0x48, 0x81, 0xE9]);
    code.extend_from_slice(&imm.to_le_bytes());
}

/// Lower a `match` over an enum value with scalar payloads.
///
/// Layout mirrors [`NativeType::Enum`]: the scrutinee occupies a tag word
/// followed by its payload words. This function (1) materializes the scrutinee's
/// value into a stack region — either an existing enum local (matched in place)
/// or a scratch region holding a freshly-constructed / copied enum — (2) loads
/// the tag word, and (3) dispatches: each variant arm compares the tag against
/// the variant's discriminant, binds the variant's payload words into arm-scoped
/// locals, lowers the arm body, then jumps to the shared match end. A wildcard
/// arm binds nothing and is unconditional.
///
/// When `is_value` is true, each arm leaves its result value in `rax`; the caller
/// emits the return epilogue after the shared end. When false the match is a
/// statement and any produced value in `rax` is discarded.
///
/// The tag numbering is exactly the interpreter/IR variant order (declared order
/// for a user enum; `some`(0)/`none`(1), `ok`(0)/`err`(1) for the built-ins), so
/// the arm a native `match` selects is identical to the interpreters'.
fn lower_native_match(
    ctx: &mut NativeCtx,
    scrutinee: &BytecodeExpr,
    arms: &[BytecodeMatchArm],
    is_value: bool,
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    // Resolve the scrutinee's enum layout from its static type.
    let layout = resolve_native_type(&scrutinee.ty, ctx.structs, ctx.enums)?;
    let NativeType::Enum {
        variants,
        payload_words,
        ..
    } = layout
    else {
        return Err("match scrutinee is not a native enum".to_string());
    };

    // Materialize the scrutinee into a stack region, yielding its base slot.
    // A plain enum local is matched in place; any other scrutinee is spilled to a
    // scratch region. The scratch cursor is saved and restored so sequential
    // matches reuse the same words.
    let saved_scratch = ctx.scratch_next;
    let base_slot = match &scrutinee.kind {
        BytecodeExprKind::Variable(name) if ctx.locals.contains_key(name) => {
            // Match an existing enum local in place (no copy needed).
            let local = ctx.local(name)?;
            if !matches!(local.ty, NativeType::Enum { .. }) {
                return Err("match scrutinee local is not an enum".to_string());
            }
            local.slot
        }
        BytecodeExprKind::Call { name, args } if variants.iter().any(|v| v.name == *name) => {
            // A freshly-constructed enum: materialize it directly into scratch.
            let words = 1 + payload_words;
            let base = ctx.alloc_scratch(words);
            lower_enum_construction(ctx, base, &variants, payload_words, name, args, code)?;
            base
        }
        BytecodeExprKind::Call { name, args }
            if name == MAP_GET_BUILTIN
                && args.len() == 2
                && supported_map_kv(&args[0].ty).is_some() =>
        {
            // `match map_get(m, k)`: materialize the builtin's `option<V>` result
            // (tag + payload words) directly into scratch, then dispatch on it. The
            // `map_get` lowering scans the map and writes `some(v)`/`none` into the
            // scratch region, exactly like a constructed enum scrutinee.
            let words = 1 + payload_words;
            let base = ctx.alloc_scratch(words);
            lower_map_get_into(ctx, base, &scrutinee.ty, &args[0], &args[1], code)?;
            base
        }
        BytecodeExprKind::Call { name, args }
            if matches!(overflow_builtin(name), Some((_, OverflowMode::Checked)))
                && args.len() == 2
                && fixed_int_kind(args[0].ty.name.as_str()).is_some() =>
        {
            // `match checked_<op>(a, b)`: materialize the builtin's `option<T>`
            // result (tag + payload words) directly into scratch, then dispatch on
            // it, exactly like a `map_get` option scrutinee.
            let (ovf_op, _) = overflow_builtin(name).expect("guarded overflow builtin");
            let kind = fixed_int_kind(args[0].ty.name.as_str()).expect("guarded fixed-width kind");
            let words = 1 + payload_words;
            let base = ctx.alloc_scratch(words);
            lower_native_checked_into(
                ctx,
                base,
                &scrutinee.ty,
                ovf_op,
                kind,
                &args[0],
                &args[1],
                code,
            )?;
            base
        }
        BytecodeExprKind::Call { name, args }
            if name == "parse_i64" && args.len() == 1 && is_string_type(&args[0].ty) =>
        {
            // `match parse_i64(s)`: materialize the builtin's `result<i64, string>`
            // (tag + payload words) directly into scratch, then dispatch on it, just
            // like a `map_get` option scrutinee.
            let words = 1 + payload_words;
            let base = ctx.alloc_scratch(words);
            lower_parse_i64_into(ctx, base, &args[0], code)?;
            base
        }
        BytecodeExprKind::Call { name, .. }
            if ctx
                .signatures
                .get(name.as_str())
                .is_some_and(NativeSignature::returns_aggregate) =>
        {
            // Matching the result of a call that *returns* an enum: materialize the
            // by-pointer aggregate return into scratch, then dispatch on it. The
            // aggregate-return ABI writes the tag + payload words directly into the
            // scratch destination.
            //
            // The scrutinee occupies scratch while the match runs; if the call also
            // needed scratch for by-pointer *aggregate arguments*, the shared region
            // (sized to the max, not the sum, of scrutinee vs. args) could overlap.
            // A call with only scalar arguments needs no arg scratch, so restrict to
            // that case and skip otherwise rather than risk a miscompile.
            let sig = ctx
                .signatures
                .get(name.as_str())
                .expect("guarded aggregate-returning signature");
            if sig.params.iter().any(NativeType::is_aggregate) {
                return Err(
                    "match on an enum-returning call whose arguments are aggregates is \
                     deferred on the native backend"
                        .to_string(),
                );
            }
            let enum_ty = NativeType::Enum {
                name: String::new(),
                variants: variants.clone(),
                payload_words,
            };
            let base = ctx.alloc_scratch(enum_ty.words());
            lower_aggregate_returning_call(ctx, base, &enum_ty, scrutinee, code)?;
            base
        }
        _ => {
            // Any other temporary enum scrutinee (e.g. an enum read out of an
            // aggregate field) is outside the supported set; such a function skips
            // gracefully to the interpreters rather than miscompiling.
            return Err(
                "match scrutinee must be an enum local, a freshly-constructed enum, \
                 or an enum-returning call"
                    .to_string(),
            );
        }
    };

    let mut end_jumps: Vec<usize> = Vec::new();
    let mut saw_wildcard = false;

    for arm in arms {
        match &arm.pattern {
            BytecodeMatchPattern::Wildcard => {
                // Unconditional: bind nothing, lower the body, jump to end.
                saw_wildcard = true;
                lower_match_arm_body(ctx, &arm.body, is_value, code, loops)?;
                code.push(0xE9); // jmp end
                let site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);
                end_jumps.push(site);
                // A wildcard is terminal (exhaustiveness), so stop emitting arms.
                break;
            }
            BytecodeMatchPattern::Variant { name, bindings } => {
                let variant = variants
                    .iter()
                    .find(|v| &v.name == name)
                    .ok_or_else(|| format!("match arm names unknown variant `{name}`"))?;
                // Reload the tag word each arm — arm bodies clobber rax — then
                // cmp rax, tag ; jne next_arm.
                load_local(code, base_slot);
                emit_cmp_rax_imm(code, variant.tag);
                code.extend_from_slice(&[0x0F, 0x85]); // jne rel32 (patched)
                let jne_site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);

                // Bind the matched variant's payload words into the arm locals.
                // Payload word k lives at base_slot + 8*(1 + prefix_words).
                let mut payload_word = base_slot + 8;
                for (binding, field_ty) in bindings.iter().zip(variant.payload.iter()) {
                    let dst = ctx.local_slot(binding)?;
                    match field_ty {
                        // An integer-cell scalar OR a `string` payload binds as a
                        // single flat word: load the payload word into `rax` (the
                        // value, or the immutable string pointer) and store it into
                        // the arm-scoped local. The bound string shares its pointer.
                        NativeType::I64 | NativeType::String => {
                            load_local(code, payload_word);
                            store_local(code, dst);
                        }
                        NativeType::F64 | NativeType::F32 => {
                            let width = match field_ty {
                                NativeType::F64 => FloatWidth::F64,
                                NativeType::F32 => FloatWidth::F32,
                                _ => unreachable!("guarded above"),
                            };
                            load_float_local(code, payload_word, width);
                            store_float_local(code, dst, width);
                        }
                        // A `HeapStruct` payload is bound as a STACK `Struct` local:
                        // load the payload's heap pointer, then flat-copy each field
                        // word `[ptr + 8*k]` -> `[dst + 8*k]`. The bound value is a
                        // fresh stack snapshot, so mutating it never touches the
                        // source heap struct (value semantics, matching the
                        // interpreters' cloned `match` binding).
                        NativeType::HeapStruct { fields, .. } => {
                            load_local(code, payload_word); // rax = heap-struct ptr
                            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
                            for word in 0..fields.len() as i32 {
                                code.extend_from_slice(&[0x48, 0x8B, 0x81]); // mov rax, [rcx+disp32]
                                code.extend_from_slice(&(word * 8).to_le_bytes());
                                store_local(code, dst + word * 8);
                            }
                        }
                        // A nested `List`/`Map` payload binds its (deep-copied)
                        // pointer word directly.
                        NativeType::List { .. } | NativeType::Map { .. } => {
                            load_local(code, payload_word);
                            emit_heap_slot_deep_copy(ctx, field_ty, code);
                            store_local(code, dst);
                        }
                        _ => {
                            return Err("enum payload binding is not a native scalar, `string`, \
                                 or one-level mutable aggregate"
                                .to_string());
                        }
                    }
                    payload_word += field_ty.words() as i32 * 8;
                }

                lower_match_arm_body(ctx, &arm.body, is_value, code, loops)?;
                code.push(0xE9); // jmp end
                let site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);
                end_jumps.push(site);

                // The next arm starts here (the jne target).
                patch_rel32(code, jne_site);
            }
        }
    }

    // If no wildcard covered the fallthrough, exhaustiveness guarantees one of the
    // variant arms matched, so this point is unreachable. Emit `ud2` to trap on
    // the impossible case rather than run off into the next function.
    if !saw_wildcard {
        code.extend_from_slice(&[0x0F, 0x0B]); // ud2
    }

    let end = code.len();
    for site in end_jumps {
        patch_rel32_to(code, site, end);
    }

    ctx.scratch_next = saved_scratch;
    Ok(())
}

/// Lower one match arm body. When `is_value` is true the arm's tail expression is
/// the match's result: its value is left in `rax` (an arm ending in an explicit
/// `return` emits its own epilogue instead). When false the body is a statement
/// block whose result is discarded.
fn lower_match_arm_body(
    ctx: &mut NativeCtx,
    body: &[BytecodeInstruction],
    is_value: bool,
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    if is_value && matches!(body.last(), Some(BytecodeInstruction::Expr(e)) if !e.ty.is_void()) {
        let (head, tail) = body.split_at(body.len() - 1);
        lower_native_stmts(ctx, head, code, loops)?;
        if let BytecodeInstruction::Expr(expr) = &tail[0] {
            lower_native_expr(ctx, expr, code)?;
        }
        Ok(())
    } else {
        lower_native_stmts(ctx, body, code, loops)
    }
}

/// `cmp rax, imm`. Uses the sign-extended imm32 form (`48 3D imm32`) when the
/// value fits in an i32 (every discriminant tag does), else materializes the
/// immediate into `rcx` and compares register-to-register.
fn emit_cmp_rax_imm(code: &mut Vec<u8>, imm: i64) {
    if let Ok(imm32) = i32::try_from(imm) {
        // cmp rax, imm32  (48 3D id) — sign-extended.
        code.push(0x48);
        code.push(0x3D);
        code.extend_from_slice(&imm32.to_le_bytes());
    } else {
        // mov rcx, imm64 ; cmp rax, rcx.
        code.extend_from_slice(&[0x48, 0xB9]);
        code.extend_from_slice(&imm.to_le_bytes());
        code.extend_from_slice(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
    }
}

/// If `cond` is a plain `i64`-vs-`i64` comparison, emit it fused with its branch:
/// lower both operands, `cmp rcx, rax`, then a single conditional jump taken when
/// the condition is FALSE (so control falls through into the guarded body only
/// when it holds). Returns the rel32 patch site of that jump for the caller to
/// point at the skip target, or `None` when `cond` is not a fusable i64
/// comparison — in which case the caller lowers the condition to a 0/1 in rax and
/// uses the generic `test rax,rax; jz` path.
///
/// This reuses the exact operand lowering and `cmp rcx, rax` that the boolean
/// comparison in `emit_i64_binop_from_stack` performs; it only replaces the
/// trailing `setcc; movzx rax,al; test rax,rax; jz` (four instructions that
/// materialize a 0/1 and re-test it) with one flag-based conditional jump —
/// exactly what a C compiler emits for `if (a < b)`. Fixed-width ints, floats,
/// strings, and non-comparison conditions fall back to the generic path.
fn try_emit_fused_i64_condition_branch(
    ctx: &mut NativeCtx,
    cond: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<Option<usize>, String> {
    let BytecodeExprKind::Binary { left, op, right } = &cond.kind else {
        return Ok(None);
    };
    // Only plain signed `i64` operands: the jumps below are the signed forms and
    // mirror the signed `setl`/`setle`/… the generic comparison path uses.
    if left.ty.name != "i64" || right.ty.name != "i64" {
        return Ok(None);
    }
    // Second byte of the `0F 8x` conditional jump taken when the comparison is
    // FALSE (the inverse of the operator), so the guarded body runs only when the
    // condition holds.
    let jump_when_false: u8 = match op {
        BinaryOp::Less => 0x8D,         // jge
        BinaryOp::LessEqual => 0x8F,    // jg
        BinaryOp::Greater => 0x8E,      // jle
        BinaryOp::GreaterEqual => 0x8C, // jl
        BinaryOp::Equal => 0x85,        // jne
        BinaryOp::NotEqual => 0x84,     // je
        _ => return Ok(None),
    };
    // Constant right operand (the common `n < 2` / `i < len` idiom): lower the
    // left operand into rax and compare against the immediate directly, skipping
    // the operand-stack shuffle (`emit_cmp_rax_imm` uses the imm32 form, or
    // materializes a full i64 into rcx). `cmp rax, imm` computes left - right,
    // so the same inverted jump applies.
    if let BytecodeExprKind::Integer(rhs) = &right.kind {
        // When the left operand is a promoted-register local and the immediate
        // fits imm32, compare the register directly (`cmp <reg>, imm`) instead of
        // `mov rax, <reg>; cmp rax, imm` — the common `i < len` loop-guard idiom.
        match (promoted_var_reg(ctx, left), i32::try_from(*rhs).ok()) {
            (Some(reg), Some(imm32)) => reg.cmp_imm(code, imm32),
            _ => {
                lower_native_expr(ctx, left, code)?;
                emit_cmp_rax_imm(code, *rhs); // cmp rax(left), right
            }
        }
        code.extend_from_slice(&[0x0F, jump_when_false]); // j<!cc> rel32 (patched by caller)
        let site = code.len();
        code.extend_from_slice(&[0, 0, 0, 0]);
        return Ok(Some(site));
    }

    lower_native_expr(ctx, left, code)?;
    code.push(0x50); // push rax (left)
    lower_native_expr(ctx, right, code)?; // right → rax
    code.push(0x59); // pop rcx (left)
    code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
    code.extend_from_slice(&[0x0F, jump_when_false]); // j<!cc> rel32 (patched by caller)
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    Ok(Some(site))
}

/// Lower an `if`/`elif`/`else` chain. Each branch: test the condition (fused into
/// a `cmp`+conditional jump for an `i64` comparison, else `eval into rax` +
/// `test rax,rax`); `j.. next`; body; `jmp end`. The final else falls through.
fn lower_native_if(
    ctx: &mut NativeCtx,
    branches: &[BytecodeIfBranch],
    else_body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    let mut end_jumps: Vec<usize> = Vec::new();

    for branch in branches {
        // Fused `cmp`+conditional-jump for an i64 comparison; else the generic
        // "evaluate to 0/1 in rax, `test rax,rax`, `jz`" path. Both yield a rel32
        // site that jumps to the next branch when the condition is false.
        let jz_site = match try_emit_fused_i64_condition_branch(ctx, &branch.condition, code)? {
            Some(site) => site,
            None => {
                lower_native_expr(ctx, &branch.condition, code)?;
                code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
                code.extend_from_slice(&[0x0F, 0x84]); // jz next_branch (patched below)
                let site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);
                site
            }
        };

        lower_native_stmts(ctx, &branch.body, code, loops)?;

        // jmp end (rel32, patched at the very end).
        code.push(0xE9);
        let end_site = code.len();
        code.extend_from_slice(&[0, 0, 0, 0]);
        end_jumps.push(end_site);

        // Patch the jz to land here (start of the next branch / else).
        patch_rel32(code, jz_site);
    }

    // Else body (may be empty).
    lower_native_stmts(ctx, else_body, code, loops)?;

    // Patch every branch's trailing `jmp end` to land here.
    let end = code.len();
    for site in end_jumps {
        patch_rel32_to(code, site, end);
    }
    Ok(())
}

/// Lower `while cond: body` as: top: eval cond; `test`; `jz end`; body;
/// `jmp top`; end:. `break` targets `end`, `continue` targets `top`.
/// A detected `while i < BOUND { acc = acc + i; i = i + 1 }` counting-sum loop,
/// with `acc` and `i` both promoted into callee-saved registers and `BOUND` a
/// positive `i32`-range constant. This is the ILP target: the serial `acc += i`
/// chain (one dependent add per iteration, so the loop is latency-bound at ~1
/// cycle/iter) is broken by summing a block of `K` consecutive iterations in a
/// single `acc` add — `acc += K*i + K*(K-1)/2` — which is exact under wrapping
/// arithmetic (`sum(i..i+K) mod 2^64` equals `K*i + K*(K-1)/2 mod 2^64`), so the
/// dependent add is paid once per `K` iterations instead of once per iteration.
struct SumReductionLoop {
    acc: PReg,
    counter: PReg,
    bound: i64,
}

/// The promoted register a bare `i64` local occupies, by name, or `None`.
fn promoted_reg_of_name(ctx: &NativeCtx, name: &str) -> Option<PReg> {
    ctx.promoted_reg(ctx.local_slot(name).ok()?)
}

/// If `expr` is `<promoted i64 reg> + const` or `<promoted i64 reg> - const` with
/// the constant in `i32` range, return the register and the signed displacement to
/// add — so the value can be formed with a single `lea reg2, [reg + disp]`.
fn promoted_reg_plus_const(ctx: &NativeCtx, expr: &BytecodeExpr) -> Option<(PReg, i32)> {
    let BytecodeExprKind::Binary { left, op, right } = &expr.kind else {
        return None;
    };
    if left.ty.name != "i64" || right.ty.name != "i64" {
        return None;
    }
    let reg = promoted_var_reg(ctx, left)?;
    let BytecodeExprKind::Integer(value) = &right.kind else {
        return None;
    };
    let value = i32::try_from(*value).ok()?;
    match op {
        BinaryOp::Add => Some((reg, value)),
        // `reg - v` == `lea [reg + (-v)]`; `checked_neg` guards the `i32::MIN` edge.
        BinaryOp::Subtract => value.checked_neg().map(|neg| (reg, neg)),
        _ => None,
    }
}

/// `lea rcx, [<reg> + disp32]` — form `reg ± const` directly into the first
/// argument register.
fn emit_lea_rcx_reg_disp(code: &mut Vec<u8>, reg: PReg, disp: i32) {
    // REX.W 8D /r ; ModRM mod=10 (disp32) reg=rcx(001) rm=<reg>. rbx/rsi need no SIB.
    let modrm = 0x80 | (0x01 << 3) | reg.code3();
    code.extend_from_slice(&[0x48, 0x8D, modrm]);
    code.extend_from_slice(&disp.to_le_bytes());
}

/// True when `stmt` is `target = target <Add> addend` (via `=` with a `+` RHS or
/// via `+=`), i.e. an in-place add of `addend` into the promoted local `target`.
fn is_promoted_self_add(stmt: &BytecodeInstruction, target: &str, addend: &AddendCheck) -> bool {
    let BytecodeInstruction::Assign {
        name,
        path,
        op,
        value,
        ..
    } = stmt
    else {
        return false;
    };
    if name != target || !path.is_empty() {
        return false;
    }
    match op {
        // `target += addend`
        AssignOp::Add => addend.matches(&value.kind),
        // `target = target + addend`
        AssignOp::Replace => matches!(
            &value.kind,
            BytecodeExprKind::Binary { left, op: BinaryOp::Add, right }
                if matches!(&left.kind, BytecodeExprKind::Variable(v) if v == target)
                    && addend.matches(&right.kind)
        ),
        _ => false,
    }
}

/// What the added value must be: either the counter variable, or the literal `1`.
enum AddendCheck<'a> {
    Var(&'a str),
    One,
}

impl AddendCheck<'_> {
    fn matches(&self, kind: &BytecodeExprKind) -> bool {
        match (self, kind) {
            (AddendCheck::Var(name), BytecodeExprKind::Variable(v)) => v == name,
            (AddendCheck::One, BytecodeExprKind::Integer(1)) => true,
            _ => false,
        }
    }
}

/// Recognize the counting-sum loop `while i < CONST { acc = acc + i; i = i + 1 }`
/// where `acc` and `i` are distinct promoted `i64` locals and `CONST` is a
/// positive `i32`-range constant large enough (≥ 8) that the blocked main loop is
/// worthwhile and its guard arithmetic (`bound - (K-1)`) cannot underflow. Any
/// deviation returns `None`, so the caller emits the ordinary loop unchanged.
fn detect_sum_reduction(
    ctx: &NativeCtx,
    condition: &BytecodeExpr,
    body: &[BytecodeInstruction],
) -> Option<SumReductionLoop> {
    // Condition: `i < BOUND`, `i` a promoted i64, BOUND a positive i32 constant.
    let BytecodeExprKind::Binary {
        left,
        op: BinaryOp::Less,
        right,
    } = &condition.kind
    else {
        return None;
    };
    let BytecodeExprKind::Variable(counter_name) = &left.kind else {
        return None;
    };
    let BytecodeExprKind::Integer(bound) = &right.kind else {
        return None;
    };
    if *bound < 8 || i32::try_from(*bound).is_err() {
        return None;
    }
    if left.ty.name != "i64" {
        return None;
    }

    // Body: exactly `[ acc = acc + i, i = i + 1 ]`.
    let [acc_stmt, step_stmt] = body else {
        return None;
    };
    let BytecodeInstruction::Assign { name: acc_name, .. } = acc_stmt else {
        return None;
    };
    if acc_name == counter_name {
        return None;
    }
    if !is_promoted_self_add(acc_stmt, acc_name, &AddendCheck::Var(counter_name)) {
        return None;
    }
    if !is_promoted_self_add(step_stmt, counter_name, &AddendCheck::One) {
        return None;
    }

    let acc = promoted_reg_of_name(ctx, acc_name)?;
    let counter = promoted_reg_of_name(ctx, counter_name)?;
    if acc == counter {
        return None;
    }
    Some(SumReductionLoop {
        acc,
        counter,
        bound: *bound,
    })
}

/// `lea rax, [<index>*4 + disp]` — the block sum `4*i + 6` in one instruction.
fn emit_lea_rax_index4_plus(code: &mut Vec<u8>, index: PReg, disp: i32) {
    // REX.W 8D /r ; ModRM mod=00 reg=rax(000) rm=100(SIB) ; SIB scale=4 index base=none(disp32).
    let sib = 0x80 | (index.code3() << 3) | 0x05;
    code.extend_from_slice(&[0x48, 0x8D, 0x04, sib]);
    code.extend_from_slice(&disp.to_le_bytes());
}

/// Emit the ILP-unrolled counting-sum loop: a blocked main loop summing `K`
/// consecutive counter values per iteration into `acc` (one dependent add per
/// block), then a scalar remainder loop for the final `< K` iterations.
fn emit_sum_reduction(plan: &SumReductionLoop, code: &mut Vec<u8>) {
    const K: i64 = 4;
    const BLOCK_ADD: i64 = K * (K - 1) / 2; // 0+1+2+3 = 6
    let SumReductionLoop {
        acc,
        counter,
        bound,
    } = *plan;

    // Main loop: while i < bound-(K-1), fold the whole block `i..i+K-1` in one
    // `acc` add. `bound-(K-1)` fits i32 (bound does and is ≥ 8), and the guard
    // never `lea`s before testing, so the counter cannot overflow into the block
    // computation. `4*i + 6` wrapping equals four wrapping `+= i` steps.
    let main_bound = (bound - (K - 1)) as i32;
    let main_top = code.len();
    counter.cmp_imm(code, main_bound); // cmp i, bound-3
    code.extend_from_slice(&[0x0F, 0x8D]); // jge main_end
    let main_exit = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    emit_lea_rax_index4_plus(code, counter, BLOCK_ADD as i32); // rax = 4*i + 6
    acc.add_rax(code); // acc += rax
    counter.add_imm(code, K as i32); // i += 4
    emit_jmp_to(code, main_top);
    let main_end = code.len();
    patch_rel32_to(code, main_exit, main_end);

    // Remainder: the ordinary scalar loop for the final < K iterations.
    let rem_top = code.len();
    counter.cmp_imm(code, bound as i32); // cmp i, bound
    code.extend_from_slice(&[0x0F, 0x8D]); // jge rem_end
    let rem_exit = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    acc.add_reg(code, counter); // acc += i
    counter.add_imm(code, 1); // i += 1
    emit_jmp_to(code, rem_top);
    let rem_end = code.len();
    patch_rel32_to(code, rem_exit, rem_end);
}

fn lower_native_while(
    ctx: &mut NativeCtx,
    condition: &BytecodeExpr,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    // ILP fast path: a promoted counting-sum loop folds a block of iterations per
    // step, breaking the serial `acc += i` dependency chain. Any deviation from
    // the exact shape falls through to the general loop lowering below.
    if let Some(plan) = detect_sum_reduction(ctx, condition, body) {
        emit_sum_reduction(&plan, code);
        return Ok(());
    }

    let top = code.len();
    // Fused `cmp`+conditional-jump for an i64 comparison; else the generic
    // "evaluate to 0/1 in rax, `test rax,rax`, `jz`" path. Both jump to `end`
    // when the loop condition is false.
    let exit_site = match try_emit_fused_i64_condition_branch(ctx, condition, code)? {
        Some(site) => site,
        None => {
            lower_native_expr(ctx, condition, code)?;
            code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            code.extend_from_slice(&[0x0F, 0x84]); // jz end (patched)
            let site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            site
        }
    };

    loops.push(NativeLoop {
        continue_target: Some(top),
        continue_sites: Vec::new(),
        break_sites: Vec::new(),
    });
    lower_native_stmts(ctx, body, code, loops)?;
    // Reclaim per-iteration owned string temporaries on the fallthrough back-edge
    // (RC drop insertion); `continue` (jumps to `top`) and `break` skip it and leak
    // on those paths, which is safe.
    emit_loop_body_string_drops(ctx, body, code)?;
    let loop_ctx = loops.pop().expect("loop pushed");

    emit_jmp_to(code, top); // jmp top

    let end = code.len();
    patch_rel32_to(code, exit_site, end);
    for site in loop_ctx.break_sites {
        patch_rel32_to(code, site, end);
    }
    Ok(())
}

/// Lower an infinite `loop`: top: body; `jmp top`; end:. `break` exits.
fn lower_native_loop(
    ctx: &mut NativeCtx,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    let top = code.len();
    loops.push(NativeLoop {
        continue_target: Some(top),
        continue_sites: Vec::new(),
        break_sites: Vec::new(),
    });
    lower_native_stmts(ctx, body, code, loops)?;
    emit_loop_body_string_drops(ctx, body, code)?;
    let loop_ctx = loops.pop().expect("loop pushed");

    emit_jmp_to(code, top);

    let end = code.len();
    for site in loop_ctx.break_sites {
        patch_rel32_to(code, site, end);
    }
    Ok(())
}

// -- Scope-based drop insertion (RC memory model, stage 2) --------------------
//
// Reference-counted heap blocks are reclaimed by inserting `rc_dec` (free-at-zero)
// at scope-exit. The FIRST increment targets the highest-value, provably-safe
// case: a `string` local declared directly in a LOOP body that is uniquely owned
// (a fresh allocation, never reassigned) and only ever BORROWED (used solely as
// the argument of `len`). Such a local is dead at the end of each iteration, so a
// single `rc_dec` on the fallthrough loop-body edge frees it — reclaiming what
// would otherwise leak and, for a long loop, exhaust the fixed heap region.
//
// Everything here is DEFAULT-DENY: any use that could alias, store, return, or
// pass ownership elsewhere disqualifies the local, which is then simply not
// dropped (it leaks exactly as before — never double-freed). Early-exit edges
// (`return`/`break`/`continue`) skip the fallthrough drop and leak on that path,
// which is safe; only the fallthrough (loop back-edge) frees, exactly once.

/// Whether `value` is a freshly-allocated `string` record this scope uniquely owns:
/// a string literal (materialized into a new record), a `+` concatenation, or a
/// `substring`/`trim`/`repeat` call (each always allocates a fresh record in the
/// native backend), or `to_string` of a non-string scalar. NOT a bare variable
/// (an alias), a container read, or a user-function result (unknown ownership).
fn is_owning_string_alloc(value: &BytecodeExpr) -> bool {
    if value.ty.name != "string" {
        return false;
    }
    match &value.kind {
        BytecodeExprKind::String(_) => true,
        BytecodeExprKind::Binary {
            op: BinaryOp::Add, ..
        } => true,
        BytecodeExprKind::Call { name, args } => match name.as_str() {
            "substring" | "trim" | "repeat" => true,
            "to_string" => args.len() == 1 && args[0].ty.name != "string",
            _ => false,
        },
        _ => false,
    }
}

/// Whether `value` is a freshly-allocated `array<string>` (`list<string>`-layout)
/// this scope uniquely owns: the result of `split`/`words`. (A user-function call
/// or a bare variable is not — ownership is unknown / borrowed.)
fn is_owning_string_array(value: &BytecodeExpr) -> bool {
    heap_string_array_element(&value.ty).is_some()
        && matches!(&value.kind, BytecodeExprKind::Call { name, .. } if name == "split" || name == "words")
}

/// Whether every use of the heap local `name` within `expr` is a pure borrow. For a
/// `string` local (`allow_index == false`) the only borrow is the sole argument of
/// `len(name)`. For an `array<string>` local (`allow_index == true`) `len(name[i])`
/// — reading an element's length — is additionally allowed; a bare `name[i]` (which
/// would alias an element the block owns) is NOT, since the block-drop frees the
/// elements. Any other mention lets ownership escape, so `name` is not droppable.
fn string_local_borrow_only_expr(name: &str, expr: &BytecodeExpr, allow_index: bool) -> bool {
    match &expr.kind {
        BytecodeExprKind::Variable(v) => v != name,
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Closure { .. } => true,
        BytecodeExprKind::Call { name: fname, args } => {
            if fname == "len" && args.len() == 1 {
                match &args[0].kind {
                    BytecodeExprKind::Variable(v) if v == name => true,
                    BytecodeExprKind::Index { target, index }
                        if allow_index
                            && matches!(&target.kind, BytecodeExprKind::Variable(v) if v == name) =>
                    {
                        // `len(name[i])`: the element is read for its length, not kept.
                        string_local_borrow_only_expr(name, index, allow_index)
                    }
                    _ => string_local_borrow_only_expr(name, &args[0], allow_index),
                }
            } else {
                args.iter()
                    .all(|a| string_local_borrow_only_expr(name, a, allow_index))
            }
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            string_local_borrow_only_expr(name, left, allow_index)
                && string_local_borrow_only_expr(name, right, allow_index)
        }
        BytecodeExprKind::Unary { expr, .. } | BytecodeExprKind::Await { expr } => {
            string_local_borrow_only_expr(name, expr, allow_index)
        }
        BytecodeExprKind::Index { target, index } => {
            string_local_borrow_only_expr(name, target, allow_index)
                && string_local_borrow_only_expr(name, index, allow_index)
        }
        BytecodeExprKind::Field { target, .. } => {
            string_local_borrow_only_expr(name, target, allow_index)
        }
        BytecodeExprKind::Array(elems) => elems
            .iter()
            .all(|e| string_local_borrow_only_expr(name, e, allow_index)),
    }
}

/// Whether every use of `name` across `stmts` (recursing into nested blocks) is a
/// pure borrow, and `name` is never reassigned, shadowed, or rebound. Any
/// violation disqualifies the local from dropping.
fn string_local_borrow_only_stmts(
    name: &str,
    stmts: &[BytecodeInstruction],
    allow_index: bool,
) -> bool {
    stmts
        .iter()
        .all(|s| string_local_borrow_only_stmt(name, s, allow_index))
}

fn string_local_borrow_only_stmt(
    name: &str,
    stmt: &BytecodeInstruction,
    allow_index: bool,
) -> bool {
    match stmt {
        BytecodeInstruction::Let { name: n, value, .. } => {
            n != name && string_local_borrow_only_expr(name, value, allow_index)
        }
        BytecodeInstruction::Assign {
            name: n,
            path,
            value,
            ..
        } => {
            // Any assignment targeting `name` (a rebind, or a container mutation of
            // `name`) breaks the unique-ownership assumption.
            n != name
                && path.iter().all(|p| match p {
                    BytecodePlace::Index(e) => string_local_borrow_only_expr(name, e, allow_index),
                    BytecodePlace::Field(_) => true,
                })
                && string_local_borrow_only_expr(name, value, allow_index)
        }
        BytecodeInstruction::Return(Some(e)) | BytecodeInstruction::Expr(e) => {
            string_local_borrow_only_expr(name, e, allow_index)
        }
        BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_)
        | BytecodeInstruction::Asm { .. } => true,
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().all(|b| {
                string_local_borrow_only_expr(name, &b.condition, allow_index)
                    && string_local_borrow_only_stmts(name, &b.body, allow_index)
            }) && string_local_borrow_only_stmts(name, else_body, allow_index)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => {
            string_local_borrow_only_expr(name, condition, allow_index)
                && string_local_borrow_only_stmts(name, body, allow_index)
        }
        BytecodeInstruction::For {
            name: v,
            start,
            end,
            step,
            body,
            ..
        } => {
            v != name
                && string_local_borrow_only_expr(name, start, allow_index)
                && string_local_borrow_only_expr(name, end, allow_index)
                && step
                    .as_ref()
                    .is_none_or(|s| string_local_borrow_only_expr(name, s, allow_index))
                && string_local_borrow_only_stmts(name, body, allow_index)
        }
        BytecodeInstruction::Loop { body, .. } => {
            string_local_borrow_only_stmts(name, body, allow_index)
        }
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => {
            string_local_borrow_only_expr(name, scrutinee, allow_index)
                && arms.iter().all(|a| {
                    let binds = matches!(&a.pattern, BytecodeMatchPattern::Variant { bindings, .. }
                        if bindings.iter().any(|b| b == name));
                    !binds && string_local_borrow_only_stmts(name, &a.body, allow_index)
                })
        }
        BytecodeInstruction::Throw { value, .. } => {
            string_local_borrow_only_expr(name, value, allow_index)
        }
        BytecodeInstruction::Try {
            body,
            catch_name,
            catch_body,
            ..
        } => {
            catch_name != name
                && string_local_borrow_only_stmts(name, body, allow_index)
                && string_local_borrow_only_stmts(name, catch_body, allow_index)
        }
    }
}

/// After lowering a loop `body`, emit a drop (free-at-zero) for each heap local
/// declared directly in `body` that is uniquely owned and only borrowed —
/// reclaiming the per-iteration allocation on the fallthrough back-edge. Handles
/// two cases: a `string` local (dropped by `rc_dec`), and an `array<string>` local
/// (a `split`/`words` result, dropped recursively by `__lullaby_drop_string_array`
/// — each element then the block). All are stack locals (a heap-using function is
/// never register-promoted, so the pointer is always in a stack slot).
fn emit_loop_body_string_drops(
    ctx: &mut NativeCtx,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
) -> Result<(), String> {
    for (idx, stmt) in body.iter().enumerate() {
        let BytecodeInstruction::Let { name, value, .. } = stmt else {
            continue;
        };
        let Ok(local) = ctx.local(name) else {
            continue;
        };
        let slot = local.slot;
        // A plain `string` local: fresh alloc, borrow-only (only `len(name)`).
        let is_string = matches!(local.ty, NativeType::String);
        // An `array<string>` local: a `split`/`words` result, borrow-only
        // (`len(name)` / `len(name[i])`; a bare `name[i]` would alias an element).
        let is_string_array = matches!(&local.ty, NativeType::List { elem } if matches!(**elem, NativeType::String))
            && heap_string_array_element(&stmt_let_ty(stmt)).is_some();
        let drop_symbol = if is_string && is_owning_string_alloc(value) {
            RC_DEC_SYMBOL
        } else if is_string_array && is_owning_string_array(value) {
            DROP_STRING_ARRAY_SYMBOL
        } else {
            continue;
        };
        let allow_index = is_string_array;
        if !string_local_borrow_only_stmts(name, &body[idx + 1..], allow_index) {
            continue;
        }
        // mov rcx, [rbp - slot] ; call <drop_symbol>
        code.extend_from_slice(&[0x48, 0x8B, 0x8D]);
        code.extend_from_slice(&(-slot).to_le_bytes());
        emit_call_symbol(ctx, drop_symbol, code);
    }
    Ok(())
}

/// The declared type of a `Let` instruction (its `ty` field). Used to distinguish a
/// heap `array<string>` local (which resolves to a `list<string>` NativeType) from a
/// genuine `list<string>` by the source spelling.
fn stmt_let_ty(stmt: &BytecodeInstruction) -> TypeRef {
    match stmt {
        BytecodeInstruction::Let { ty, .. } => ty.clone(),
        _ => TypeRef::new(""),
    }
}

/// Lower a range `for i = start..=end step s` to an `i64` counter loop mirroring
/// the interpreter's inclusive range: ascending stops when `i > end`, descending
/// when `i < end`. `continue` jumps to the step, `break` exits.
#[allow(clippy::too_many_arguments)]
fn lower_native_for(
    ctx: &mut NativeCtx,
    name: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    // Auto-vectorize a recognized `for i from S to E: acc += a[i]` sum reduction
    // over an `array<i64>` into an SSE2 packed loop. Anything that does not match
    // the exact shape falls through to the scalar lowering below, so correctness
    // never depends on the pattern matcher.
    if let Some(reduction) = detect_reduction(ctx, name, step, body) {
        return lower_native_vectorized_reduction(ctx, name, start, end, &reduction, code);
    }
    // Auto-vectorize `for i: acc = max(acc, a[i])` / `min(...)` via SSE4.2 with a
    // runtime CPUID gate (scalar fallback on older CPUs). Same matcher discipline.
    if let Some(minmax) = detect_minmax_reduction(ctx, name, step, body) {
        return lower_native_minmax_reduction(ctx, name, start, end, &minmax, code);
    }
    // Auto-vectorize f64 sum/dot reductions (`acc += a[i]` / `acc += a[i]*b[i]`)
    // ONLY under `--fast-math`: a 2-lane packed accumulator reorders the additions
    // (float `+` is not associative), so the result can differ from the scalar fold
    // in the last ULP. Off by default -> the reduction runs scalar and stays
    // bit-exact with the interpreters.
    if ctx.fast_math
        && let Some(red) = detect_f64_reduction(ctx, name, step, body)
    {
        return lower_native_f64_reduction(ctx, name, start, end, &red, code);
    }
    // Auto-vectorize `for i: c[i] = a[i] <op> b[i]` element-wise map. Same exact
    // matcher-with-scalar-fallback discipline as the reduction.
    if let Some(map) = detect_elementwise_map(ctx, name, step, body) {
        return lower_native_vectorized_map(ctx, name, start, end, &map, code);
    }

    // The counter and its two hidden slots (bound, step) were reserved during
    // frame planning, keyed by the counter name.
    let i_slot = ctx.local_slot(name)?;
    let end_slot = ctx.local_slot(&format!("{name}__end"))?;
    let step_slot = ctx.local_slot(&format!("{name}__step"))?;
    // The counter may be register-promoted (the bound/step stay on the stack as
    // loop-invariant memory operands). `i_reg` drives register vs stack access.
    let i_reg = ctx.promoted_reg(i_slot);
    let store_counter = |code: &mut Vec<u8>| match i_reg {
        Some(reg) => reg.from_rax(code), // reg = rax
        None => store_local(code, i_slot),
    };
    let load_counter = |code: &mut Vec<u8>| match i_reg {
        Some(reg) => reg.to_rax(code), // rax = reg
        None => load_local(code, i_slot),
    };

    // i = start
    lower_native_expr(ctx, start, code)?;
    store_counter(code);
    // end_local = end
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    // step_local = step (default 1)
    match step {
        Some(step_expr) => lower_native_expr(ctx, step_expr, code)?,
        None => emit_mov_rax_imm(code, 1),
    }
    store_local(code, step_slot);

    let top = code.len();
    // Loop guard: decide whether to run another iteration.
    // cond = (step >= 0) ? (i <= end) : (i >= end), placed in al.
    load_local(code, step_slot); // mov rax, [step]
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    // js descending  (jump if step < 0)
    code.extend_from_slice(&[0x0F, 0x88]);
    let js_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // Ascending: cond = (i <= end)  ->  setle al
    load_counter(code);
    emit_for_compare(code, end_slot, 0x9E);
    code.push(0xE9); // jmp check
    let asc_done = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // Descending: cond = (i >= end)  ->  setge al
    patch_rel32(code, js_site);
    load_counter(code);
    emit_for_compare(code, end_slot, 0x9D);

    // check: test al, al; jz end
    patch_rel32(code, asc_done);
    code.extend_from_slice(&[0x84, 0xC0]); // test al, al
    code.extend_from_slice(&[0x0F, 0x84]); // jz end (patched)
    let exit_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // `continue` jumps forward to the step block, so its target is not yet known.
    loops.push(NativeLoop {
        continue_target: None,
        continue_sites: Vec::new(),
        break_sites: Vec::new(),
    });
    lower_native_stmts(ctx, body, code, loops)?;
    // Reclaim uniquely-owned per-iteration string temporaries on the fallthrough
    // back-edge (RC drop insertion). Placed BEFORE the step label so a `continue`
    // (which jumps to the step label) skips it — leaking on that path, which is
    // safe — while the common no-`continue` body frees every iteration.
    emit_loop_body_string_drops(ctx, body, code)?;
    let loop_ctx = loops.pop().expect("loop pushed");

    // Step block (target of `continue`): i += step.
    let step_label = code.len();
    for site in loop_ctx.continue_sites {
        patch_rel32_to(code, site, step_label);
    }
    load_counter(code); // mov rax, i (register or stack)
    code.push(0x50); // push rax
    load_local(code, step_slot); // mov rax, [step]
    emit_i64_binop_from_stack(code, BinaryOp::Add)?;
    store_counter(code); // i = rax

    emit_jmp_to(code, top);

    let end = code.len();
    patch_rel32_to(code, exit_site, end);
    for site in loop_ctx.break_sites {
        patch_rel32_to(code, site, end);
    }
    Ok(())
}

/// Emit `cmp rax, [end]; set<cc> al` where the counter `i` is already in `rax`
/// and `set_opcode` is the second byte of the `0F` `setcc` form (e.g. `0x9E` =
/// setle, `0x9D` = setge). The bound stays a stack memory operand.
fn emit_for_compare(code: &mut Vec<u8>, end_slot: i32, set_opcode: u8) {
    // cmp rax, [rbp - end_slot]  ->  48 3B 85 disp32
    code.extend_from_slice(&[0x48, 0x3B, 0x85]);
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    // set<cc> al
    code.extend_from_slice(&[0x0F, set_opcode, 0xC0]);
}

/// A recognized `for i from S to E: acc <op>= a[i]` reduction over an `array<i64>`,
/// ready to vectorize. Element `k` of the array sits at
/// `[rbp - array_base_static - 8*k]`, matching the scalar index addressing.
struct Reduction {
    acc_slot: i32,
    array_base_static: i32,
    array_len: i64,
    op: ReduceOp,
}

/// Recognize a `for counter from S to E: acc <op>= array[counter]` reduction,
/// where `acc` is an `i64` local, `array` is an `array<i64>`, and `<op>` is one of
/// the vectorizable reductions: `+` (spelled `acc += array[i]`) or bitwise
/// `& | ^` (spelled `acc = acc <op> array[i]`, either operand order — they are
/// commutative). Returns `None` for anything else so the caller falls back to the
/// scalar loop. Bounds `S`/`E` are lowered by the vectorizer itself.
fn detect_reduction(
    ctx: &NativeCtx,
    counter: &str,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<Reduction> {
    // Default ascending step of 1 only.
    match step {
        None => {}
        Some(expr) => match expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    // Exactly one body statement, assigning a plain local (no field/index path).
    let [
        BytecodeInstruction::Assign {
            name: acc,
            path,
            op: assign_op,
            value,
            ..
        },
    ] = body
    else {
        return None;
    };
    if !path.is_empty() || acc == counter {
        return None;
    }
    // Determine the reduction operator and the `array[counter]` element read.
    // `+` uses the compound `acc += array[i]` (value is the bare element read);
    // bitwise ops use `acc = acc <op> array[i]` (value is a binary with `acc` as
    // one operand and the element read as the other, in either order).
    let (op, element) = match assign_op {
        AssignOp::Add => (ReduceOp::Add, value),
        AssignOp::Replace => {
            let BytecodeExprKind::Binary { left, op, right } = &value.kind else {
                return None;
            };
            let reduce_op = match op {
                BinaryOp::BitAnd => ReduceOp::And,
                BinaryOp::BitOr => ReduceOp::Or,
                BinaryOp::BitXor => ReduceOp::Xor,
                _ => return None,
            };
            // One operand must be exactly the accumulator variable; the other is
            // the element read. Bitwise ops are commutative, so accept both orders.
            let is_acc =
                |e: &BytecodeExpr| matches!(&e.kind, BytecodeExprKind::Variable(v) if v == acc);
            let element = if is_acc(left) {
                right
            } else if is_acc(right) {
                left
            } else {
                return None;
            };
            (reduce_op, element.as_ref())
        }
        _ => return None,
    };
    // The element must be `array[counter]`: resolve it as a dynamic i64 element
    // read (reusing the scalar addressing) whose index is exactly the counter.
    let BytecodeExprKind::Index { index, .. } = &element.kind else {
        return None;
    };
    let BytecodeExprKind::Variable(idx) = &index.kind else {
        return None;
    };
    if idx != counter {
        return None;
    }
    let place = resolve_read_place(ctx, element).ok()?;
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len,
        ..
    } = place
    else {
        return None;
    };
    if elem_words != 1 {
        return None; // only a contiguous i64 array is 16-byte packable
    }
    // The accumulator must be a plain `i64` local, distinct from the array root.
    let acc_local = ctx.locals.get(acc)?;
    if !matches!(acc_local.ty, NativeType::I64) {
        return None;
    }
    Some(Reduction {
        acc_slot: acc_local.slot,
        array_base_static: base_slot + const_words as i32 * 8,
        array_len: index_len,
        op,
    })
}

/// A recognized `for i from S to E: acc = max(acc, a[i])` / `min(acc, a[i])`
/// reduction over a contiguous `array<i64>`. Vectorized via SSE4.2 with a runtime
/// CPUID gate and scalar fallback (see [`lower_native_minmax_reduction`]).
struct MinMaxReduction {
    acc_slot: i32,
    array_base_static: i32,
    array_len: i64,
    op: MinMaxOp,
}

/// Recognize `for counter from S to E: acc = max(acc, array[counter])` (or `min`),
/// where `acc` is an `i64` local and `array` is a contiguous `array<i64>`. `max`/
/// `min` are commutative, so the accumulator may be either argument. Returns `None`
/// for anything else so the caller falls back to the scalar loop.
fn detect_minmax_reduction(
    ctx: &NativeCtx,
    counter: &str,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<MinMaxReduction> {
    match step {
        None => {}
        Some(expr) => match expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    let [
        BytecodeInstruction::Assign {
            name: acc,
            path,
            op: AssignOp::Replace,
            value,
            ..
        },
    ] = body
    else {
        return None;
    };
    if !path.is_empty() || acc == counter {
        return None;
    }
    // value must be `max(_, _)` / `min(_, _)` with exactly two args.
    let BytecodeExprKind::Call { name, args } = &value.kind else {
        return None;
    };
    let op = match name.as_str() {
        "max" => MinMaxOp::Max,
        "min" => MinMaxOp::Min,
        _ => return None,
    };
    let [a0, a1] = args.as_slice() else {
        return None;
    };
    // One argument is exactly the accumulator; the other is `array[counter]`.
    let is_acc = |e: &BytecodeExpr| matches!(&e.kind, BytecodeExprKind::Variable(v) if v == acc);
    let element = if is_acc(a0) {
        a1
    } else if is_acc(a1) {
        a0
    } else {
        return None;
    };
    let BytecodeExprKind::Index { index, .. } = &element.kind else {
        return None;
    };
    let BytecodeExprKind::Variable(idx) = &index.kind else {
        return None;
    };
    if idx != counter {
        return None;
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len,
        ..
    } = resolve_read_place(ctx, element).ok()?
    else {
        return None;
    };
    if elem_words != 1 {
        return None;
    }
    let acc_local = ctx.locals.get(acc)?;
    if !matches!(acc_local.ty, NativeType::I64) {
        return None;
    }
    Some(MinMaxReduction {
        acc_slot: acc_local.slot,
        array_base_static: base_slot + const_words as i32 * 8,
        array_len: index_len,
        op,
    })
}

/// A recognized `for i from S to E: c[i] = a[i] <op> b[i]` element-wise map over
/// contiguous `array<i64>`s (`op` is `+ - & | ^`). Element `k` of each array sits
/// at `[rbp - base - 8*k]`, matching the scalar index addressing.
struct ElementwiseMap {
    dest_base: i32,
    lhs_base: i32,
    rhs_base: i32,
    /// The smallest of the three arrays' lengths — the loop must stay within all
    /// of dest/lhs/rhs, so the hoisted bounds guard checks against the minimum.
    min_len: i64,
    kind: MapKind,
}

/// True when `expr` is exactly the loop counter `counter`.
fn index_is_counter(expr: &BytecodeExpr, counter: &str) -> bool {
    matches!(&expr.kind, BytecodeExprKind::Variable(v) if v == counter)
}

/// If `expr` is `array[counter]` over a contiguous `i64` array, return the array's
/// static element-0 base (`base_slot + 8*const_words`) and its element count.
fn indexed_i64_base(ctx: &NativeCtx, expr: &BytecodeExpr, counter: &str) -> Option<(i32, i64)> {
    let BytecodeExprKind::Index { index, .. } = &expr.kind else {
        return None;
    };
    if !index_is_counter(index, counter) {
        return None;
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len,
        ..
    } = resolve_read_place(ctx, expr).ok()?
    else {
        return None;
    };
    if elem_words != 1 {
        return None;
    }
    Some((base_slot + const_words as i32 * 8, index_len))
}

/// Like [`indexed_i64_base`] but for a contiguous `array<f64>` element read.
fn indexed_f64_base(ctx: &NativeCtx, expr: &BytecodeExpr, counter: &str) -> Option<(i32, i64)> {
    let BytecodeExprKind::Index { index, .. } = &expr.kind else {
        return None;
    };
    if !index_is_counter(index, counter) {
        return None;
    }
    let (place, elem_ty) = resolve_read_place_typed(ctx, expr).ok()?;
    if !matches!(elem_ty, NativeType::F64) {
        return None;
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len,
        ..
    } = place
    else {
        return None;
    };
    if elem_words != 1 {
        return None;
    }
    Some((base_slot + const_words as i32 * 8, index_len))
}

/// Recognize `for counter from S to E: dest[counter] = lhs[counter] (+|-)
/// rhs[counter]` over contiguous `array<i64>`s (default step 1). Returns `None`
/// for anything else so the caller falls back to the scalar loop.
fn detect_elementwise_map(
    ctx: &NativeCtx,
    counter: &str,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<ElementwiseMap> {
    match step {
        None => {}
        Some(expr) => match expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    let [
        BytecodeInstruction::Assign {
            name: dest,
            path,
            op: AssignOp::Replace,
            value,
            ..
        },
    ] = body
    else {
        return None;
    };
    // The destination is `dest[counter]`.
    let [BytecodePlace::Index(dest_index)] = path.as_slice() else {
        return None;
    };
    if !index_is_counter(dest_index, counter) {
        return None;
    }
    // The value is `lhs[counter] <op> rhs[counter]` for a vectorizable `op`. Try the
    // i64 forms first (`+ - & | ^`); if the operands are `array<f64>` instead, try
    // the float forms (`+ - *` via addpd/subpd/mulpd — bit-exact, per-lane).
    let BytecodeExprKind::Binary { left, op, right } = &value.kind else {
        return None;
    };
    let (lhs_base, lhs_len, rhs_base, rhs_len, kind, dest_float) = if let Some(map_op) = match op {
        BinaryOp::Add => Some(MapOp::Add),
        BinaryOp::Subtract => Some(MapOp::Sub),
        BinaryOp::BitAnd => Some(MapOp::And),
        BinaryOp::BitOr => Some(MapOp::Or),
        BinaryOp::BitXor => Some(MapOp::Xor),
        _ => None,
    }
    .filter(|_| indexed_i64_base(ctx, left, counter).is_some())
    {
        let (lb, ll) = indexed_i64_base(ctx, left, counter)?;
        let (rb, rl) = indexed_i64_base(ctx, right, counter)?;
        (lb, ll, rb, rl, MapKind::Int(map_op), false)
    } else {
        let fop = match op {
            BinaryOp::Add => FloatMapOp::Add,
            BinaryOp::Subtract => FloatMapOp::Sub,
            BinaryOp::Multiply => FloatMapOp::Mul,
            _ => return None,
        };
        let (lb, ll) = indexed_f64_base(ctx, left, counter)?;
        let (rb, rl) = indexed_f64_base(ctx, right, counter)?;
        (lb, ll, rb, rl, MapKind::Float(fop), true)
    };
    let (dest_place, dest_ty) = resolve_scalar_place_typed(ctx, dest, path).ok()?;
    // The destination element type must match the operands (i64 or f64).
    if dest_float != matches!(dest_ty, NativeType::F64) {
        return None;
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len: dest_len,
        ..
    } = dest_place
    else {
        return None;
    };
    if elem_words != 1 {
        return None;
    }
    Some(ElementwiseMap {
        dest_base: base_slot + const_words as i32 * 8,
        lhs_base,
        rhs_base,
        min_len: dest_len.min(lhs_len).min(rhs_len),
        kind,
    })
}

/// Vectorize an element-wise map `dest[i] = lhs[i] (+|-) rhs[i]` into an SSE2
/// packed loop (two `i64` lanes per iteration) with a scalar tail for the odd
/// element. Lane order is preserved because all three arrays share the same
/// reverse `[rbp - base - 8*k]` addressing, so this is bit-for-bit identical to
/// the scalar loop (and correct under `dest` aliasing `lhs`/`rhs`).
fn lower_native_vectorized_map(
    ctx: &mut NativeCtx,
    counter: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    map: &ElementwiseMap,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let i_slot = ctx.local_slot(counter)?;
    let end_slot = ctx.local_slot(&format!("{counter}__end"))?;

    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    // Hoisted bounds guard against the smallest of dest/lhs/rhs (all three are
    // indexed inline below without a per-access check).
    emit_loop_bounds_guard(code, i_slot, end_slot, map.min_len);

    // `rcx = &array[i+1]` given `rdx = 8*(i+1)`: rcx = rbp - rdx - base.
    let block_addr = |code: &mut Vec<u8>, base: i32| {
        code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
        code.extend_from_slice(&[0x48, 0x29, 0xD1]); // sub rcx, rdx
        emit_sub_rcx_imm(code, base);
    };

    // --- main SIMD loop: while i + 1 <= end, map the pair (i, i+1) ---
    let main_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg after_main
    let after_main_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3  -> 8*(i+1)
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (block offset)
    block_addr(code, map.lhs_base);
    emit_movdqu_xmm0_from_rcx(code);
    block_addr(code, map.rhs_base);
    emit_movdqu_xmm1_from_rcx(code);
    match map.kind {
        MapKind::Int(op) => op.emit_packed(code),
        MapKind::Float(op) => op.emit_packed(code),
    }
    block_addr(code, map.dest_base);
    emit_movdqu_rcx_from_xmm0(code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x02]); // add rax, 2
    store_local(code, i_slot);
    emit_jmp_to(code, main_top);

    // --- scalar remainder: while i <= end, dest[i] = lhs[i] <op> rhs[i] ---
    patch_rel32(code, after_main_site);
    let rem_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rdx = 8*i (element offset; the scalar addressing uses &array[i]).
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax
    match map.kind {
        MapKind::Int(op) => {
            // rax = lhs[i]
            block_addr(code, map.lhs_base);
            code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
            code.push(0x50); // push rax (lhs)
            // rax = rhs[i]
            block_addr(code, map.rhs_base);
            code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
            code.push(0x59); // pop rcx (rcx = lhs)
            op.emit_scalar_tail(code); // rax = lhs <op> rhs
            // dest[i] = rax
            code.push(0x50); // push rax (result)
            block_addr(code, map.dest_base);
            code.push(0x58); // pop rax (result)
            code.extend_from_slice(&[0x48, 0x89, 0x01]); // mov [rcx], rax
        }
        MapKind::Float(op) => {
            // xmm0 = lhs[i] ; xmm1 = rhs[i] ; xmm0 <op>= xmm1 ; dest[i] = xmm0.
            block_addr(code, map.lhs_base);
            load_float_from_rcx(code, FloatWidth::F64); // movsd xmm0, [rcx]
            block_addr(code, map.rhs_base);
            emit_movsd_xmm1_from_rcx(code); // movsd xmm1, [rcx]
            op.emit_scalar(code); // addsd/subsd/mulsd xmm0, xmm1
            block_addr(code, map.dest_base);
            store_float_from_rcx(code, FloatWidth::F64); // movsd [rcx], xmm0
        }
    }
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, rem_top);

    patch_rel32(code, done_site);
    Ok(())
}

/// `acc = acc <op> rax`, honoring register promotion of the accumulator.
/// Preserves the operand (the loaded element or packed total) in `rdx` while
/// loading/combining/storing `acc`.
fn emit_reduce_into_acc(ctx: &NativeCtx, acc_slot: i32, op: ReduceOp, code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (save operand)
    match ctx.promoted_reg(acc_slot) {
        Some(reg) => reg.to_rax(code), // rax = acc
        None => load_local(code, acc_slot),
    }
    op.emit_rax_rdx(code); // rax = acc <op> rdx
    match ctx.promoted_reg(acc_slot) {
        Some(reg) => reg.from_rax(code), // acc = rax
        None => store_local(code, acc_slot),
    }
}

/// Emit the vectorized reduction `acc <op>= a[S..=E]`. Combines the array two
/// `i64`s at a time with the packed op (`paddq`/`pand`/`por`/`pxor`), horizontally
/// folds the packed accumulator into `acc`, then a scalar tail loop handles a
/// final odd element. The counter and bound live on the stack for this loop (the
/// counter is dead after it). Every offered op is associative (and, for bitwise,
/// commutative) and exact on `i64`, so the total matches the scalar fold
/// bit-for-bit regardless of the pairing order.
fn lower_native_vectorized_reduction(
    ctx: &mut NativeCtx,
    counter: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    reduction: &Reduction,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let i_slot = ctx.local_slot(counter)?;
    let end_slot = ctx.local_slot(&format!("{counter}__end"))?;
    let base = reduction.array_base_static;
    let op = reduction.op;

    // i = start ; end_local = end
    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    // Hoisted bounds guard: the inline-addressed loop below bypasses the per-access
    // check, so trap here if the (non-empty) index range escapes the array.
    emit_loop_bounds_guard(code, i_slot, end_slot, reduction.array_len);
    op.emit_packed_identity(code); // packed accumulator = identity

    // --- main SIMD loop: while i + 1 <= end, combine the pair (a[i], a[i+1]) ---
    let main_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1  -> rax = i+1
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg after_main (i+1 > end)
    let after_main_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // addr of the 16-byte block = rbp - base - 8*(i+1); rax already holds i+1.
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3  -> 8*(i+1)
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
    emit_sub_rcx_imm(code, base); // rcx = &a[i+1] (block start; covers a[i+1],a[i])
    emit_movdqu_xmm1_from_rcx(code);
    op.emit_packed(code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x02]); // add rax, 2
    store_local(code, i_slot);
    emit_jmp_to(code, main_top);

    // after_main: fold the two packed lanes into acc.
    patch_rel32(code, after_main_site);
    emit_hfold_xmm0_into_rax(op, code); // rax = lane0 <op> lane1
    emit_reduce_into_acc(ctx, reduction.acc_slot, op, code);

    // --- scalar remainder: while i <= end, combine a[i] ---
    let rem_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done (i > end)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // load a[i]: addr = rbp - base - 8*i
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
    emit_sub_rcx_imm(code, base);
    code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
    emit_reduce_into_acc(ctx, reduction.acc_slot, op, code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, rem_top);

    patch_rel32(code, done_site);
    Ok(())
}

/// Emit a min/max reduction `acc = max(acc, a[S..=E])` (or `min`) with runtime
/// CPUID dispatch: an SSE4.2 packed path (`pcmpgtq` mask-blend, two `i64` lanes per
/// iteration + scalar tail) when the CPU has SSE4.2, else a plain scalar fold. Both
/// paths fold the array into `acc`, so the result is identical (min/max is
/// associative and commutative). The `cpuid` probe runs once at loop entry.
fn lower_native_minmax_reduction(
    ctx: &mut NativeCtx,
    counter: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    red: &MinMaxReduction,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let i_slot = ctx.local_slot(counter)?;
    let end_slot = ctx.local_slot(&format!("{counter}__end"))?;
    let base = red.array_base_static;
    let op = red.op;

    // i = start ; end_local = end  (set before the probe so the scalar fallback,
    // which the probe jumps to BEFORE any SIMD code runs, starts from `start`).
    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    // Hoisted bounds guard (both the SSE4.2 and scalar paths below index inline).
    emit_loop_bounds_guard(code, i_slot, end_slot, red.array_len);

    // Runtime CPUID gate: jump to the scalar fallback when SSE4.2 is absent.
    let fallback_site = emit_cpuid_sse42_probe(code);

    // --- SSE4.2 packed path ---
    op.emit_packed_seed(code); // xmm0 = identity broadcast to both lanes
    let main_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1  (i+1)
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg after_main
    let after_main_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3  -> 8*(i+1)
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
    emit_sub_rcx_imm(code, base); // rcx = &a[i+1] (block start)
    emit_movdqu_xmm1_from_rcx(code);
    op.emit_packed(code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x02]); // add rax, 2
    store_local(code, i_slot);
    emit_jmp_to(code, main_top);

    // Fold the two packed lanes, then into acc.
    patch_rel32(code, after_main_site);
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]); // movq rax, xmm0 (lane0)
    code.extend_from_slice(&[0x66, 0x0F, 0x73, 0xD8, 0x08]); // psrldq xmm0, 8
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC1]); // movq rcx, xmm0 (lane1)
    op.emit_scalar_rax_rcx(code); // rax = minmax(lane0, lane1)
    op.emit_reduce_into_acc(ctx, red.acc_slot, code);

    // Scalar tail for the odd final element (SSE4.2 path).
    let tail_top = code.len();
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg simd_done
    let simd_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    emit_load_array_elem(code, i_slot, base); // rax = a[i]
    op.emit_reduce_into_acc(ctx, red.acc_slot, code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, tail_top);
    patch_rel32(code, simd_done_site);
    // Skip the scalar fallback.
    code.push(0xE9);
    let done_jmp_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // --- scalar fallback (no SSE4.2): fold every element into acc ---
    patch_rel32(code, fallback_site);
    let scalar_top = code.len();
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done
    let scalar_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    emit_load_array_elem(code, i_slot, base); // rax = a[i]
    op.emit_reduce_into_acc(ctx, red.acc_slot, code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, scalar_top);

    patch_rel32(code, scalar_done_site);
    patch_rel32(code, done_jmp_site); // both fallthroughs land here
    Ok(())
}

/// `rax = a[i]` for a contiguous i64 array whose element 0 sits at `rbp - base`:
/// addr = rbp - base - 8*i.
fn emit_load_array_elem(code: &mut Vec<u8>, i_slot: i32, base: i32) {
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
    emit_sub_rcx_imm(code, base);
    code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
}

/// `movsd xmm1, [rbp - slot]` — load an f64 local into xmm1.
fn emit_movsd_xmm1_from_local(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x8D]); // movsd xmm1, [rbp + disp32]
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// A recognized f64 reduction: `for i: acc += a[i]` (sum) or `acc += a[i]*b[i]`
/// (dot). Only vectorized under `--fast-math` (the 2-lane packed fold reorders the
/// additions). `rhs_base` is `Some` for a dot product, `None` for a plain sum.
struct F64Reduction {
    acc_slot: i32,
    lhs_base: i32,
    rhs_base: Option<i32>,
    array_len: i64,
}

/// Recognize `for counter from S to E: acc += a[counter]` or
/// `acc += a[counter] * b[counter]` where `acc` is an `f64` local and the arrays are
/// `array<f64>`. Returns `None` for anything else (scalar fallback).
fn detect_f64_reduction(
    ctx: &NativeCtx,
    counter: &str,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<F64Reduction> {
    match step {
        None => {}
        Some(expr) => match expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    let [
        BytecodeInstruction::Assign {
            name: acc,
            path,
            op: AssignOp::Add,
            value,
            ..
        },
    ] = body
    else {
        return None;
    };
    if !path.is_empty() {
        return None;
    }
    let acc_local = ctx.locals.get(acc)?;
    if !matches!(acc_local.ty, NativeType::F64) {
        return None;
    }
    match &value.kind {
        // sum: acc += a[i]
        BytecodeExprKind::Index { .. } => {
            let (lhs_base, len) = indexed_f64_base(ctx, value, counter)?;
            Some(F64Reduction {
                acc_slot: acc_local.slot,
                lhs_base,
                rhs_base: None,
                array_len: len,
            })
        }
        // dot: acc += a[i] * b[i]
        BytecodeExprKind::Binary {
            left,
            op: BinaryOp::Multiply,
            right,
        } => {
            let (lhs_base, ll) = indexed_f64_base(ctx, left, counter)?;
            let (rhs_base, rl) = indexed_f64_base(ctx, right, counter)?;
            Some(F64Reduction {
                acc_slot: acc_local.slot,
                lhs_base,
                rhs_base: Some(rhs_base),
                array_len: ll.min(rl),
            })
        }
        _ => None,
    }
}

/// Emit an f64 sum/dot reduction with a 2-lane packed accumulator: `pxor` seeds
/// `xmm0` to `0.0`; the main loop `movdqu`-loads a pair (and `mulpd`s the b-pair
/// for a dot), `addpd`s into `xmm0`; then the two lanes fold (`unpckhpd`+`addsd`,
/// SSE2) into the `acc` local, and a scalar tail (`addsd`/`mulsd`) handles the odd
/// element. `--fast-math` only (the packed pairing reorders the additions).
fn lower_native_f64_reduction(
    ctx: &mut NativeCtx,
    counter: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    red: &F64Reduction,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let i_slot = ctx.local_slot(counter)?;
    let end_slot = ctx.local_slot(&format!("{counter}__end"))?;
    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    emit_loop_bounds_guard(code, i_slot, end_slot, red.array_len);
    code.extend_from_slice(&[0x66, 0x0F, 0xEF, 0xC0]); // pxor xmm0, xmm0 (packed acc = 0)

    // `rcx = &array[i+1]` given `rdx = 8*(i+1)`: rcx = rbp - rdx - base.
    let block_addr = |code: &mut Vec<u8>, base: i32| {
        code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
        code.extend_from_slice(&[0x48, 0x29, 0xD1]); // sub rcx, rdx
        emit_sub_rcx_imm(code, base);
    };

    // --- main SIMD loop: while i + 1 <= end, accumulate the pair ---
    let main_top = code.len();
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    code.extend_from_slice(&[0x48, 0x3B, 0x85]);
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg after_main
    let after_main = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (block offset)
    block_addr(code, red.lhs_base);
    emit_movdqu_xmm1_from_rcx(code); // xmm1 = a pair
    if let Some(rhs_base) = red.rhs_base {
        block_addr(code, rhs_base);
        code.extend_from_slice(&[0xF3, 0x0F, 0x6F, 0x11]); // movdqu xmm2, [rcx] (b pair)
        code.extend_from_slice(&[0x66, 0x0F, 0x59, 0xCA]); // mulpd xmm1, xmm2
    }
    code.extend_from_slice(&[0x66, 0x0F, 0x58, 0xC1]); // addpd xmm0, xmm1
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x02]); // add rax, 2
    store_local(code, i_slot);
    emit_jmp_to(code, main_top);

    // after_main: fold the two lanes, then add into acc.
    patch_rel32(code, after_main);
    code.extend_from_slice(&[0x66, 0x0F, 0x28, 0xC8]); // movapd xmm1, xmm0
    code.extend_from_slice(&[0x66, 0x0F, 0x15, 0xC9]); // unpckhpd xmm1, xmm1 (high lane -> low)
    code.extend_from_slice(&[0xF2, 0x0F, 0x58, 0xC1]); // addsd xmm0, xmm1 (lane0 = lane0+lane1)
    emit_movsd_xmm1_from_local(code, red.acc_slot); // xmm1 = acc
    code.extend_from_slice(&[0xF2, 0x0F, 0x58, 0xC1]); // addsd xmm0, xmm1 (acc + packed sum)
    store_float_local(code, red.acc_slot, FloatWidth::F64);

    // --- scalar tail: while i <= end, acc += a[i] (* b[i]) ---
    let rem_top = code.len();
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x3B, 0x85]);
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done
    let done = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax
    block_addr(code, red.lhs_base);
    load_float_from_rcx(code, FloatWidth::F64); // xmm0 = a[i]
    if let Some(rhs_base) = red.rhs_base {
        block_addr(code, rhs_base);
        emit_movsd_xmm1_from_rcx(code); // xmm1 = b[i]
        code.extend_from_slice(&[0xF2, 0x0F, 0x59, 0xC1]); // mulsd xmm0, xmm1
    }
    emit_movsd_xmm1_from_local(code, red.acc_slot); // xmm1 = acc
    code.extend_from_slice(&[0xF2, 0x0F, 0x58, 0xC1]); // addsd xmm0, xmm1
    store_float_local(code, red.acc_slot, FloatWidth::F64);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, rem_top);

    patch_rel32(code, done);
    Ok(())
}

// -- Expression lowering (result left in rax) --------------------------------

fn lower_native_expr(
    ctx: &mut NativeCtx,
    expr: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    match &expr.kind {
        BytecodeExprKind::Integer(value) => {
            emit_mov_rax_imm(code, *value);
            Ok(())
        }
        // A `bool` literal is a `0`/`1` cell; a `char` literal is its Unicode
        // scalar value (code point). Both are single normalized `i64` cells — the
        // same representation the interpreters use — so they load as an immediate.
        // These reach here mainly through `to_string(true)` / `to_string('x')`, but
        // a bool/char value is a valid `i64`-cell scalar wherever one is expected.
        BytecodeExprKind::Bool(value) => {
            emit_mov_rax_imm(code, i64::from(*value));
            Ok(())
        }
        BytecodeExprKind::Char(value) => {
            emit_mov_rax_imm(code, i64::from(u32::from(*value)));
            Ok(())
        }
        BytecodeExprKind::Variable(name) => {
            let slot = ctx.local_slot(name)?;
            match ctx.promoted_reg(slot) {
                Some(reg) => reg.to_rax(code),
                None => load_local(code, slot),
            }
            Ok(())
        }
        BytecodeExprKind::Unary { op, expr: inner } => match op {
            lullaby_parser::UnaryOp::Not => {
                // Boolean `not`: rax = (inner == 0) ? 1 : 0.
                lower_native_expr(ctx, inner, code)?;
                code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
                code.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
                code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
                Ok(())
            }
            // Integer bitwise NOT (`~`). On a fixed-width kind it is one's
            // complement re-normalized to the width, matching the interpreter's
            // `Value::int(!v, ty)`. On plain `i64` it is a full-width `not`.
            lullaby_parser::UnaryOp::BitNot => {
                lower_native_expr(ctx, inner, code)?;
                code.extend_from_slice(&[0x48, 0xF7, 0xD0]); // not rax
                if let Some(kind) = fixed_int_kind(inner.ty.name.as_str()) {
                    emit_normalize_rax(code, kind);
                }
                Ok(())
            }
            // Integer arithmetic negation (`-x`). Wrapping `neg`, re-normalized on
            // a fixed-width kind. (Float `-x` is handled on the float path.)
            lullaby_parser::UnaryOp::Negate => {
                lower_native_expr(ctx, inner, code)?;
                code.extend_from_slice(&[0x48, 0xF7, 0xD8]); // neg rax
                if let Some(kind) = fixed_int_kind(inner.ty.name.as_str()) {
                    emit_normalize_rax(code, kind);
                }
                Ok(())
            }
        },
        BytecodeExprKind::Binary { left, op, right } => {
            lower_native_binary(ctx, left, *op, right, code)
        }
        BytecodeExprKind::Call { name, args } => {
            // Fixed-width integer conversions are emitted inline, not as calls.
            // `to_<T>(x)` normalizes the argument's `i64` cell into `T`'s width
            // (truncate + sign/zero-extend), matching the interpreter's
            // `Value::int(x, T)`. `to_i64(x)` widens a fixed-width cell to `i64`,
            // which is the identity on the already-normalized cell.
            if let Some(kind) = to_int_conversion_kind(name) {
                if args.len() != 1 {
                    return Err(format!("`{name}` takes exactly one argument"));
                }
                lower_native_expr(ctx, &args[0], code)?;
                emit_normalize_rax(code, kind);
                return Ok(());
            }
            if name == "to_i64" {
                if args.len() != 1 {
                    return Err("`to_i64` takes exactly one argument".to_string());
                }
                // The source cell is already normalized; widening to `i64` keeps
                // the bits unchanged.
                lower_native_expr(ctx, &args[0], code)?;
                return Ok(());
            }
            // `to_string(x)` produces a fresh heap string record (a pointer in
            // `rax`), matching the interpreters' `Display`/`builtin_to_string`:
            //   * an integer (`i64`/fixed-width) → decimal digits, signed or
            //     unsigned by the argument's kind (`byte` prints its 0..=255 value);
            //   * `bool` → `"true"`/`"false"`;
            //   * `char` → the code point's UTF-8 encoding;
            //   * `string` → identity (the argument's pointer is already the value).
            // Float `to_string` (dtoa) is deferred and falls back to the
            // interpreters (rejected here so the function skips gracefully).
            if name == "to_string" {
                if args.len() != 1 {
                    return Err("`to_string` takes exactly one argument".to_string());
                }
                return lower_to_string(ctx, &args[0], code);
            }
            // Growable `list<T>` (scalar `T`) builtins. `list_new()` allocates a
            // fresh `[len=0][cap=LIST_INITIAL_CAP][slots]` heap block; `push`/`set`/
            // `pop` are value-semantic (they deep-copy their source and mutate the
            // copy); `get` loads element `i`; `len(l)` loads the list's `len`
            // header. Each leaves a pointer (or, for `get`/`len`, an `i64`) in
            // `rax`. Dispatched by the (scalar-element) list type of the operand.
            if name == LIST_NEW_BUILTIN {
                if !args.is_empty() {
                    return Err("list_new expects 0 arguments".to_string());
                }
                lower_list_new(ctx, code);
                return Ok(());
            }
            if name == LIST_PUSH_BUILTIN
                && args.len() == 2
                && supported_list_element(&args[0].ty).is_some()
            {
                lower_list_push(ctx, &args[0], &args[1], code)?;
                return Ok(());
            }
            if name == LIST_SET_BUILTIN
                && args.len() == 3
                && supported_list_element(&args[0].ty).is_some()
            {
                lower_list_set(ctx, &args[0], &args[1], &args[2], code)?;
                return Ok(());
            }
            if name == LIST_POP_BUILTIN
                && args.len() == 1
                && supported_list_element(&args[0].ty).is_some()
            {
                lower_list_pop(ctx, &args[0], code)?;
                return Ok(());
            }
            if name == LIST_GET_BUILTIN
                && args.len() == 2
                && supported_list_element(&args[0].ty).is_some()
            {
                lower_list_get(ctx, &args[0], &args[1], code)?;
                return Ok(());
            }
            // `len(l)` on a growable list loads its `len` header word.
            if name == "len"
                && args.len() == 1
                && supported_list_element(&args[0].ty).is_some()
            {
                lower_native_expr(ctx, &args[0], code)?; // list pointer -> rax
                // mov rax, [rax + LIST_LEN_OFF]
                emit_mov_rax_from_rax_disp(code, LIST_LEN_OFF);
                return Ok(());
            }
            // `len(a)` on a heap `array<string>` reads the same `len` header word
            // (it shares the `list<string>` block layout).
            if name == "len"
                && args.len() == 1
                && heap_string_array_element(&args[0].ty).is_some()
            {
                lower_native_expr(ctx, &args[0], code)?; // block pointer -> rax
                emit_mov_rax_from_rax_disp(code, LIST_LEN_OFF);
                return Ok(());
            }
            // `split(text, sep) -> array<string>`: stage the two string operands into
            // rcx/rdx and call the split helper, which builds a fresh `list<string>`
            // block of the fields. The result is a single pointer word (like
            // `list_new`), so it lowers here in the scalar path.
            if name == "split"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(ctx, &args[0], &args[1], STR_SPLIT_SYMBOL, code);
            }
            // `join(a, sep) -> string`: stage the `array<string>` block and the
            // separator into rcx/rdx and call the join helper (a fresh record).
            if name == "join"
                && args.len() == 2
                && heap_string_array_element(&args[0].ty).is_some()
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(ctx, &args[0], &args[1], STR_JOIN_SYMBOL, code);
            }
            // Growable `map<K, V>` (scalar key/value) builtins with a *scalar*
            // result. `map_new()` allocates an empty header; `map_set` deep-copies
            // then updates-or-appends (value-semantic, returns the fresh map
            // pointer); `map_has` scans to a `bool`; `map_len` reads the `len`
            // header. `map_get` returns `option<V>` (an aggregate), so it is lowered
            // in the aggregate paths (`lower_aggregate_init` / a `match` scrutinee),
            // not here. Each name is unique (no array/list op shares it), so they
            // dispatch by name; the key/value types are validated in each lowering.
            if name == MAP_NEW_BUILTIN {
                if !args.is_empty() {
                    return Err("map_new expects 0 arguments".to_string());
                }
                lower_map_new(ctx, code);
                return Ok(());
            }
            if name == MAP_SET_BUILTIN
                && args.len() == 3
                && supported_map_kv(&args[0].ty).is_some()
            {
                lower_map_set(ctx, &args[0], &args[1], &args[2], code)?;
                return Ok(());
            }
            if name == MAP_HAS_BUILTIN
                && args.len() == 2
                && supported_map_kv(&args[0].ty).is_some()
            {
                lower_map_has(ctx, &args[0], &args[1], code)?;
                return Ok(());
            }
            if name == MAP_LEN_BUILTIN
                && args.len() == 1
                && supported_map_kv(&args[0].ty).is_some()
            {
                lower_native_expr(ctx, &args[0], code)?; // map pointer -> rax
                // mov rax, [rax + MAP_LEN_OFF]
                emit_mov_rax_from_rax_disp(code, MAP_LEN_OFF);
                return Ok(());
            }
            // `char_code(c)`: a `char` is stored as its Unicode code point in an i64
            // cell, so `char_code` is the identity on that cell (matches the
            // interpreters' `char as i64`).
            if name == "char_code" && args.len() == 1 && args[0].ty.name == "char" {
                lower_native_expr(ctx, &args[0], code)?;
                return Ok(());
            }
            // `is_digit(c)`: 1 when the code point is an ASCII digit `'0'..='9'`
            // (48..=57), else 0 — matching the interpreters' `is_ascii_digit`. One
            // unsigned range test: `(c - 48) <= 9` (a `c < 48` underflows to a huge
            // unsigned value that is not `<= 9`). Other `is_*` predicates are
            // Unicode-aware and stay on the interpreters.
            if name == "is_digit" && args.len() == 1 {
                lower_native_expr(ctx, &args[0], code)?; // c -> rax
                code.extend_from_slice(&[0x48, 0x83, 0xE8, 0x30]); // sub rax, 48
                code.extend_from_slice(&[0x48, 0x83, 0xF8, 0x09]); // cmp rax, 9
                code.extend_from_slice(&[0x0F, 0x96, 0xC0]); // setbe al
                code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
                return Ok(());
            }
            // `len(arr)` over a fixed native array folds to a compile-time
            // constant (arrays never grow in the native subset).
            if name == "len"
                && args.len() == 1
                && let BytecodeExprKind::Variable(var) = &args[0].kind
                && let Ok(local) = ctx.local(var)
                && let NativeType::Array { len, .. } = &local.ty
            {
                emit_mov_rax_imm(code, *len as i64);
                return Ok(());
            }
            // `len(string_literal)` is the first heap-backed native string op.
            // The literal's bytes live in `.rdata`; `__lullaby_strlen_copy` bump-
            // allocates a heap copy of them, scans the copy for its terminator,
            // and returns the byte length in rax (== the interpreter's char count
            // for the ASCII strings this subset accepts). This exercises the
            // whole first heap step end to end: a `.rdata` constant, a REL32
            // relocation to its address, the bump allocator, and per-byte reads
            // of both `.rdata` and the heap.
            if name == "len"
                && args.len() == 1
                && let BytecodeExprKind::String(text) = &args[0].kind
            {
                if !text.is_ascii() {
                    return Err("native string len supports ASCII string literals only".to_string());
                }
                let symbol = ctx.strings.intern(text);
                // lea rcx, [rip + __str] ; the 4-byte rel32 is a REL32 relocation.
                code.extend_from_slice(&[0x48, 0x8D, 0x0D]);
                let site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);
                ctx.relocations.push(CodeRelocation {
                    offset: site as u32,
                    symbol,
                });
                // call __lullaby_strlen_copy (rel32 relocation).
                code.push(0xE8);
                let call_site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);
                ctx.relocations.push(CodeRelocation {
                    offset: call_site as u32,
                    symbol: HEAP_STRLEN_SYMBOL.to_string(),
                });
                return Ok(());
            }
            // `len(s)` on any other string VALUE (a variable, a parameter, a
            // concatenation result, a `to_string` result, …) reads the `char_len`
            // header of the record the string pointer addresses. This gives the
            // Unicode scalar count for arbitrary UTF-8 strings, not only ASCII
            // literals.
            if name == "len" && args.len() == 1 && is_string_type(&args[0].ty) {
                lower_native_expr(ctx, &args[0], code)?; // string pointer -> rax
                emit_mov_rax_from_rax_disp(code, STR_CHAR_LEN_OFF);
                return Ok(());
            }
            // Index-based string operations over the heap `[char_len][byte_len]
            // [utf8]` record. Each stages its string (and index) operands into the
            // Win64 argument registers and calls a `.text` helper; the helper does
            // the UTF-8-aware scanning and (for `substring`) allocation, exactly
            // matching the interpreters' semantics (char-indexed `substring`/`find`,
            // byte-wise `contains`/`starts_with`/`ends_with`). Guarded by the string
            // operand type so a same-named user function still resolves as a call.
            if name == "substring"
                && args.len() == 3
                && is_string_type(&args[0].ty)
                && args[1].ty.name == "i64"
                && args[2].ty.name == "i64"
            {
                return lower_string_substring(ctx, &args[0], &args[1], &args[2], code);
            }
            if name == "find"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(ctx, &args[0], &args[1], STR_FIND_SYMBOL, code);
            }
            // `count(text, sub)`: non-overlapping occurrence count (i64). Guarded on
            // two string operands so a user-defined `count` still resolves as a call.
            if name == "count"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(ctx, &args[0], &args[1], STR_COUNT_SYMBOL, code);
            }
            // `repeat(text, count)`: text repeated `count` times (a fresh record).
            if name == "repeat"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && args[1].ty.name == "i64"
            {
                return lower_string_repeat(ctx, &args[0], &args[1], code);
            }
            // `trim(text)`: leading/trailing ASCII whitespace removed (fresh record).
            if name == "trim" && args.len() == 1 && is_string_type(&args[0].ty) {
                return lower_string_trim(ctx, &args[0], code);
            }
            if name == "contains"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(ctx, &args[0], &args[1], STR_CONTAINS_SYMBOL, code);
            }
            if name == "starts_with"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(
                    ctx,
                    &args[0],
                    &args[1],
                    STR_STARTS_WITH_SYMBOL,
                    code,
                );
            }
            if name == "ends_with"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(ctx, &args[0], &args[1], STR_ENDS_WITH_SYMBOL, code);
            }
            // Overflow-aware arithmetic builtins. `wrapping_*` reuses the default
            // fixed-width `+`/`-`/`*` (wrap then normalize); `saturating_*` clamps
            // to `T`'s bounds. `checked_*` yields `option<T>` (an aggregate) and is
            // lowered in the aggregate positions (binding/return via
            // `lower_aggregate_init`, or a `match` scrutinee), never as a scalar,
            // so it is not handled here. Guarded by a fixed-width first operand so
            // the names cannot shadow a user function of the same spelling.
            if let Some((ovf_op, mode)) = overflow_builtin(name)
                && args.len() == 2
                && let Some(kind) = fixed_int_kind(args[0].ty.name.as_str())
            {
                match mode {
                    OverflowMode::Wrapping => {
                        lower_native_expr(ctx, &args[0], code)?;
                        code.push(0x50); // push rax (left)
                        lower_native_expr(ctx, &args[1], code)?; // right in rax
                        emit_fixed_binop_from_stack(code, ovf_op.binary_op(), kind)?;
                        return Ok(());
                    }
                    OverflowMode::Saturating => {
                        return lower_native_saturating(ctx, ovf_op, kind, &args[0], &args[1], code);
                    }
                    OverflowMode::Checked => {}
                }
            }
            if !ctx.callable.contains(name.as_str()) {
                return Err(format!(
                    "call to non-i64-scalar or unknown function `{name}`"
                ));
            }
            // A call to a compiled function that *returns* an aggregate cannot
            // leave its result in `rax` as a value: it writes through a hidden
            // pointer. Such a call reaching here (in scalar expression position)
            // would be a use of its aggregate result that we do not handle, so it
            // is routed through `lower_aggregate_init` instead. Guard against it.
            if let Some(sig) = ctx.signatures.get(name.as_str())
                && sig.returns_aggregate()
            {
                return Err(format!(
                    "aggregate-returning call `{name}` is only supported in a binding or \
                     return position on the native backend"
                ));
            }
            // If the target is an `extern fn` (a C symbol), marshal it across the
            // Win64 C ABI via `emit_extern_call`: each argument is routed to the
            // register selected by its position and type (integer/pointer →
            // `rcx`/`rdx`/`r8`/`r9`; float → `xmm0..3`, §4.1). This scalar-
            // expression position consumes the return in `rax`, so a float-
            // returning extern here would be a value in `xmm0` we cannot use as an
            // `rax` result — reject it (the type checker never routes a float
            // return into an integer context, so this only guards a
            // miscompile). A narrow integer return is re-normalized in `rax`. A
            // non-extern call keeps the internal i64 convention below.
            if let Some(sig) = ctx.extern_sigs.get(name.as_str()) {
                let sig = *sig;
                match emit_extern_call(ctx, name, sig, args, code)? {
                    Some(FfiScalarClass::Int(Some(fixed))) => {
                        // The Win64 ABI leaves the upper bits of a narrow integer
                        // return undefined, so a returned `i8`/`i16`/`i32`/`u8`/
                        // `u16`/`u32` is re-normalized (sign/zero extended) so
                        // downstream Lullaby code sees the same cell the
                        // interpreters produce.
                        emit_normalize_rax(code, fixed);
                    }
                    // An `i64`/`u64`/`isize`/`usize`/pointer return already fills the
                    // cell; a `void` return leaves no value (the call is a discarded
                    // statement). Nothing to normalize in either case.
                    Some(FfiScalarClass::Int(None)) | None => {}
                    Some(FfiScalarClass::Float(_)) => {
                        return Err(format!(
                            "float-returning extern `{name}` cannot be used in an \
                             integer-scalar context on the native backend"
                        ));
                    }
                }
                return Ok(());
            }
            // A non-extern (compiled/builtin) call: stage every argument onto the
            // machine stack (a scalar value, a float word, or an aggregate-copy
            // pointer), then distribute the first four into the Win64 argument
            // registers and any 5th+ into the outgoing stack-argument area above
            // the callee's shadow space. No hidden return pointer here (this path
            // is scalar-returning).
            emit_native_call_args(ctx, name, args, None, code)?;
            // call rel32 -> relocation against the target symbol.
            code.push(0xE8);
            let site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            ctx.relocations.push(CodeRelocation {
                offset: site as u32,
                symbol: name.clone(),
            });
            Ok(())
        }
        // `s[i]` on a heap string yields the `i`-th Unicode scalar (a `char`), via
        // the UTF-8-aware char-at helper. Guarded on the string operand type so a
        // normal array index still resolves as a stack access below.
        BytecodeExprKind::Index { target, index } if is_string_type(&target.ty) => {
            lower_string_char_at(ctx, target, index, code)
        }
        // `a[i]` on a heap `array<string>` loads the `i`-th shared string pointer
        // from the `[len][cap][slot…]` block, bounds-checked against `len` (trapping
        // with `ud2` on an out-of-range index, mirroring the interpreters' `L0413`).
        BytecodeExprKind::Index { target, index }
            if heap_string_array_element(&target.ty).is_some() =>
        {
            lower_array_string_index(ctx, target, index, code)
        }
        BytecodeExprKind::Field { .. } | BytecodeExprKind::Index { .. } => {
            // A struct-field or array-index read yielding an i64 scalar. Resolve
            // the access to a stack word and load it into rax.
            let place = resolve_read_place(ctx, expr)?;
            emit_load_place(ctx, &place, code)
        }
        // A string literal used as a general VALUE (not just `len`'s argument):
        // materialize its `.rdata` bytes into a fresh heap string record at
        // runtime and leave the record pointer in `rax`. The `.rdata` bytes stay
        // NUL-terminated (shared with the `len("literal")` path); the
        // `__lullaby_str_lit` helper computes the char/byte headers and copies the
        // bytes into the record.
        BytecodeExprKind::String(text) => {
            let symbol = ctx.strings.intern(text);
            // lea rcx, [rip + __str] ; the 4-byte rel32 is a REL32 relocation.
            code.extend_from_slice(&[0x48, 0x8D, 0x0D]);
            let site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            ctx.relocations.push(CodeRelocation {
                offset: site as u32,
                symbol,
            });
            emit_call_symbol(ctx, STR_LIT_SYMBOL, code);
            Ok(())
        }
        BytecodeExprKind::Float(_)
        | BytecodeExprKind::Array(_)
        | BytecodeExprKind::Await { .. }
        // Closures are not in the native scalar subset: a function that
        // constructs or calls one is skipped and runs on the interpreters.
        | BytecodeExprKind::Closure { .. } => {
            Err("expression is not in the native i64-scalar subset".to_string())
        }
    }
}

// -- Internal call argument ABI (registers + stack spill) --------------------
//
// A compiled Lullaby callee receives its first four **effective** arguments in
// the Win64 registers (`rcx`/`rdx`/`r8`/`r9` for integer/pointer/aggregate-copy
// pointers; `xmm0..3` positionally for floats) and its 5th+ arguments on the
// stack, pushed above the callee's 32-byte shadow space. When the callee returns
// an aggregate, its hidden result pointer consumes register 0, shifting the
// visible arguments down by one effective position. `emit_native_call_args`
// stages every visible argument onto the machine stack, then distributes each to
// its register or outgoing stack slot before the `call`.

/// Load the staged word at machine-stack offset `disp` into effective integer
/// register `pos` (`rcx`/`rdx`/`r8`/`r9`).
const GPR_ARG_INDEX: [u8; 4] = [0, 1, 2, 3];

/// Stage a call's arguments and place them into the Win64 argument registers and
/// (for a 5th+ argument) the outgoing stack area, then leave the machine stack as
/// the emitter found it so the `call` sees the reserved outgoing area intact.
///
/// `sret` is the caller-allocated destination slot when the callee returns an
/// aggregate (its address is passed as the hidden first argument, register 0),
/// otherwise `None`. A scalar argument stages its value word; a float argument
/// stages its raw float word; an aggregate argument stages a *pointer* to a fresh
/// caller-owned copy in scratch (value semantics). After staging all `n` words on
/// the stack (argument `i` at `[rsp + 8*(n-1-i)]`), the first four effective
/// positions load into registers and each later position is copied into the
/// outgoing area at `[rsp + 8*n + 32 + 8*(pos-4)]` (which becomes
/// `[rsp' + 32 + 8*(pos-4)]` once the staging words are discarded).
fn emit_native_call_args(
    ctx: &mut NativeCtx,
    callee: &str,
    args: &[BytecodeExpr],
    sret: Option<i32>,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // The callee's parameter layouts (when it is a compiled function) tell us
    // which arguments are aggregates or floats. An `extern`/builtin call has no
    // aggregate/float parameters on this path (guarded elsewhere), so treat a
    // missing signature as all-scalar-integer.
    let param_tys: Vec<Option<NativeType>> = match ctx.signatures.get(callee) {
        Some(sig) => sig.params.iter().map(|t| Some(t.clone())).collect(),
        None => args.iter().map(|_| None).collect(),
    };
    // Fast path: a single scalar integer/pointer argument with no hidden
    // aggregate-return pointer. Staging exists only to keep an already-placed
    // register from being clobbered while a *later* argument is evaluated — with
    // one argument there is nothing to clobber, so evaluate it straight into the
    // first argument register (`rcx`) instead of the stack round-trip.
    let single_agg_or_float = matches!(
        param_tys.first(),
        Some(Some(t)) if t.is_aggregate() || matches!(t, NativeType::F64 | NativeType::F32)
    );
    if sret.is_none() && args.len() == 1 && !single_agg_or_float {
        // `f(reg ± const)` (the recursive `fib(n - 1)` / `fib(n - 2)` idiom):
        // compute the argument with a single `lea rcx, [reg ± imm]`, exactly as C
        // does, instead of `mov rax, reg; add/sub rax, imm; mov rcx, rax`.
        if let Some((reg, disp)) = promoted_reg_plus_const(ctx, &args[0]) {
            emit_lea_rcx_reg_disp(code, reg, disp);
            return Ok(());
        }
        lower_native_expr(ctx, &args[0], code)?; // arg → rax
        code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
        return Ok(());
    }
    // Stage every argument onto the machine stack as one 8-byte word, left to
    // right, so evaluating a later argument cannot clobber an already-placed
    // register. Reset the scratch cursor so each call reuses the shared region.
    let saved_scratch = ctx.scratch_next;
    for (arg, param_ty) in args.iter().zip(param_tys.iter()) {
        match param_ty {
            Some(ty) if ty.is_aggregate() => {
                // Materialize the argument aggregate into scratch, then push its
                // address (the callee copies-in from this snapshot).
                let base = ctx.alloc_scratch(ty.words());
                lower_aggregate_init(ctx, base, ty, arg, code)?;
                emit_lea_rax_slot(code, base); // rax = &scratch copy
                code.push(0x50); // push rax
            }
            Some(NativeType::F64) | Some(NativeType::F32) => {
                // A float argument evaluates into `xmm0`; spill it as one raw word.
                lower_native_float_expr(ctx, arg, code)?;
                code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]); // sub rsp, 8
                code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x04, 0x24]); // movsd [rsp], xmm0
            }
            _ => {
                // A scalar integer/pointer argument: evaluate into rax and push it.
                lower_native_expr(ctx, arg, code)?;
                code.push(0x50); // push rax
            }
        }
    }
    ctx.scratch_next = saved_scratch;

    let n = args.len();
    let hidden = usize::from(sret.is_some());
    // Distribute each staged word to its effective position. Register positions
    // (< 4) load into the GPR/XMM chosen by position and class; stack positions
    // (>= 4) copy into the outgoing area above the shadow space.
    for (i, param_ty) in param_tys.iter().enumerate() {
        let staged_disp = 8 * (n - 1 - i) as i32; // arg i at [rsp + staged_disp]
        let pos = i + hidden;
        let is_float = matches!(param_ty, Some(NativeType::F64) | Some(NativeType::F32));
        if pos < 4 {
            if is_float {
                emit_load_xmm_from_rsp_disp(code, pos as u8, staged_disp);
            } else {
                emit_load_gpr_from_rsp_disp(code, GPR_ARG_INDEX[pos], staged_disp);
            }
        } else {
            // Copy the staged word into the outgoing stack slot. After the staging
            // words are discarded (`add rsp, 8*n`), the slot at
            // `[rsp + 8*n + 32 + 8*(pos-4)]` becomes `[rsp' + 32 + 8*(pos-4)]`,
            // exactly where the callee reads its `(pos-4)`-th stack parameter from
            // `[rbp + 16 + 8*(pos-4)]`.
            let out_disp = 8 * n as i32 + 32 + 8 * (pos as i32 - 4);
            // mov rax, [rsp + staged_disp] ; mov [rsp + out_disp], rax.
            code.extend_from_slice(&[0x48, 0x8B, 0x84, 0x24]); // mov rax, [rsp + disp32]
            code.extend_from_slice(&staged_disp.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x89, 0x84, 0x24]); // mov [rsp + disp32], rax
            code.extend_from_slice(&out_disp.to_le_bytes());
        }
    }
    // The hidden aggregate-return pointer occupies register 0 (`rcx`).
    if let Some(dest_slot) = sret {
        emit_lea_rcx_slot(code, dest_slot);
    }
    // Discard the staging words; the outgoing area and shadow space remain.
    if n > 0 {
        emit_add_rsp(code, 8 * n as i32);
    }
    Ok(())
}

/// The marshalling class of one C-ABI scalar crossing the FFI boundary: an
/// integer/pointer value (Win64 GPR `rcx`/`rdx`/`r8`/`r9`, an optional
/// re-normalization on a narrow return) or a float value (`f64`/`f32` in the SSE
/// registers `xmm0..3`, returned in `xmm0`). Positional routing (§4.1) chooses
/// the register for argument N by N's *position and type*: a float at position N
/// consumes `xmm N`, an integer at position N consumes integer register N, and
/// each position consumes exactly one slot in exactly one sequence.
#[derive(Clone, Copy)]
enum FfiScalarClass {
    /// An integer/pointer scalar; `Some(kind)` needs a narrow-return normalization
    /// in `rax`, `None` already fills the 64-bit cell (`i64`/`u64`/`isize`/`usize`).
    Int(Option<IntKind>),
    /// An `f64`/`f32` float scalar routed through the SSE registers.
    Float(FloatWidth),
}

/// One `extern fn` parameter's marshalling class across the Win64 C ABI: a scalar
/// value (integer/pointer/float, routed by [`FfiScalarClass`]), or a `cstr` — a
/// Lullaby `string` the boundary materializes into a fresh NUL-terminated C buffer
/// (`__lullaby_to_cstr`) whose pointer is then routed like any pointer word.
#[derive(Clone, Copy)]
enum FfiParam {
    Scalar(FfiScalarClass),
    Cstr,
}

impl FfiParam {
    /// A `cstr` argument occupies a pointer word in an integer register; a scalar
    /// float occupies an SSE register. This selects the register class at a
    /// given argument position.
    fn is_float(self) -> bool {
        matches!(self, FfiParam::Scalar(FfiScalarClass::Float(_)))
    }
}

/// Whether a Lullaby type name spells a raw pointer: the modern `ptr<T>` form or
/// the legacy `ptr_T` form that `alloc` produces. A raw pointer is a single
/// machine-address word (a C `T*`) at the FFI boundary.
fn is_raw_pointer_type_name(name: &str) -> bool {
    name.starts_with("ptr<") || name.starts_with("ptr_")
}

/// Classify a Lullaby type name as a marshallable FFI scalar (integer, raw
/// pointer, or float), or `None` for a type outside the scalar marshalling set
/// (`string`/`list`/`map`, non-`repr(C)` structs, callbacks), which demotes the
/// extern caller so it runs on the interpreters. A raw pointer `ptr<T>` marshals
/// to a C `T*`: a 64-bit address passed/returned in a GPR with no narrow-return
/// normalization, i.e. the same class as `i64` (`Int(None)`).
fn ffi_scalar_class(type_name: &str) -> Option<FfiScalarClass> {
    if is_raw_pointer_type_name(type_name) {
        return Some(FfiScalarClass::Int(None));
    }
    if let Some(kind) = ffi_scalar_int_kind(type_name) {
        return Some(FfiScalarClass::Int(kind));
    }
    FloatWidth::from_type_name(type_name).map(FfiScalarClass::Float)
}

/// Marshal and emit a call to an `extern fn` C symbol across the Win64 C ABI.
/// Validates that every parameter and the return type is a marshallable scalar
/// (integer/pointer or `f64`/`f32`); stages each argument's value on the machine
/// stack; loads each argument into the register selected by its **position and
/// type** (integer/pointer → `rcx`/`rdx`/`r8`/`r9` at that position; float →
/// `xmm0..3` at that position, §4.1); then emits the `call rel32` relocation. An
/// integer return is left in `rax` (the caller re-normalizes a narrow width); a
/// float return is left in `xmm0`. Returns the return-value class so the caller
/// can finish the return normalization for its result context.
///
/// There is no fixed argument-count cap: the first four arguments use the Win64
/// argument registers and the 5th+ spill onto the stack above the callee's 32-byte
/// shadow space (the outgoing area the frame reserved). A non-marshallable
/// parameter/return type demotes the caller gracefully.
fn emit_extern_call(
    ctx: &mut NativeCtx,
    name: &str,
    sig: &crate::IrExternSignature,
    args: &[BytecodeExpr],
    code: &mut Vec<u8>,
) -> Result<Option<FfiScalarClass>, String> {
    if args.len() != sig.params.len() {
        return Err(format!(
            "extern `{name}` expects {} argument(s) but got {}",
            sig.params.len(),
            args.len()
        ));
    }
    // Classify each parameter and the return type. Any non-marshallable type
    // demotes the caller to the interpreters (which reject the extern call with
    // `L0423`). A `cstr` parameter materializes a NUL-terminated C buffer from a
    // Lullaby `string`; a raw pointer / scalar routes by its Win64 register class.
    let param_classes: Vec<FfiParam> = sig
        .params
        .iter()
        .map(|param_ty| {
            if param_ty.name == "cstr" {
                return Ok(FfiParam::Cstr);
            }
            ffi_scalar_class(&param_ty.name)
                .map(FfiParam::Scalar)
                .ok_or_else(|| {
                    format!(
                        "extern `{name}` parameter type `{}` is not a native FFI \
                         parameter (aggregates/callbacks are deferred)",
                        param_ty.name
                    )
                })
        })
        .collect::<Result<_, _>>()?;
    // A `cstr` cannot be *returned* (an inbound C string is received as `ptr<byte>`
    // and copied explicitly), so the return type is `void` (no value) or a plain
    // FFI scalar/pointer.
    if sig.return_type.name == "cstr" {
        return Err(format!(
            "extern `{name}` returns `cstr`; an inbound C string must be typed \
             `ptr<byte>` (owned-string conversion is deferred)"
        ));
    }
    let return_class = if sig.return_type.is_void() {
        None
    } else {
        Some(ffi_scalar_class(&sig.return_type.name).ok_or_else(|| {
            format!(
                "extern `{name}` return type `{}` is not a native FFI scalar/pointer \
                 (aggregates/strings are deferred)",
                sig.return_type.name
            )
        })?)
    };

    // Stage each argument onto the machine stack as one 8-byte word, left to
    // right. An integer/pointer argument evaluates into `rax` and is `push`ed; a
    // float argument evaluates into `xmm0` and is spilled into a reserved 8-byte
    // word; a `cstr` argument evaluates its `string` into a record pointer, then
    // `__lullaby_to_cstr` materializes a NUL-terminated buffer whose pointer is
    // pushed. After the loop, argument at position i sits at `[rsp + 8*(n-1-i)]`
    // (the first-pushed argument is deepest). Staging first, then loading
    // registers, avoids one argument's evaluation clobbering an already-loaded
    // register.
    for (arg, class) in args.iter().zip(param_classes.iter()) {
        match class {
            FfiParam::Scalar(FfiScalarClass::Int(_)) => {
                lower_native_expr(ctx, arg, code)?;
                code.push(0x50); // push rax
            }
            FfiParam::Scalar(FfiScalarClass::Float(_)) => {
                lower_native_float_expr(ctx, arg, code)?;
                // sub rsp, 8 ; movsd [rsp], xmm0  (spill one 8-byte float word).
                code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]);
                code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x04, 0x24]);
            }
            FfiParam::Cstr => {
                // Evaluate the `string` argument into a heap record pointer, then
                // materialize a fresh NUL-terminated UTF-8 buffer. The helper only
                // calls the leaf bump allocator, so it tolerates the mid-staging
                // (possibly unaligned) `rsp`; the real C `call` below is realigned.
                lower_native_expr(ctx, arg, code)?; // rax = string record ptr
                code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
                emit_call_symbol(ctx, TO_CSTR_SYMBOL, code); // rax = C buffer ptr
                code.push(0x50); // push rax
            }
        }
    }
    let n = args.len();
    // Distribute each staged word to its Win64 position. Positions 0..4 load into
    // the argument register selected by position and class (GPR for int/pointer/
    // cstr, XMM for float); position 4+ copies into the outgoing stack-argument
    // area above the 32-byte shadow space, exactly where the callee reads it. The
    // staged words are still on the stack here (they are discarded after), so a
    // stack slot sits at `[rsp + 8*n + 32 + 8*(pos-4)]`.
    for (i, class) in param_classes.iter().enumerate() {
        let staged_disp = 8 * (n - 1 - i) as i32;
        if i < 4 {
            if class.is_float() {
                emit_load_xmm_from_rsp_disp(code, i as u8, staged_disp);
            } else {
                emit_load_gpr_from_rsp_disp(code, i as u8, staged_disp);
            }
        } else {
            let out_disp = 8 * n as i32 + 32 + 8 * (i as i32 - 4);
            // mov rax, [rsp + staged_disp] ; mov [rsp + out_disp], rax.
            code.extend_from_slice(&[0x48, 0x8B, 0x84, 0x24]); // mov rax, [rsp + disp32]
            code.extend_from_slice(&staged_disp.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x89, 0x84, 0x24]); // mov [rsp + disp32], rax
            code.extend_from_slice(&out_disp.to_le_bytes());
        }
    }
    // Discard the staged words: add rsp, 8*n. `rsp` returns to the frame's
    // call-ready position (16-byte aligned, 32-byte shadow + outgoing area below).
    if n > 0 {
        emit_add_rsp(code, 8 * n as i32);
    }
    // call rel32 -> relocation against the (undefined external) C symbol.
    code.push(0xE8);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: site as u32,
        symbol: name.to_string(),
    });
    Ok(return_class)
}

/// Emit `add rsp, imm` (imm > 0). Uses the imm8 form when it fits, else imm32.
fn emit_add_rsp(code: &mut Vec<u8>, amount: i32) {
    if (0..=127).contains(&amount) {
        code.extend_from_slice(&[0x48, 0x83, 0xC4, amount as u8]);
    } else {
        code.extend_from_slice(&[0x48, 0x81, 0xC4]);
        code.extend_from_slice(&amount.to_le_bytes());
    }
}

/// Lower a call to a compiled function that returns an aggregate, writing the
/// result into the words at `dest_slot`. The caller-allocated destination address
/// is passed as the hidden first integer argument (`rcx`); the visible arguments
/// (scalar values / aggregate-copy pointers) follow in `rdx`/`r8`/`r9`. The callee
/// writes the result through the hidden pointer, so after the call `dest_slot`
/// holds the returned aggregate.
fn lower_aggregate_returning_call(
    ctx: &mut NativeCtx,
    dest_slot: i32,
    ty: &NativeType,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let BytecodeExprKind::Call { name, args } = &value.kind else {
        return Err("expected a call expression".to_string());
    };
    let sig = ctx
        .signatures
        .get(name.as_str())
        .ok_or_else(|| format!("call target `{name}` has no native signature"))?;
    if !sig.returns_aggregate() {
        return Err(format!("call `{name}` does not return an aggregate"));
    }
    // The callee writes `sig.ret.words()` words into the destination; the caller's
    // destination must reserve at least that many. (An enum `match` scrutinee
    // constructs an equivalent layout with a synthetic name, so compare by word
    // count rather than exact type equality.)
    if sig.ret.words() != ty.words() {
        return Err(format!(
            "call `{name}` return layout ({} words) does not match the destination ({} words)",
            sig.ret.words(),
            ty.words()
        ));
    }
    // Stage the visible arguments and distribute them past the hidden return
    // pointer: the pointer consumes register 0 (`rcx`), the visible args follow in
    // `rdx`/`r8`/`r9` and then the outgoing stack area for a 5th+ effective arg.
    emit_native_call_args(ctx, name, args, Some(dest_slot), code)?;
    // call rel32 -> relocation against the callee.
    code.push(0xE8);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: site as u32,
        symbol: name.clone(),
    });
    // The callee wrote the result words into `[rcx]` == `dest_slot`; nothing more
    // to copy.
    Ok(())
}

/// Lower a `return <aggregate>` (or an aggregate tail expression): materialize the
/// value into scratch, then copy its words through the hidden return pointer and
/// leave that pointer in `rax` (the by-pointer return convention). A direct
/// aggregate-returning call is special-cased to write straight into the hidden
/// pointer's destination (no scratch round-trip).
fn lower_aggregate_return(
    ctx: &mut NativeCtx,
    expr: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let sret_slot = ctx
        .sret_slot
        .ok_or("aggregate return without a hidden result pointer")?;
    let ty = ctx.return_ty.clone();
    // Materialize into a scratch region, then copy the words to `[sret]`. Using
    // scratch (rather than writing straight through the pointer) keeps the
    // materialization code — which addresses `[rbp - slot]` frame slots — reusable
    // for every initializer shape (constructor, literal, local copy, call).
    let saved_scratch = ctx.scratch_next;
    let base = ctx.alloc_scratch(ty.words());
    lower_aggregate_init(ctx, base, &ty, expr, code)?;
    // rax = hidden result pointer (the caller-allocated destination, addressing
    // word 0). Aggregate words descend in memory, so word k is written at
    // `[rax - 8*k]`, matching the destination's `[rbp - (slot + 8*k)]` layout.
    emit_mov_rax_from_slot(code, sret_slot);
    // Copy each word: rcx = [rbp - (base + 8k)]; [rax - 8k] = rcx.
    for word in 0..ty.words() as i32 {
        emit_mov_rcx_from_slot(code, base + word * 8);
        emit_mov_rax_disp_from_rcx(code, -word * 8);
    }
    ctx.scratch_next = saved_scratch;
    // Per the ABI, an aggregate return leaves the result pointer in rax.
    emit_mov_rax_from_slot(code, sret_slot);
    Ok(())
}

/// `lea rax, [rbp - slot]` — the effective address of a frame slot.
fn emit_lea_rax_slot(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0x48, 0x8D, 0x85]); // lea rax, [rbp + disp32]
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// `lea rcx, [rbp - slot]` — the effective address of a frame slot.
fn emit_lea_rcx_slot(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0x48, 0x8D, 0x8D]); // lea rcx, [rbp + disp32]
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// `mov rax, [rbp - slot]`.
fn emit_mov_rax_from_slot(code: &mut Vec<u8>, slot: i32) {
    load_local(code, slot);
}

/// Lower a binary expression. `and`/`or` short-circuit; other operators evaluate
/// left (pushed), right (in rax), then combine popping the left back.
fn lower_native_binary(
    ctx: &mut NativeCtx,
    left: &BytecodeExpr,
    op: BinaryOp,
    right: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // String ordering (`< <= > >=`) is lexicographic by content; the scalar
    // backend would compare heap pointers, so defer the whole function to the
    // interpreters (which compare correctly). Concatenation `+` and equality are
    // handled by their own paths.
    if matches!(
        op,
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual
    ) && (left.ty.name == "string" || right.ty.name == "string")
    {
        return Err(
            "string ordering comparison is not supported on the native backend".to_string(),
        );
    }
    match op {
        BinaryOp::And => {
            // rax = left ? (right != 0 ? 1 : 0) : 0
            lower_native_expr(ctx, left, code)?;
            code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            code.extend_from_slice(&[0x0F, 0x84]); // jz false (patched)
            let false_site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            lower_native_expr(ctx, right, code)?;
            normalize_bool(code); // rax = (rax != 0) ? 1 : 0
            code.push(0xE9); // jmp done
            let done_site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            patch_rel32(code, false_site);
            code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax
            patch_rel32(code, done_site);
            Ok(())
        }
        BinaryOp::Or => {
            // rax = left ? 1 : (right != 0 ? 1 : 0)
            lower_native_expr(ctx, left, code)?;
            code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            code.extend_from_slice(&[0x0F, 0x85]); // jnz true (patched)
            let true_site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            lower_native_expr(ctx, right, code)?;
            normalize_bool(code);
            code.push(0xE9); // jmp done
            let done_site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            patch_rel32(code, true_site);
            emit_mov_rax_imm(code, 1);
            patch_rel32(code, done_site);
            Ok(())
        }
        // String concatenation: `a + b` on two strings allocates a fresh record
        // and byte-copies both operands' UTF-8 ranges. Detected by the result type
        // being `string` (the type checker only allows `+` between two strings).
        BinaryOp::Add if is_string_type(&left.ty) && is_string_type(&right.ty) => {
            lower_string_concat(ctx, left, right, code)
        }
        _ => {
            // A float comparison (`f64`/`f32` operands, ordered relational or
            // equality op) produces an `i64`/`bool` result in rax via SSE compare
            // + set. Float *arithmetic* never reaches here in an i64-return
            // function (it flows through the float-expr lowerer), so only
            // comparisons need handling on this i64-producing path.
            //
            // Detect float-ness from the operands' structure rather than the
            // comparison node's own `ty`: the IR annotates an arithmetic float
            // node (e.g. `a + b`) with `i64`, so the leaf-derived width is the
            // reliable signal (float literals, float locals, and `to_f32`/`to_f64`
            // conversions all carry a correct concrete float type).
            if let Some(width) =
                float_width_of_expr(ctx, left).or_else(|| float_width_of_expr(ctx, right))
            {
                return lower_native_float_compare(ctx, left, op, right, width, code);
            }
            // Constant right operand on a plain `i64` add/sub/mul: fold into an
            // immediate (`add`/`sub rax, imm32`, or `imul rax, rax, imm32`),
            // skipping the operand-stack shuffle. x86 `add`/`sub`/`imul` keep the
            // low 64 bits, matching the interpreters' wrapping arithmetic. Only for
            // plain `i64` (fixed-width kinds need width re-normalization) with an
            // i32-range immediate; anything else uses the general path below.
            if fixed_int_kind(left.ty.name.as_str()).is_none()
                && let BytecodeExprKind::Integer(rhs) = &right.kind
                && let Ok(imm) = i32::try_from(*rhs)
            {
                let opcode: Option<&[u8]> = match op {
                    BinaryOp::Add => Some(&[0x48, 0x05]),      // add rax, imm32
                    BinaryOp::Subtract => Some(&[0x48, 0x2D]), // sub rax, imm32
                    BinaryOp::Multiply => Some(&[0x48, 0x69, 0xC0]), // imul rax, rax, imm32
                    _ => None,
                };
                if let Some(prefix) = opcode {
                    lower_native_expr(ctx, left, code)?; // left in rax
                    code.extend_from_slice(prefix);
                    code.extend_from_slice(&imm.to_le_bytes());
                    return Ok(());
                }
            }
            lower_native_expr(ctx, left, code)?;
            code.push(0x50); // push rax (left)
            lower_native_expr(ctx, right, code)?; // right in rax
            // A fixed-width operand kind (both operands share it; the type checker
            // forbids mixing widths) selects width- and signedness-correct codegen
            // that re-normalizes the result. Plain `i64` uses the full-width path.
            match fixed_int_kind(left.ty.name.as_str()) {
                Some(kind) => emit_fixed_binop_from_stack(code, op, kind),
                None => emit_i64_binop_from_stack(code, op),
            }
        }
    }
}

// -- Floating-point lowering (SSE scalar, XMM0/XMM1) -------------------------
//
// Float values live in XMM registers: `f64` as a `double`, `f32` as a `single`
// kept rounded to single precision after every operation (matching the
// interpreter's real `f32` storage). The float lowerer is a small stack machine
// over `xmm0`, spilling the left operand of a binary op to the machine stack
// (`sub rsp,16; movsd [rsp],xmm0`) so `xmm0` is free to evaluate the right.
//
// Float literals are materialized without any `.rdata` constant: the IEEE-754
// bit pattern is loaded into a GPR (`mov rax, imm64` for f64, `mov eax, imm32`
// for f32) and moved into an XMM register (`movq`/`movd`). This keeps every
// float function self-contained (no new relocations or data symbols).

/// Lower a float-valued expression, leaving its value in `xmm0` and returning
/// the [`FloatWidth`] of the result (`f64` as a double, `f32` rounded to single).
/// Handles float literals, float locals, the `to_f32`/`to_f64` conversions, and
/// `f64`/`f32` arithmetic (`+ - * /`). Anything else (e.g. a float-returning
/// call, a math builtin) is rejected so the enclosing function skips gracefully.
fn lower_native_float_expr(
    ctx: &mut NativeCtx,
    expr: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<FloatWidth, String> {
    match &expr.kind {
        BytecodeExprKind::Float(value) => {
            // A bare float literal's static type says whether it is f64 or f32.
            // The type checker pins every float literal to a concrete type.
            let width = FloatWidth::from_type_name(expr.ty.name.as_str())
                .ok_or_else(|| format!("float literal has non-float type `{}`", expr.ty.name))?;
            emit_float_immediate(code, *value, width);
            Ok(width)
        }
        BytecodeExprKind::Variable(name) => {
            let local = ctx.local(name)?;
            let width = match local.ty {
                NativeType::F64 => FloatWidth::F64,
                NativeType::F32 => FloatWidth::F32,
                _ => return Err(format!("`{name}` is not a float local")),
            };
            let slot = local.slot;
            load_float_local(code, slot, width);
            Ok(width)
        }
        BytecodeExprKind::Call { name, args } => {
            // `to_f32(x f64) -> f32`: evaluate the f64 argument, then round it to
            // single precision with `cvtsd2ss`.
            if name == "to_f32" {
                if args.len() != 1 {
                    return Err("`to_f32` takes exactly one argument".to_string());
                }
                let arg_width = lower_native_float_expr(ctx, &args[0], code)?;
                if arg_width != FloatWidth::F64 {
                    return Err("`to_f32` expects an f64 argument".to_string());
                }
                // cvtsd2ss xmm0, xmm0  (F2 0F 5A C0)
                code.extend_from_slice(&[0xF2, 0x0F, 0x5A, 0xC0]);
                return Ok(FloatWidth::F32);
            }
            // `to_f64(x f32) -> f64`: evaluate the f32 argument, then widen it with
            // `cvtss2sd` (exact).
            if name == "to_f64" {
                if args.len() != 1 {
                    return Err("`to_f64` takes exactly one argument".to_string());
                }
                let arg_width = lower_native_float_expr(ctx, &args[0], code)?;
                if arg_width != FloatWidth::F32 {
                    return Err("`to_f64` expects an f32 argument".to_string());
                }
                // cvtss2sd xmm0, xmm0  (F3 0F 5A C0)
                code.extend_from_slice(&[0xF3, 0x0F, 0x5A, 0xC0]);
                return Ok(FloatWidth::F64);
            }
            // `get(l, i)` on a float-element list: load the raw 8-byte element
            // word into `rax`, then move its bits into `xmm0` at the element's
            // width (the low four bytes of the word for f32).
            if name == LIST_GET_BUILTIN
                && args.len() == 2
                && let Some(elem) = supported_list_element(&args[0].ty)
                && let Some(width) = FloatWidth::from_type_name(&elem.name)
            {
                lower_list_get(ctx, &args[0], &args[1], code)?; // element word -> rax
                emit_movq_xmm0_from_rax(code, width);
                return Ok(width);
            }
            // A float-returning `extern fn` C call: marshal the arguments across
            // the Win64 C ABI (integer/pointer → GPRs, float → `xmm0..3`, §4.1)
            // and read the `f64`/`f32` return from `xmm0`.
            if let Some(sig) = ctx.extern_sigs.get(name.as_str()) {
                let sig = *sig;
                return match emit_extern_call(ctx, name, sig, args, code)? {
                    Some(FfiScalarClass::Float(width)) => Ok(width),
                    Some(FfiScalarClass::Int(_)) | None => Err(format!(
                        "extern `{name}` does not return a float, so it \
                         cannot be used in a float context"
                    )),
                };
            }
            Err(format!(
                "float call `{name}` is not in the native subset (non-extern float-returning functions and math builtins are deferred)"
            ))
        }
        BytecodeExprKind::Binary { left, op, right } => {
            // Derive the width from the operands' structure (an arithmetic float
            // node is annotated `i64` in the IR, so its own `ty` is unreliable).
            let width = float_width_of_expr(ctx, left)
                .or_else(|| float_width_of_expr(ctx, right))
                .ok_or_else(|| "float binary op on non-float operands".to_string())?;
            match op {
                BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide => {
                    // Evaluate left into xmm0 and spill it; evaluate right into
                    // xmm0; restore left into xmm1; apply left <op> right.
                    let left_width = lower_native_float_expr(ctx, left, code)?;
                    push_xmm0(code); // save left
                    let right_width = lower_native_float_expr(ctx, right, code)?;
                    debug_assert_eq!(left_width, width);
                    debug_assert_eq!(right_width, width);
                    // xmm1 = right, xmm0 = left.
                    move_xmm0_to_xmm1(code); // xmm1 = right
                    pop_xmm0(code); // xmm0 = left
                    emit_float_arith(code, *op, width);
                    Ok(width)
                }
                _ => Err(
                    "float comparison does not produce a float value (handled on the i64 path)"
                        .to_string(),
                ),
            }
        }
        // Float arithmetic negation (`-x`): IEEE-754 sign-bit flip, matching the
        // interpreters' `-f`. Move the value through a GPR, XOR the sign bit, move
        // it back to xmm0. (`0 - x` would mishandle `-0.0`/NaN signs.)
        BytecodeExprKind::Unary {
            op: lullaby_parser::UnaryOp::Negate,
            expr: inner,
        } => {
            let width = lower_native_float_expr(ctx, inner, code)?;
            emit_movq_rax_from_xmm0(code, width); // rax = bits
            match width {
                FloatWidth::F64 => {
                    // mov rcx, 0x8000000000000000 ; xor rax, rcx
                    code.extend_from_slice(&[0x48, 0xB9]);
                    code.extend_from_slice(&0x8000_0000_0000_0000u64.to_le_bytes());
                    code.extend_from_slice(&[0x48, 0x31, 0xC8]);
                }
                FloatWidth::F32 => {
                    // xor eax, 0x80000000
                    code.push(0x35);
                    code.extend_from_slice(&0x8000_0000u32.to_le_bytes());
                }
            }
            emit_movq_xmm0_from_rax(code, width); // xmm0 = negated bits
            Ok(width)
        }
        // A float array element / float struct field read: `a[i]` / `s.f` where the
        // element is f64/f32. Resolve the place (constant or bounds-checked dynamic
        // address) and load it into xmm0 with movsd/movss.
        BytecodeExprKind::Index { .. } | BytecodeExprKind::Field { .. } => {
            let (place, elem_ty) = resolve_read_place_typed(ctx, expr)?;
            let width = match elem_ty {
                NativeType::F64 => FloatWidth::F64,
                NativeType::F32 => FloatWidth::F32,
                _ => return Err("float access resolved to a non-float element".to_string()),
            };
            match place {
                ScalarPlace::Const { slot } => load_float_local(code, slot, width),
                ScalarPlace::Dynamic { .. } => {
                    emit_dynamic_addr_into_rcx(ctx, &place, code)?; // rcx = &elem (bounds-checked)
                    load_float_from_rcx(code, width);
                }
            }
            Ok(width)
        }
        _ => Err("expression is not in the native float subset".to_string()),
    }
}

/// Emit `left <op> right` where `left` is in `xmm0` and `right` is in `xmm1`,
/// leaving the result in `xmm0`. `op` is one of `+ - * /`. Uses the double
/// (`*sd`) or single (`*ss`) opcode family per `width`; an `*ss` op inherently
/// rounds its result to single precision, matching the interpreter's f32 store.
fn emit_float_arith(code: &mut Vec<u8>, op: BinaryOp, width: FloatWidth) {
    // Opcode second byte selects add/mul/sub/div: 58/59/5C/5E.
    let arith = match op {
        BinaryOp::Add => 0x58,
        BinaryOp::Subtract => 0x5C,
        BinaryOp::Multiply => 0x59,
        BinaryOp::Divide => 0x5E,
        _ => unreachable!("emit_float_arith only handles + - * /"),
    };
    // Prefix: F2 for scalar-double (*sd), F3 for scalar-single (*ss).
    let prefix = match width {
        FloatWidth::F64 => 0xF2,
        FloatWidth::F32 => 0xF3,
    };
    // <op>s{d,s} xmm0, xmm1  ->  prefix 0F <arith> C1
    code.extend_from_slice(&[prefix, 0x0F, arith, 0xC1]);
}

/// Lower a float comparison (`< <= > >= == !=`) whose operands are `f64`/`f32`,
/// leaving a canonical `0`/`1` in `rax`. Uses ordered SSE compares (`ucomisd`/
/// `ucomiss`) with the unordered-aware condition codes so a NaN operand yields
/// exactly the interpreter's result: every relational compare is false on NaN,
/// `==` is false on NaN, and `!=` is true on NaN (Rust/IEEE-754 semantics).
fn lower_native_float_compare(
    ctx: &mut NativeCtx,
    left: &BytecodeExpr,
    op: BinaryOp,
    right: &BytecodeExpr,
    width: FloatWidth,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // Evaluate left into xmm0, spill it; evaluate right into xmm0, move to xmm1;
    // restore left into xmm0. Result: xmm0 = left, xmm1 = right.
    lower_native_float_expr(ctx, left, code)?;
    push_xmm0(code);
    lower_native_float_expr(ctx, right, code)?;
    move_xmm0_to_xmm1(code); // xmm1 = right
    pop_xmm0(code); // xmm0 = left

    // `ucomis{d,s}` sets CF/ZF/PF as an unsigned-style compare of xmm0 vs xmm1:
    //   xmm0 <  xmm1 -> CF=1, ZF=0
    //   xmm0 == xmm1 -> CF=0, ZF=1
    //   xmm0 >  xmm1 -> CF=0, ZF=0
    //   unordered    -> CF=1, ZF=1, PF=1
    // `seta` (CF=0 & ZF=0) is strict-greater-ordered; `setae` (CF=0) is
    // greater-or-equal-ordered — both false when unordered. So we realize `<`
    // and `<=` by swapping the compare operands (compare right vs left).
    let cmp_prefixed = |code: &mut Vec<u8>, swap: bool| {
        // ucomis{d,s} first, second.  prefix(0x66 for sd, none for ss) 0F 2E /r
        // For F64 use `ucomisd` (66 0F 2E), for F32 `ucomiss` (0F 2E).
        let (a, b) = if swap { (1u8, 0u8) } else { (0u8, 1u8) }; // xmm regs
        let modrm = 0xC0 | (a << 3) | b; // ucomis <xmm a>, <xmm b>
        match width {
            FloatWidth::F64 => code.extend_from_slice(&[0x66, 0x0F, 0x2E, modrm]),
            FloatWidth::F32 => code.extend_from_slice(&[0x0F, 0x2E, modrm]),
        }
    };

    match op {
        BinaryOp::Greater => {
            cmp_prefixed(code, false); // ucomis xmm0, xmm1
            code.extend_from_slice(&[0x0F, 0x97, 0xC0]); // seta al
            movzx_al_to_rax(code);
        }
        BinaryOp::GreaterEqual => {
            cmp_prefixed(code, false);
            code.extend_from_slice(&[0x0F, 0x93, 0xC0]); // setae al
            movzx_al_to_rax(code);
        }
        BinaryOp::Less => {
            // left < right  <=>  right > left. Compare xmm1 vs xmm0.
            cmp_prefixed(code, true); // ucomis xmm1, xmm0
            code.extend_from_slice(&[0x0F, 0x97, 0xC0]); // seta al
            movzx_al_to_rax(code);
        }
        BinaryOp::LessEqual => {
            cmp_prefixed(code, true); // ucomis xmm1, xmm0
            code.extend_from_slice(&[0x0F, 0x93, 0xC0]); // setae al
            movzx_al_to_rax(code);
        }
        BinaryOp::Equal => {
            // Ordered equality: ZF=1 (equal) AND not unordered (PF=0).
            cmp_prefixed(code, false); // ucomis xmm0, xmm1
            code.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
            code.extend_from_slice(&[0x0F, 0x9B, 0xC1]); // setnp cl
            code.extend_from_slice(&[0x20, 0xC8]); // and al, cl
            movzx_al_to_rax(code);
        }
        BinaryOp::NotEqual => {
            // Inequality including unordered: ZF=0 (not equal) OR unordered (PF=1).
            cmp_prefixed(code, false); // ucomis xmm0, xmm1
            code.extend_from_slice(&[0x0F, 0x95, 0xC0]); // setne al
            code.extend_from_slice(&[0x0F, 0x9A, 0xC1]); // setp cl
            code.extend_from_slice(&[0x08, 0xC8]); // or al, cl
            movzx_al_to_rax(code);
        }
        _ => return Err("unsupported float comparison operator".to_string()),
    }
    Ok(())
}

/// Determine the [`FloatWidth`] of `expr` if it is a float value, using leaf
/// types that the IR annotates correctly (float literals, float locals, and the
/// `to_f32`/`to_f64` conversions) and recursing through float arithmetic. Returns
/// `None` for a non-float expression. This is more reliable than reading a
/// `Binary` node's own `ty`, which the IR annotates `i64` for float arithmetic.
fn float_width_of_expr(ctx: &NativeCtx, expr: &BytecodeExpr) -> Option<FloatWidth> {
    match &expr.kind {
        BytecodeExprKind::Float(_) => FloatWidth::from_type_name(expr.ty.name.as_str()),
        BytecodeExprKind::Variable(name) => match ctx.locals.get(name)?.ty {
            NativeType::F64 => Some(FloatWidth::F64),
            NativeType::F32 => Some(FloatWidth::F32),
            _ => None,
        },
        BytecodeExprKind::Call { name, .. } => match name.as_str() {
            "to_f32" => Some(FloatWidth::F32),
            "to_f64" => Some(FloatWidth::F64),
            _ => None,
        },
        // Float arithmetic propagates its operands' width; a comparison yields a
        // bool (not a float), so those and all other ops report `None`.
        BytecodeExprKind::Binary {
            left,
            op: BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide,
            right,
        } => float_width_of_expr(ctx, left).or_else(|| float_width_of_expr(ctx, right)),
        // Unary negation of a float operand is a float of the same width, so it
        // must route to the float path (a sign-bit flip), not the integer `neg`.
        BytecodeExprKind::Unary {
            op: lullaby_parser::UnaryOp::Negate,
            expr: inner,
        } => float_width_of_expr(ctx, inner),
        // A float array element / float struct field read (`a[i]`, `s.f`): resolve
        // the place (read-only) and report the element width, so `a[i] + x` and
        // `a[i] < x` route to the float lowerer / float comparator.
        BytecodeExprKind::Index { .. } | BytecodeExprKind::Field { .. } => {
            match resolve_read_place_typed(ctx, expr) {
                Ok((_, NativeType::F64)) => Some(FloatWidth::F64),
                Ok((_, NativeType::F32)) => Some(FloatWidth::F32),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `movzx rax, al` — zero-extend the boolean in `al` into the full `rax`.
fn movzx_al_to_rax(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]);
}

/// Materialize a float immediate into `xmm0`: the IEEE-754 bit pattern is loaded
/// into a GPR and moved into `xmm0` (`movq` for f64, `movd` for f32). The f32
/// path rounds `value` to `f32` first so the stored bits match the interpreter.
fn emit_float_immediate(code: &mut Vec<u8>, value: f64, width: FloatWidth) {
    match width {
        FloatWidth::F64 => {
            emit_mov_rax_imm(code, value.to_bits() as i64);
            // movq xmm0, rax  (66 48 0F 6E C0)
            code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
        }
        FloatWidth::F32 => {
            let bits = (value as f32).to_bits();
            // mov eax, imm32  (B8 imm32) — zero-extends into rax.
            code.push(0xB8);
            code.extend_from_slice(&bits.to_le_bytes());
            // movd xmm0, eax  (66 0F 6E C0)
            code.extend_from_slice(&[0x66, 0x0F, 0x6E, 0xC0]);
        }
    }
}

/// `movs{d,s} xmm0, [rbp - slot]` — load a float local into `xmm0`.
fn load_float_local(code: &mut Vec<u8>, slot: i32, width: FloatWidth) {
    // movsd: F2 0F 10 /r ; movss: F3 0F 10 /r. ModRM 0x85 = [rbp + disp32], reg 0.
    let prefix = match width {
        FloatWidth::F64 => 0xF2,
        FloatWidth::F32 => 0xF3,
    };
    code.extend_from_slice(&[prefix, 0x0F, 0x10, 0x85]);
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// `movsd xmm0, [rcx]` (F64) / `movss xmm0, [rcx]` (F32) — load a float array
/// element from its computed address in `rcx`. ModRM 0x01 = `[rcx]`, reg 0.
fn load_float_from_rcx(code: &mut Vec<u8>, width: FloatWidth) {
    let prefix = match width {
        FloatWidth::F64 => 0xF2,
        FloatWidth::F32 => 0xF3,
    };
    code.extend_from_slice(&[prefix, 0x0F, 0x10, 0x01]);
}

/// `movsd [rcx], xmm0` (F64) / `movss [rcx], xmm0` (F32) — store xmm0 to a float
/// array element at its computed address in `rcx`. ModRM 0x01 = `[rcx]`, reg 0.
fn store_float_from_rcx(code: &mut Vec<u8>, width: FloatWidth) {
    let prefix = match width {
        FloatWidth::F64 => 0xF2,
        FloatWidth::F32 => 0xF3,
    };
    code.extend_from_slice(&[prefix, 0x0F, 0x11, 0x01]);
}

/// `movs{d,s} [rbp - slot], xmm0` — store `xmm0` into a float local.
fn store_float_local(code: &mut Vec<u8>, slot: i32, width: FloatWidth) {
    // movsd: F2 0F 11 /r ; movss: F3 0F 11 /r.
    let prefix = match width {
        FloatWidth::F64 => 0xF2,
        FloatWidth::F32 => 0xF3,
    };
    code.extend_from_slice(&[prefix, 0x0F, 0x11, 0x85]);
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// Spill `xmm0` onto the machine stack (16 bytes, keeping 16-byte rsp alignment).
/// Paired with [`pop_xmm0`]/[`pop_xmm1`]. The full 8-byte `movsd` store preserves
/// an f32's low bits too, so one spill primitive serves both widths.
fn push_xmm0(code: &mut Vec<u8>) {
    // sub rsp, 16
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x10]);
    // movsd [rsp], xmm0  (F2 0F 11 04 24)
    code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x04, 0x24]);
}

/// Restore a spilled float from the machine stack into `xmm0`.
fn pop_xmm0(code: &mut Vec<u8>) {
    // movsd xmm0, [rsp]  (F2 0F 10 04 24)
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x04, 0x24]);
    // add rsp, 16
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x10]);
}

/// Restore a spilled float from the machine stack into `xmm1`.
fn pop_xmm1(code: &mut Vec<u8>) {
    // movsd xmm1, [rsp]  (F2 0F 10 0C 24)
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x0C, 0x24]);
    // add rsp, 16
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x10]);
}

/// `movsd xmm1, xmm0` — copy `xmm0` into `xmm1` (full 8-byte move; preserves an
/// f32's low bits as well).
fn move_xmm0_to_xmm1(code: &mut Vec<u8>) {
    // movsd xmm1, xmm0  (F2 0F 10 C8)
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0xC8]);
}

/// `movs{d,s} [rbp - slot], xmm{n}` — store one of the low four XMM registers
/// (`xmm0..3`, the Win64 SSE argument registers) into a frame slot. Used by the
/// prologue to spill a float parameter that arrived in its positional XMM
/// register. `movsd` (`F2`) stores an f64 word; `movss` (`F3`) an f32 word.
fn emit_store_xmm_to_slot(code: &mut Vec<u8>, xmm: u8, slot: i32, width: FloatWidth) {
    debug_assert!(xmm < 4, "only xmm0..3 are Win64 argument registers");
    let prefix = match width {
        FloatWidth::F64 => 0xF2,
        FloatWidth::F32 => 0xF3,
    };
    // ModRM 0x85 = [rbp + disp32], reg field selects the XMM source.
    let modrm = 0x85 | (xmm << 3);
    code.extend_from_slice(&[prefix, 0x0F, 0x11, modrm]);
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// `movsd xmm{n}, [rsp + disp]` — load a raw 8-byte word from a stack offset into
/// one of the low four XMM registers (`xmm0..3`). Used to move a staged float
/// argument into its positional SSE argument register before an extern call. A
/// full 8-byte `movsd` load preserves an f32's low four bytes too, so one loader
/// serves both widths.
fn emit_load_xmm_from_rsp_disp(code: &mut Vec<u8>, xmm: u8, disp: i32) {
    debug_assert!(xmm < 4, "only xmm0..3 are Win64 argument registers");
    // movsd xmm{n}, [rsp + disp8/disp32]. Base rsp needs a SIB byte (0x24).
    // ModRM.reg = xmm; ModRM.rm = 100b (SIB). disp8 when it fits, else disp32.
    code.extend_from_slice(&[0xF2, 0x0F, 0x10]);
    emit_rsp_mem_operand(code, xmm, disp);
}

/// `mov reg64, [rsp + disp]` — load a raw 8-byte word from a stack offset into one
/// of the first four Win64 integer argument registers (`rcx`/`rdx`/`r8`/`r9`, by
/// index). Used to move a staged integer argument into its positional GPR before
/// an extern call.
fn emit_load_gpr_from_rsp_disp(code: &mut Vec<u8>, index: u8, disp: i32) {
    // rcx/rdx are in the base encoding (REX.W); r8/r9 need REX.B. Reg field:
    // rcx=1, rdx=2, r8/r9=0/1 with REX.B.
    let (rex, reg): (u8, u8) = match index {
        0 => (0x48, 1), // rcx
        1 => (0x48, 2), // rdx
        2 => (0x4C, 0), // r8
        3 => (0x4C, 1), // r9
        _ => unreachable!("only four Win64 integer argument registers"),
    };
    code.push(rex);
    code.push(0x8B); // mov r64, r/m64
    emit_rsp_mem_operand(code, reg, disp);
}

/// Emit the ModRM+SIB(+disp) bytes for an `[rsp + disp]` memory operand with the
/// given ModRM.reg field. `rsp` as a base always requires the SIB byte `0x24`
/// (base=rsp, index=none). A zero displacement still needs an explicit `disp8`
/// because `[rsp]` with mod=00 is the SIB form without a displacement — encode
/// `disp8` for values in `i8` range, otherwise `disp32`.
fn emit_rsp_mem_operand(code: &mut Vec<u8>, reg: u8, disp: i32) {
    if let Ok(d) = i8::try_from(disp) {
        // mod=01 (disp8), rm=100 (SIB), SIB=0x24 (base=rsp, no index).
        code.push(0x40 | (reg << 3) | 0x04);
        code.push(0x24);
        code.push(d as u8);
    } else {
        // mod=10 (disp32), rm=100 (SIB), SIB=0x24.
        code.push(0x80 | (reg << 3) | 0x04);
        code.push(0x24);
        code.extend_from_slice(&disp.to_le_bytes());
    }
}

/// Combine a fixed-width binary op whose left operand is on the stack and whose
/// right operand is in `rax`, leaving the result (a normalized cell for
/// arithmetic/bitwise/shift, a canonical `0`/`1` for comparisons) in `rax`. This
/// mirrors the interpreter free functions exactly: arithmetic wraps then
/// re-normalizes (`Value::int`), division and comparison are signedness-aware
/// (`int_div`/`int_cmp`), and shifts mask the count to the width and honor
/// signedness (`int_shl`/`int_shr`).
fn emit_fixed_binop_from_stack(
    code: &mut Vec<u8>,
    op: BinaryOp,
    kind: IntKind,
) -> Result<(), String> {
    match op {
        BinaryOp::Add => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x01, 0xC8]); // add rax, rcx
            emit_normalize_rax(code, kind);
        }
        BinaryOp::Subtract => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
            emit_normalize_rax(code, kind);
        }
        BinaryOp::Multiply => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]); // imul rax, rcx
            emit_normalize_rax(code, kind);
        }
        BinaryOp::Divide => {
            // left / right, where left is the dividend. Divide on the full 64-bit
            // cell (signedness-correct because signed cells are sign-extended and
            // unsigned cells zero-extended), then re-normalize the quotient.
            code.push(0x59); // pop rcx (left = dividend)
            code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (divisor)
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx (dividend)
            if kind.is_unsigned() {
                code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx
                code.extend_from_slice(&[0x49, 0xF7, 0xF0]); // div r8
            } else {
                emit_signed_idiv_r8(code); // guarded against i64::MIN / -1 overflow
            }
            emit_normalize_rax(code, kind);
        }
        BinaryOp::Remainder => {
            // left % right: the same div/idiv leaves the remainder in rdx (rather
            // than the quotient in rax). Move it into rax and re-normalize.
            code.push(0x59); // pop rcx (left = dividend)
            code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (divisor)
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx (dividend)
            if kind.is_unsigned() {
                code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx
                code.extend_from_slice(&[0x49, 0xF7, 0xF0]); // div r8
            } else {
                emit_signed_irem_r8(code); // guarded so `x % -1 == 0` (rdx = 0)
            }
            code.extend_from_slice(&[0x48, 0x89, 0xD0]); // mov rax, rdx (remainder)
            emit_normalize_rax(code, kind);
        }
        BinaryOp::Equal | BinaryOp::NotEqual => {
            // Equality is width-agnostic on the normalized cells.
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
            let set_opcode = if matches!(op, BinaryOp::Equal) {
                0x94 // sete
            } else {
                0x95 // setne
            };
            code.extend_from_slice(&[0x0F, set_opcode, 0xC0]); // set<cc> al
            code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
        }
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual => {
            // Ordering uses unsigned condition codes for unsigned kinds and
            // signed condition codes for signed kinds, on the normalized cells.
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
            let set_opcode = if kind.is_unsigned() {
                match op {
                    BinaryOp::Less => 0x92,         // setb
                    BinaryOp::LessEqual => 0x96,    // setbe
                    BinaryOp::Greater => 0x97,      // seta
                    BinaryOp::GreaterEqual => 0x93, // setae
                    _ => unreachable!(),
                }
            } else {
                match op {
                    BinaryOp::Less => 0x9C,         // setl
                    BinaryOp::LessEqual => 0x9E,    // setle
                    BinaryOp::Greater => 0x9F,      // setg
                    BinaryOp::GreaterEqual => 0x9D, // setge
                    _ => unreachable!(),
                }
            };
            code.extend_from_slice(&[0x0F, set_opcode, 0xC0]); // set<cc> al
            code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
        }
        BinaryOp::BitAnd => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x21, 0xC8]); // and rax, rcx
            emit_normalize_rax(code, kind);
        }
        BinaryOp::BitOr => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x09, 0xC8]); // or rax, rcx
            emit_normalize_rax(code, kind);
        }
        BinaryOp::BitXor => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x31, 0xC8]); // xor rax, rcx
            emit_normalize_rax(code, kind);
        }
        BinaryOp::Shl | BinaryOp::Shr => {
            // Mask the shift count to `width-1` (matching `int_shl`/`int_shr`),
            // move it into cl, then shift the left operand and re-normalize. `<<`
            // is `shl`; `>>` is `sar` (arithmetic) for signed kinds and `shr`
            // (logical) for unsigned kinds.
            //
            // Stack holds the left operand; rax holds the right (count).
            let mask = (kind.width_bits() - 1) as u8; // 7/15/31/63, fits imm8
            code.extend_from_slice(&[0x48, 0x83, 0xE0, mask]); // and rax, mask
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (count in cl)
            code.push(0x58); // pop rax (left = value to shift)
            let shift_op: &[u8] = match (op, kind.is_unsigned()) {
                (BinaryOp::Shl, _) => &[0x48, 0xD3, 0xE0], // shl rax, cl
                (BinaryOp::Shr, true) => &[0x48, 0xD3, 0xE8], // shr rax, cl (logical)
                (BinaryOp::Shr, false) => &[0x48, 0xD3, 0xF8], // sar rax, cl (arithmetic)
                _ => unreachable!(),
            };
            code.extend_from_slice(shift_op);
            emit_normalize_rax(code, kind);
        }
        BinaryOp::And | BinaryOp::Or => {
            return Err("logical and/or must be short-circuited".to_string());
        }
    }
    Ok(())
}

// -- Overflow-aware arithmetic (checked/saturating/wrapping) -----------------
//
// The overflow-aware builtins operate on two operands of the same fixed-width
// kind `T` (`i8`…`u64`/`isize`/`usize`; `i64` is excluded by the type checker).
// `wrapping_*` reuses the default fixed-width `+`/`-`/`*` (wrap then normalize).
// `saturating_*` and `checked_*` share [`emit_overflow_core`], which computes the
// wrapped result plus an overflow flag and a saturation target using hardware
// carry/overflow flags for the 64-bit kinds and exact-then-range-check for the
// narrow kinds — producing results bit-identical to the interpreters'
// `overflow_arith` for every width and sign. No division is used, so no case can
// trap.

/// x86-64 GPR indices for the raw encoders below.
const REG_RAX: u8 = 0;
const REG_RCX: u8 = 1;
const REG_RDX: u8 = 2;
const REG_R8: u8 = 8;
const REG_R9: u8 = 9;
const REG_R10: u8 = 10;

/// `(rex_extension_bit, low_three_bits)` for a GPR index (`0`=rax … `15`=r15).
fn gpr_bits(reg: u8) -> (u8, u8) {
    (u8::from(reg >= 8), reg & 0x7)
}

/// `mov <reg>, imm64` (the full 10-byte form; REX.W, plus REX.B for r8..r15).
fn emit_mov_reg_imm64(code: &mut Vec<u8>, reg: u8, imm: i64) {
    let (ext, low) = gpr_bits(reg);
    code.push(0x48 | ext);
    code.push(0xB8 | low);
    code.extend_from_slice(&imm.to_le_bytes());
}

/// `mov <dest>, <src>` (register to register, REX.W).
fn emit_mov_reg_reg(code: &mut Vec<u8>, dest: u8, src: u8) {
    let (dext, dlow) = gpr_bits(dest);
    let (sext, slow) = gpr_bits(src);
    // 89 /r: r/m <- reg, so reg field = src, r/m field = dest.
    code.push(0x48 | (sext << 2) | dext);
    code.push(0x89);
    code.push(0xC0 | (slow << 3) | dlow);
}

/// A register-to-register ALU op (`opcode` is the `r/m, r` form: `01`=add,
/// `29`=sub, `31`=xor), computing `dest <op>= src` (REX.W).
fn emit_alu_reg_reg(code: &mut Vec<u8>, opcode: u8, dest: u8, src: u8) {
    let (dext, dlow) = gpr_bits(dest);
    let (sext, slow) = gpr_bits(src);
    code.push(0x48 | (sext << 2) | dext);
    code.push(opcode);
    code.push(0xC0 | (slow << 3) | dlow);
}

/// `test <reg>, <reg>` (REX.W), setting SF/ZF for a following conditional jump.
fn emit_test_reg(code: &mut Vec<u8>, reg: u8) {
    let (ext, low) = gpr_bits(reg);
    code.push(0x48 | (ext << 2) | ext);
    code.push(0x85);
    code.push(0xC0 | (low << 3) | low);
}

/// `mov r8b, 1` — set the low byte of the (already-zeroed) overflow register.
fn emit_set_r8b_one(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x41, 0xB0, 0x01]);
}

/// Emit a `jcc rel32` with a placeholder displacement, returning the patch site.
/// `cc` is the low opcode byte (`0x82`=jb/jc, `0x83`=jae/jnc, `0x84`=je/jz,
/// `0x80`=jo, `0x81`=jno, `0x86`=jbe, `0x88`=js, `0x89`=jns, `0x8C`=jl, `0x8F`=jg).
fn emit_jcc(code: &mut Vec<u8>, cc: u8) -> usize {
    code.extend_from_slice(&[0x0F, cc]);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    site
}

/// Emit a `jmp rel32` placeholder, returning the patch site.
fn emit_jmp(code: &mut Vec<u8>) -> usize {
    code.push(0xE9);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    site
}

/// Compute the overflow-aware result of `a <op> b` for fixed-width `kind`.
///
/// Precondition: `a` (left) in `rax`, `b` (right) in `rcx`.
/// Postcondition: `rax` = the wrapped result normalized to `kind` (identical to
/// `wrapping_<op>`); `r8` = the overflow flag (`0`/`1`, full register);
/// `r9` = the saturation target (`T`'s `MAX`/`MIN`/`0`, valid iff `r8 == 1`).
///
/// The 64-bit kinds use the hardware `CF`/`OF` flags after `add`/`sub` and the
/// widening `mul`/`imul` (high half in `rdx`, or `OF` for signed) to detect
/// overflow exactly. The narrow kinds compute the exact 64-bit result (which
/// cannot overflow a 64-bit register) and range-check it against `[min, max]`.
fn emit_overflow_core(code: &mut Vec<u8>, op: OverflowOp, kind: IntKind) {
    let (min_i128, max_i128) = kind.range_i128();
    let min = min_i128 as i64;
    let max = max_i128 as i64;
    let w64 = matches!(kind, IntKind::U64 | IntKind::Usize | IntKind::Isize);
    let unsigned = kind.is_unsigned();

    if w64 {
        // Clear the overflow flag before the arithmetic (xor also clears CF/OF).
        emit_alu_reg_reg(code, 0x31, REG_R8, REG_R8); // xor r8, r8
        match op {
            OverflowOp::Add => emit_alu_reg_reg(code, 0x01, REG_RAX, REG_RCX), // add rax, rcx
            OverflowOp::Sub => emit_alu_reg_reg(code, 0x29, REG_RAX, REG_RCX), // sub rax, rcx
            OverflowOp::Mul => {
                if unsigned {
                    // mul rcx: rdx:rax = rax * rcx (unsigned). Overflow iff rdx != 0.
                    code.extend_from_slice(&[0x48, 0xF7, 0xE1]);
                } else {
                    // Signed: product sign = sign(a) ^ sign(b); capture it in r10
                    // before `imul` overwrites rax.
                    emit_mov_reg_reg(code, REG_R10, REG_RAX); // r10 = a
                    emit_alu_reg_reg(code, 0x31, REG_R10, REG_RCX); // r10 ^= b
                    // imul rcx (one-operand): rdx:rax = rax * rcx (signed); OF set
                    // when the full product does not fit 64-bit signed.
                    code.extend_from_slice(&[0x48, 0xF7, 0xE9]);
                }
            }
        }
        // Branch to `done` when there is no overflow, else set r8 = 1 and the
        // saturation target r9.
        let no_ovf = match op {
            OverflowOp::Mul if unsigned => {
                emit_test_reg(code, REG_RDX); // test rdx, rdx
                emit_jcc(code, 0x84) // jz -> no overflow
            }
            _ if unsigned => emit_jcc(code, 0x83), // jnc -> no overflow (add/sub carry)
            _ => emit_jcc(code, 0x81),             // jno -> no overflow (signed OF)
        };
        emit_set_r8b_one(code);
        match (op, unsigned) {
            // Unsigned add/mul saturate up to MAX; unsigned sub saturates to 0.
            (OverflowOp::Sub, true) => emit_alu_reg_reg(code, 0x31, REG_R9, REG_R9), // r9 = 0
            (_, true) => emit_mov_reg_imm64(code, REG_R9, max),
            // Signed mul: target sign from r10 (product sign). Add/sub: from the
            // wrapped result's sign (a signed overflow flips the true sign).
            (OverflowOp::Mul, false) => {
                emit_mov_reg_imm64(code, REG_R9, max);
                emit_test_reg(code, REG_R10);
                let keep = emit_jcc(code, 0x89); // jns -> product >= 0, keep MAX
                emit_mov_reg_imm64(code, REG_R9, min);
                patch_rel32(code, keep);
            }
            (_, false) => {
                emit_mov_reg_imm64(code, REG_R9, max);
                emit_test_reg(code, REG_RAX);
                let keep = emit_jcc(code, 0x88); // js -> wrapped < 0, keep MAX
                emit_mov_reg_imm64(code, REG_R9, min);
                patch_rel32(code, keep);
            }
        }
        patch_rel32(code, no_ovf);
    } else {
        // Narrow kinds: the exact 64-bit result cannot overflow the register.
        match op {
            OverflowOp::Add => emit_alu_reg_reg(code, 0x01, REG_RAX, REG_RCX), // add rax, rcx
            OverflowOp::Sub => emit_alu_reg_reg(code, 0x29, REG_RAX, REG_RCX), // sub rax, rcx
            OverflowOp::Mul => code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]), // imul rax, rcx
        }
        emit_alu_reg_reg(code, 0x31, REG_R8, REG_R8); // xor r8, r8 (default no overflow)
        if unsigned {
            match op {
                // Unsigned subtraction underflows iff the exact result is negative;
                // it saturates to 0.
                OverflowOp::Sub => {
                    emit_test_reg(code, REG_RAX);
                    let no_ovf = emit_jcc(code, 0x89); // jns -> >= 0, no underflow
                    emit_set_r8b_one(code);
                    emit_alu_reg_reg(code, 0x31, REG_R9, REG_R9); // r9 = 0
                    patch_rel32(code, no_ovf);
                }
                // Unsigned add/mul overflow iff the exact result exceeds MAX; they
                // saturate up to MAX.
                _ => {
                    emit_cmp_rax_imm(code, max);
                    let no_ovf = emit_jcc(code, 0x86); // jbe -> <= max, no overflow
                    emit_set_r8b_one(code);
                    emit_mov_reg_imm64(code, REG_R9, max);
                    patch_rel32(code, no_ovf);
                }
            }
        } else {
            // Signed: overflow iff the exact result is outside [min, max]; the
            // saturation target is the bound it crossed.
            emit_cmp_rax_imm(code, max);
            let pos = emit_jcc(code, 0x8F); // jg -> above max
            emit_cmp_rax_imm(code, min);
            let neg = emit_jcc(code, 0x8C); // jl -> below min
            let done_ok = emit_jmp(code);
            patch_rel32(code, pos);
            emit_set_r8b_one(code);
            emit_mov_reg_imm64(code, REG_R9, max);
            let done_pos = emit_jmp(code);
            patch_rel32(code, neg);
            emit_set_r8b_one(code);
            emit_mov_reg_imm64(code, REG_R9, min);
            patch_rel32(code, done_ok);
            patch_rel32(code, done_pos);
        }
        // Normalize the wrapped result to the kind's width (identity when the
        // result is in range, which is the only case saturating/checked read it).
        emit_normalize_rax(code, kind);
    }
}

/// Lower `saturating_<op>(a, b) -> T`: compute the clamped result into `rax`.
fn lower_native_saturating(
    ctx: &mut NativeCtx,
    op: OverflowOp,
    kind: IntKind,
    left: &BytecodeExpr,
    right: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, left, code)?;
    code.push(0x50); // push rax (a)
    lower_native_expr(ctx, right, code)?; // rax = b
    emit_mov_reg_reg(code, REG_RCX, REG_RAX); // rcx = b
    code.push(0x58); // pop rax (a)
    emit_overflow_core(code, op, kind);
    // result = overflow ? target : wrapped.
    emit_test_reg(code, REG_R8);
    let keep = emit_jcc(code, 0x84); // jz -> no overflow, keep wrapped
    emit_mov_reg_reg(code, REG_RAX, REG_R9); // rax = saturation target
    patch_rel32(code, keep);
    Ok(())
}

/// Lower `checked_<op>(a, b) -> option<T>` into the enum record at `base_slot`:
/// tag word = `some`/`none` per overflow, payload word = the wrapped result
/// (read only in the `some` case). Mirrors [`lower_map_get_into`]'s option build.
#[allow(clippy::too_many_arguments)]
fn lower_native_checked_into(
    ctx: &mut NativeCtx,
    base_slot: i32,
    result_ty: &TypeRef,
    op: OverflowOp,
    kind: IntKind,
    left: &BytecodeExpr,
    right: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let layout = resolve_native_type(result_ty, ctx.structs, ctx.enums)?;
    let NativeType::Enum { variants, .. } = &layout else {
        return Err(format!(
            "checked_* result type `{}` is not a supported option enum",
            result_ty.name
        ));
    };
    let some_tag = variants
        .iter()
        .find(|v| v.name == "some")
        .map(|v| v.tag)
        .ok_or_else(|| "checked_* result option layout missing `some` variant".to_string())?;
    let none_tag = variants
        .iter()
        .find(|v| v.name == "none")
        .map(|v| v.tag)
        .ok_or_else(|| "checked_* result option layout missing `none` variant".to_string())?;
    lower_native_expr(ctx, left, code)?;
    code.push(0x50); // push rax (a)
    lower_native_expr(ctx, right, code)?; // rax = b
    emit_mov_reg_reg(code, REG_RCX, REG_RAX); // rcx = b
    code.push(0x58); // pop rax (a)
    emit_overflow_core(code, op, kind);
    // Payload word = the wrapped result (rax) at base_slot + 8.
    store_local(code, base_slot + 8);
    // Tag word = overflow ? none : some, at base_slot.
    emit_mov_rax_imm(code, some_tag);
    emit_test_reg(code, REG_R8);
    let store = emit_jcc(code, 0x84); // jz -> no overflow, keep `some`
    emit_mov_rax_imm(code, none_tag);
    patch_rel32(code, store);
    store_local(code, base_slot);
    Ok(())
}

/// Normalize rax to a canonical boolean (1 if non-zero, else 0).
fn normalize_bool(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.extend_from_slice(&[0x0F, 0x95, 0xC0]); // setne al
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
}

/// Combine a binary op whose left operand is on the stack (pushed) and whose
/// right operand is in rax. Result left in rax.
fn emit_i64_binop_from_stack(code: &mut Vec<u8>, op: BinaryOp) -> Result<(), String> {
    // pop rcx (left); result = rcx <op> rax for arithmetic that isn't commutative
    // is handled by moving operands into the right registers below.
    match op {
        BinaryOp::Add => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x01, 0xC8]); // add rax, rcx
        }
        BinaryOp::Subtract => {
            // want left - right = rcx - rax
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
        }
        BinaryOp::Multiply => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]); // imul rax, rcx
        }
        BinaryOp::Divide => {
            // left / right = rcx / rax ; idiv divides rdx:rax by operand.
            code.push(0x59); // pop rcx (left = dividend)
            code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (divisor)
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx (dividend)
            emit_signed_idiv_r8(code); // guarded against i64::MIN / -1 overflow
        }
        BinaryOp::Remainder => {
            // left % right: the same idiv leaves the remainder in rdx; move it
            // into rax. `x % -1 == 0` is handled inside emit_signed_irem_r8.
            code.push(0x59); // pop rcx (left = dividend)
            code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (divisor)
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx (dividend)
            emit_signed_irem_r8(code);
            code.extend_from_slice(&[0x48, 0x89, 0xD0]); // mov rax, rdx (remainder)
        }
        BinaryOp::Equal
        | BinaryOp::NotEqual
        | BinaryOp::Less
        | BinaryOp::LessEqual
        | BinaryOp::Greater
        | BinaryOp::GreaterEqual => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
            let set_opcode = match op {
                BinaryOp::Equal => 0x94,        // sete
                BinaryOp::NotEqual => 0x95,     // setne
                BinaryOp::Less => 0x9C,         // setl
                BinaryOp::LessEqual => 0x9E,    // setle
                BinaryOp::Greater => 0x9F,      // setg
                BinaryOp::GreaterEqual => 0x9D, // setge
                _ => unreachable!(),
            };
            code.extend_from_slice(&[0x0F, set_opcode, 0xC0]); // set<cc> al
            code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
        }
        BinaryOp::And | BinaryOp::Or => {
            return Err("logical and/or must be short-circuited".to_string());
        }
        BinaryOp::BitAnd => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x21, 0xC8]); // and rax, rcx
        }
        BinaryOp::BitOr => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x09, 0xC8]); // or rax, rcx
        }
        BinaryOp::BitXor => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x31, 0xC8]); // xor rax, rcx
        }
        BinaryOp::Shl | BinaryOp::Shr => {
            // `i64` is signed, so `>>` is an arithmetic shift (`sar`). The count is
            // masked to 63 (matching `int_shl`/`int_shr`'s `& (width-1)`). Stack
            // holds the left value; rax holds the right (count).
            code.extend_from_slice(&[0x48, 0x83, 0xE0, 0x3F]); // and rax, 63
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (count in cl)
            code.push(0x58); // pop rax (left = value to shift)
            match op {
                BinaryOp::Shl => code.extend_from_slice(&[0x48, 0xD3, 0xE0]), // shl rax, cl
                BinaryOp::Shr => code.extend_from_slice(&[0x48, 0xD3, 0xF8]), // sar rax, cl
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}

/// Emit a signed 64-bit division of `rax` (dividend) by `r8` (divisor), leaving
/// the quotient in `rax`. The plain `idiv` instruction raises a hardware #DE on
/// the single overflow case `i64::MIN / -1`, whereas the interpreters use
/// `wrapping_div`, which yields `i64::MIN` for that input (see `int_div` in
/// `lullaby_runtime`). To match the interpreters bit-for-bit and avoid the trap,
/// special-case a divisor of `-1`: for any `x`, `x / -1 == -x` under wrapping
/// (including `i64::MIN / -1 == i64::MIN`, since `neg` of `i64::MIN` wraps to
/// itself). The caller must guarantee a non-zero divisor (division by zero is
/// rejected earlier as `L0404`).
fn emit_signed_idiv_r8(code: &mut Vec<u8>) {
    // cmp r8, -1
    code.extend_from_slice(&[0x49, 0x83, 0xF8, 0xFF]);
    // jne +5  (skip the neg/jmp pair, fall through to cqo/idiv)
    code.extend_from_slice(&[0x75, 0x05]);
    // neg rax  (rax = -rax, wrapping; this is x / -1 for the whole i64 range)
    code.extend_from_slice(&[0x48, 0xF7, 0xD8]);
    // jmp +5  (skip cqo/idiv)
    code.extend_from_slice(&[0xEB, 0x05]);
    // cqo  (sign-extend rax into rdx:rax)
    code.extend_from_slice(&[0x48, 0x99]);
    // idiv r8
    code.extend_from_slice(&[0x49, 0xF7, 0xF8]);
}

/// Emit a signed 64-bit remainder of `rax` (dividend) by `r8` (divisor), leaving
/// the remainder in `rdx` (the caller moves it where it needs it). Like
/// [`emit_signed_idiv_r8`], the plain `idiv` raises #DE on `i64::MIN / -1`, but
/// the true remainder there is `0` (`i64::MIN % -1 == 0`, matching `wrapping_rem`
/// in the interpreters). Special-case a divisor of `-1` by setting the remainder
/// to `0` directly and skipping the trapping `idiv`. The caller must guarantee a
/// non-zero divisor.
fn emit_signed_irem_r8(code: &mut Vec<u8>) {
    // cmp r8, -1
    code.extend_from_slice(&[0x49, 0x83, 0xF8, 0xFF]);
    // jne +5  (skip the xor/jmp pair, fall through to cqo/idiv)
    code.extend_from_slice(&[0x75, 0x05]);
    // xor rdx, rdx  (remainder of x % -1 is 0 for the whole i64 range)
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);
    // jmp +5  (skip cqo/idiv)
    code.extend_from_slice(&[0xEB, 0x05]);
    // cqo  (sign-extend rax into rdx:rax)
    code.extend_from_slice(&[0x48, 0x99]);
    // idiv r8  (quotient -> rax, remainder -> rdx)
    code.extend_from_slice(&[0x49, 0xF7, 0xF8]);
}

// -- String op lowering (native) ---------------------------------------------
//
// A `string` value is a heap pointer to `[char_len i64][byte_len i64][utf8]`. The
// heavy lifting (allocate, header math, byte copies, itoa) lives in `.text`
// helpers so each call site stays small; the inline codegen below stages
// operands and calls them. Every helper call is a `call`, so the frame reserves
// shadow space and stays 16-byte aligned (see `expr_has_call`, which reports a
// string literal and a string `+` as calls so the planner reserves it).

/// Lower `a + b` string concatenation: evaluate both operands to record pointers
/// (`a` in `rcx`, `b` in `rdx`), then call `__lullaby_str_concat`, which
/// allocates a fresh record, sums the headers, and byte-copies both UTF-8 ranges.
/// The concatenated record pointer is left in `rax`.
fn lower_string_concat(
    ctx: &mut NativeCtx,
    left: &BytecodeExpr,
    right: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // Evaluate the left operand and spill its pointer, then the right (its
    // evaluation may itself be a call that clobbers registers), then load both
    // into the helper's argument registers.
    lower_native_expr(ctx, left, code)?;
    code.push(0x50); // push rax (left pointer)
    lower_native_expr(ctx, right, code)?; // right pointer -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (right)
    code.push(0x59); // pop rcx (left)
    // If an operand is a uniquely-owned fresh string temporary (a literal,
    // `to_string`/`substring`/`trim`/`repeat`, or a nested concat — never a borrowed
    // variable/container read), it is dead after the concat; use the ownership-aware
    // helper to `rc_dec` it, reclaiming the intermediate. When neither operand is a
    // fresh temp (the common `var + var`), the bare concat keeps zero overhead.
    let mask =
        (is_owning_string_alloc(left) as i32) | ((is_owning_string_alloc(right) as i32) << 1);
    if mask == 0 {
        emit_call_symbol(ctx, STR_CONCAT_SYMBOL, code);
    } else {
        // mov r8d, mask ; call __lullaby_str_concat_own
        code.extend_from_slice(&[0x41, 0xB8]);
        code.extend_from_slice(&mask.to_le_bytes());
        emit_call_symbol(ctx, STR_CONCAT_OWN_SYMBOL, code);
    }
    Ok(())
}

/// Lower `substring(s, start, end) -> string`: evaluate the source record pointer
/// into `rcx`, the `start`/`end` char indices (i64) into `rdx`/`r8`, then call
/// `__lullaby_str_substring`, which bounds-checks (trapping on `L0413`), maps the
/// char indices to byte offsets by walking the UTF-8, allocates a fresh record,
/// and byte-copies the slice. The slice record pointer is left in `rax`. Operands
/// are evaluated left-to-right and spilled, because each evaluation may itself be
/// a call that clobbers the argument registers.
fn lower_string_substring(
    ctx: &mut NativeCtx,
    s: &BytecodeExpr,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, s, code)?; // record pointer -> rax
    code.push(0x50); // push rax (string record)
    lower_native_expr(ctx, start, code)?; // start i64 -> rax
    code.push(0x50); // push rax (start)
    lower_native_expr(ctx, end, code)?; // end i64 -> rax
    code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (end)
    code.push(0x5A); // pop rdx (start)
    code.push(0x59); // pop rcx (string record)
    emit_call_symbol(ctx, STR_SUBSTRING_SYMBOL, code);
    Ok(())
}

/// Lower `s[i]`: stage the string record pointer into `rcx` and the char index
/// into `rdx`, then call the char-at helper, which leaves the `i`-th code point
/// (an `i64` `char` cell) in `rax`. Operands spill because evaluating the index
/// may clobber the string's register.
fn lower_string_char_at(
    ctx: &mut NativeCtx,
    s: &BytecodeExpr,
    index: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, s, code)?; // record pointer -> rax
    code.push(0x50); // push rax (string record)
    lower_native_expr(ctx, index, code)?; // index i64 -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (index)
    code.push(0x59); // pop rcx (string record)
    emit_call_symbol(ctx, STR_CHAR_AT_SYMBOL, code);
    Ok(())
}

/// Lower `repeat(s, count)`: stage the source record into `rcx` and the count
/// into `rdx`, then call the repeat helper (result record in `rax`).
fn lower_string_repeat(
    ctx: &mut NativeCtx,
    s: &BytecodeExpr,
    count: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, s, code)?; // record pointer -> rax
    code.push(0x50); // push rax (record)
    lower_native_expr(ctx, count, code)?; // count -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (count)
    code.push(0x59); // pop rcx (record)
    emit_call_symbol(ctx, STR_REPEAT_SYMBOL, code);
    Ok(())
}

/// Lower `trim(s)`: stage the source record into `rcx` and call the trim helper
/// (result record in `rax`).
fn lower_string_trim(
    ctx: &mut NativeCtx,
    s: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, s, code)?; // record pointer -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_call_symbol(ctx, STR_TRIM_SYMBOL, code);
    Ok(())
}

/// Lower a two-string operation (`find`/`contains`/`starts_with`/`ends_with`):
/// evaluate the first string record pointer into `rcx` and the second into `rdx`,
/// then call the named `.text` helper, which leaves its result (an `i64` char
/// index for `find`, a `0`/`1` bool for the predicates) in `rax`. The operands are
/// spilled because the right operand's evaluation may clobber the left's register.
fn lower_string_binary_op(
    ctx: &mut NativeCtx,
    left: &BytecodeExpr,
    right: &BytecodeExpr,
    symbol: &str,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, left, code)?; // left record pointer -> rax
    code.push(0x50); // push rax (left)
    lower_native_expr(ctx, right, code)?; // right record pointer -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (right)
    code.push(0x59); // pop rcx (left)
    emit_call_symbol(ctx, symbol, code);
    Ok(())
}

/// Lower `to_string(x)` to a fresh heap string record pointer in `rax`, matching
/// the interpreters' `Display`/`builtin_to_string`. An `f64`/`f32` argument
/// (dtoa) is deferred and rejected so the enclosing function skips gracefully.
fn lower_to_string(
    ctx: &mut NativeCtx,
    arg: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let type_name = arg.ty.name.as_str();
    match type_name {
        // Identity: a string is already a heap record pointer.
        "string" => lower_native_expr(ctx, arg, code),
        // `bool` -> "true"/"false".
        "bool" => {
            lower_native_expr(ctx, arg, code)?; // 0/1 -> rax
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
            emit_call_symbol(ctx, STR_FROM_BOOL_SYMBOL, code);
            Ok(())
        }
        // `char` -> the code point's UTF-8 encoding (a one-char string).
        "char" => {
            lower_native_expr(ctx, arg, code)?; // code point -> rax
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
            emit_call_symbol(ctx, STR_FROM_CHAR_SYMBOL, code);
            Ok(())
        }
        // `byte` (0..=255) -> unsigned decimal.
        "byte" => lower_int_to_string(ctx, arg, false, code),
        // `i64` and the fixed-width integers -> decimal, signed or unsigned by kind.
        "i64" => lower_int_to_string(ctx, arg, true, code),
        name => match fixed_int_kind(name) {
            Some(kind) => lower_int_to_string(ctx, arg, !kind.is_unsigned(), code),
            None => Err(format!(
                "to_string of `{name}` is not in the native subset (float to_string is deferred)"
            )),
        },
    }
}

/// Lower an integer `to_string(x)`: evaluate `x` into `rcx`, set `rdx` to the
/// signedness flag (nonzero = signed `i64` formatting, zero = unsigned `u64`),
/// then call `__lullaby_str_from_int`.
fn lower_int_to_string(
    ctx: &mut NativeCtx,
    arg: &BytecodeExpr,
    signed: bool,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, arg, code)?; // normalized cell -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (value)
    // mov edx, signed_flag  (0 or 1; zero-extends into rdx)
    code.push(0xBA);
    code.extend_from_slice(&(i32::from(signed)).to_le_bytes());
    emit_call_symbol(ctx, STR_FROM_INT_SYMBOL, code);
    Ok(())
}

// -- Growable list op lowering (native) --------------------------------------
//
// A `list<T>` value is a heap pointer to `[len i64][cap i64][slots]`. The heavy
// lifting (allocate, deep-copy, grow) lives in three `.text` helpers
// (`__lullaby_list_new`/`__lullaby_list_copy`/`__lullaby_list_grow`) so each call
// site stays small; the inline codegen below stages operands and calls them. The
// helper calls (and any list op) are `Call` IR nodes, so the frame reserves
// shadow space and stays 16-byte aligned at each `call` exactly like other calls.

/// `list_new()` -> a fresh `[len=0][cap=LIST_INITIAL_CAP][slots]` heap block
/// pointer in `rax`. Just calls the runtime helper.
fn lower_list_new(ctx: &mut NativeCtx, code: &mut Vec<u8>) {
    emit_call_symbol(ctx, LIST_NEW_SYMBOL, code);
}

/// Emit a relocated `call rel32` against a `.text` symbol, leaving the callee's
/// `rax` result in place. Used for the list runtime helpers.
fn emit_call_symbol(ctx: &mut NativeCtx, symbol: &str, code: &mut Vec<u8>) {
    code.push(0xE8);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: site as u32,
        symbol: symbol.to_string(),
    });
}

/// Deep-copy the collection-slot value whose pointer is in `rax`, leaving a fresh
/// independent value's pointer in `rax`. This is the native realization of the
/// interpreters' recursive `Value::clone` on a MUTABLE-heap collection element /
/// map value / enum payload, mirroring the WASM backend's `emit_deep_copy`:
///
/// - a **scalar or `string`** slot needs no copy — the word in `rax` is already the
///   value (a `string`'s shared pointer IS its value-semantic clone since strings
///   are immutable), so this is the identity;
/// - a **`HeapStruct`** slot calls `__lullaby_struct_copy` (a fresh independent
///   field block, deep at the one-level bound);
/// - a nested **`List`** slot calls `__lullaby_list_copy` (its own elements are
///   scalars/strings at this bound, so a flat copy is an exact deep copy);
/// - a nested **`Map`** slot calls `__lullaby_map_copy`.
///
/// The call is emitted inline within a list/map op (a `Call` IR node), so the frame
/// already reserves shadow space and keeps `rsp` 16-byte aligned at the `call`.
fn emit_heap_slot_deep_copy(ctx: &mut NativeCtx, slot_ty: &NativeType, code: &mut Vec<u8>) {
    match slot_ty {
        NativeType::HeapStruct { .. } => {
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
            emit_call_symbol(ctx, STRUCT_COPY_SYMBOL, code);
        }
        NativeType::List { .. } => {
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
            emit_call_symbol(ctx, LIST_COPY_SYMBOL, code);
        }
        NativeType::Map { .. } => {
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
            emit_call_symbol(ctx, MAP_COPY_SYMBOL, code);
        }
        // Scalar or immutable string: the value in rax is already its own copy.
        _ => {}
    }
}

/// After a list has been flat-copied (via `__lullaby_list_copy`/`_grow`), walk its
/// `len` live element slots and replace each with an INDEPENDENT deep copy of
/// itself, so a mutable-aggregate element (`HeapStruct`/nested `List`/`Map`) is not
/// shared between the source list and the copy. `list_slot`/`elem_ty`: the list
/// pointer lives in a caller frame slot (so it survives the internal helper calls),
/// and `elem_ty` is the element's resolved `NativeType`. A scalar/string element
/// needs no fixup and this is a no-op. The fixup runs entirely on volatile registers
/// plus the frame slot; each per-element deep copy keeps `rsp` aligned at its call.
fn emit_list_deep_fixup(
    ctx: &mut NativeCtx,
    list_slot: i32,
    elem_ty: &NativeType,
    code: &mut Vec<u8>,
) {
    if !native_slot_needs_deep_copy(elem_ty) {
        return;
    }
    // A per-element counter local and a saved list-pointer local keep state across
    // the deep-copy calls (which clobber volatiles). Use two scratch frame slots.
    let saved_scratch = ctx.scratch_next;
    let i_slot = ctx.alloc_scratch(1);
    // i = 0
    emit_mov_rax_imm(code, 0);
    store_local(code, i_slot);
    let loop_top = code.len();
    // if i >= len -> done. rcx = list; r8 = len = [rcx + LIST_LEN_OFF].
    load_local(code, list_slot); // rax = list ptr
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_mov_r8_from_rcx_disp(code, LIST_LEN_OFF); // r8 = len
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x4C, 0x39, 0xC0]); // cmp rax, r8
    code.extend_from_slice(&[0x0F, 0x8D]); // jge done (rel32)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rax = [rcx + rax*8 + LIST_DATA_OFF]  (load element i pointer)
    code.extend_from_slice(&[0x48, 0x8B, 0x84, 0xC1]); // mov rax, [rcx + rax*8 + disp32]
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    // rax = deep copy of the element.
    emit_heap_slot_deep_copy(ctx, elem_ty, code);
    // Store the fresh pointer back: [list + i*8 + LIST_DATA_OFF] = rax.
    // rcx = list; rdx = i.
    code.push(0x50); // push rax (fresh copy)
    load_local(code, list_slot); // rax = list ptr
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax  (i)
    code.push(0x58); // pop rax (fresh copy)
    // lea r8, [rcx + rdx*8 + LIST_DATA_OFF] ; mov [r8], rax
    code.extend_from_slice(&[0x4C, 0x8D, 0x84, 0xD1]); // lea r8, [rcx + rdx*8 + disp32]
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x49, 0x89, 0x00]); // mov [r8], rax
    // i += 1
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
    store_local(code, i_slot);
    emit_jmp_to(code, loop_top);
    patch_rel32(code, done_site);
    ctx.scratch_next = saved_scratch;
}

/// After a map has been flat-copied (via `__lullaby_map_copy`), walk its `len` live
/// entries and replace each entry's VALUE word with an independent deep copy, so a
/// mutable-aggregate value (`map<K, struct>`) is not shared between the source and
/// the copy. Keys stay flat (they are integer-cell scalars). `map_slot`/`value_ty`
/// mirror [`emit_list_deep_fixup`]. A scalar/string value needs no fixup (no-op).
fn emit_map_deep_fixup(
    ctx: &mut NativeCtx,
    map_slot: i32,
    value_ty: &NativeType,
    code: &mut Vec<u8>,
) {
    if !native_slot_needs_deep_copy(value_ty) {
        return;
    }
    let saved_scratch = ctx.scratch_next;
    let i_slot = ctx.alloc_scratch(1);
    emit_mov_rax_imm(code, 0);
    store_local(code, i_slot);
    let loop_top = code.len();
    load_local(code, map_slot); // rax = map ptr
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_mov_r8_from_rcx_disp(code, MAP_LEN_OFF); // r8 = len
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x4C, 0x39, 0xC0]); // cmp rax, r8
    code.extend_from_slice(&[0x0F, 0x8D]); // jge done
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // Entry value address = rcx + MAP_DATA_OFF + i*MAP_ENTRY_SIZE + MAP_VALUE_OFF.
    // i*16 = i<<4. rax = i ; shl rax, 4 ; lea rdx, [rcx + rax + MAP_DATA_OFF+VALUE].
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x04]); // shl rax, 4
    code.extend_from_slice(&[0x48, 0x8D, 0x94, 0x01]); // lea rdx, [rcx + rax + disp32]
    code.extend_from_slice(&(MAP_DATA_OFF + MAP_VALUE_OFF).to_le_bytes());
    // rax = [rdx]  (the entry value pointer)
    code.extend_from_slice(&[0x48, 0x8B, 0x02]); // mov rax, [rdx]
    emit_heap_slot_deep_copy(ctx, value_ty, code); // rax = fresh copy
    // Recompute the value slot address (rcx/rdx clobbered by the copy) and store.
    code.push(0x50); // push rax (fresh copy)
    load_local(code, map_slot);
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x04]); // shl rax, 4
    code.extend_from_slice(&[0x48, 0x8D, 0x94, 0x01]); // lea rdx, [rcx + rax + disp32]
    code.extend_from_slice(&(MAP_DATA_OFF + MAP_VALUE_OFF).to_le_bytes());
    code.push(0x58); // pop rax (fresh copy)
    code.extend_from_slice(&[0x48, 0x89, 0x02]); // mov [rdx], rax
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
    store_local(code, i_slot);
    emit_jmp_to(code, loop_top);
    patch_rel32(code, done_site);
    ctx.scratch_next = saved_scratch;
}

/// Construct a heap struct from a `Name(field…)` constructor call, leaving the
/// fresh field-0 pointer in `rax`. Allocates `STRUCT_HEADER_SIZE + nwords * 8`,
/// writes the `[nwords]` header, and materializes each field word (a scalar/string
/// through `rax`; a nested MUTABLE-aggregate field is out of the one-level bound and
/// never reaches here). The block is freshly allocated, so the returned value is
/// already an independent snapshot (no extra deep copy needed).
fn lower_heap_struct_construct(
    ctx: &mut NativeCtx,
    fields: &[(String, NativeType)],
    args: &[BytecodeExpr],
    code: &mut Vec<u8>,
) -> Result<(), String> {
    if args.len() != fields.len() {
        return Err("heap struct constructor field-count mismatch".to_string());
    }
    let nwords = fields.len() as i32;
    // rcx = STRUCT_HEADER_SIZE + nwords * 8 ; call __lullaby_alloc -> rax = base.
    emit_mov_rcx_imm(code, (STRUCT_HEADER_SIZE + nwords * 8) as i64);
    emit_call_symbol(ctx, HEAP_ALLOC_SYMBOL, code);
    // Stash the base pointer in a scratch slot across the field evaluations.
    let saved_scratch = ctx.scratch_next;
    let base_slot = ctx.alloc_scratch(1);
    store_local(code, base_slot);
    // [base] = nwords (header word).
    load_local(code, base_slot);
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_mov_rax_imm(code, nwords as i64);
    // mov [rcx], rax
    code.extend_from_slice(&[0x48, 0x89, 0x01]);
    // Each field word at [base + STRUCT_HEADER_SIZE + k*8].
    for (k, (arg, (_, field_ty))) in args.iter().zip(fields.iter()).enumerate() {
        // Evaluate the field value into rax. A field is a scalar or a `string`
        // (immutable pointer); a float field is out of scope (structs reject floats).
        if matches!(field_ty, NativeType::F64 | NativeType::F32) {
            return Err("float heap-struct fields are not in the native subset".to_string());
        }
        lower_native_expr(ctx, arg, code)?;
        code.push(0x50); // push rax (field value)
        load_local(code, base_slot);
        code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (base)
        code.push(0x58); // pop rax (field value)
        // mov [rcx + STRUCT_HEADER_SIZE + k*8], rax
        code.extend_from_slice(&[0x48, 0x89, 0x81]);
        code.extend_from_slice(&(STRUCT_HEADER_SIZE + k as i32 * 8).to_le_bytes());
    }
    // rax = field-0 pointer = base + STRUCT_HEADER_SIZE.
    load_local(code, base_slot);
    emit_add_rax_imm32(code, STRUCT_HEADER_SIZE);
    ctx.scratch_next = saved_scratch;
    Ok(())
}

/// Produce a fresh, INDEPENDENT collection-slot value pointer in `rax` for a
/// MUTABLE-aggregate element/value being stored (by `push`/`set`/`map_set`).
///
/// - A **struct constructor** (`Point(1, 2)`) is built directly on the heap
///   ([`lower_heap_struct_construct`]) — already independent.
/// - A **nested-list literal or any other expression** yielding a `List`/`Map`/
///   `HeapStruct` pointer is evaluated and then DEEP-COPIED, so a later mutation of
///   the source binding never leaks into the collection (the interpreters clone the
///   argument `Value` before storing it).
fn lower_heap_slot_value(
    ctx: &mut NativeCtx,
    slot_ty: &NativeType,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    if let (NativeType::HeapStruct { name, fields }, BytecodeExprKind::Call { name: cname, args }) =
        (slot_ty, &value.kind)
        && cname == name
    {
        // A direct constructor: build fresh on the heap (already independent).
        return lower_heap_struct_construct(ctx, fields, args, code);
    }
    // Any other expression yields an existing aggregate pointer; deep-copy it.
    lower_native_expr(ctx, value, code)?;
    emit_heap_slot_deep_copy(ctx, slot_ty, code);
    Ok(())
}

/// Lower `push(l, x) -> list<T>` (value-semantic append): deep-copy `l`, grow the
/// copy if it is full, store `x` into slot `len`, bump `len`, and leave the fresh
/// list pointer in `rax`. Because `push` always returns a NEW list,
/// `l = push(l, x)` matches the interpreters' `Value::clone`-then-append.
fn lower_list_push(
    ctx: &mut NativeCtx,
    list: &BytecodeExpr,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let elem = supported_list_element(&list.ty).ok_or_else(|| {
        format!(
            "push expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let elem_ty = native_collection_slot(&elem, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element `{}` is not layable-out", elem.name))?;
    let deep_elem = native_slot_needs_deep_copy(&elem_ty);
    // rax = deep copy of the source list.
    lower_native_expr(ctx, list, code)?;
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (copy source)
    emit_call_symbol(ctx, LIST_COPY_SYMBOL, code); // rax = fresh copy
    // Ensure room for one more element: rax = grow(copy) (a no-op when cap > len).
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_call_symbol(ctx, LIST_GROW_SYMBOL, code); // rax = grown copy
    // For a MUTABLE-aggregate element, deep-copy the copied list's existing elements
    // so they are independent of the source list (the flat helper copy shared them).
    let saved_scratch = ctx.scratch_next;
    let list_slot = ctx.alloc_scratch(1);
    store_local(code, list_slot); // stash the (grown) list pointer
    if deep_elem {
        emit_list_deep_fixup(ctx, list_slot, &elem_ty, code);
    }
    load_local(code, list_slot); // rax = list ptr
    code.push(0x50); // push rax (save the list pointer across value evaluation)
    // Evaluate the value to append into rax (a scalar, a float bit pattern, or a
    // MUTABLE-aggregate pointer that is deep-copied so a later mutation of the
    // source value never leaks into the list — matching the interpreters).
    if let Some(width) = FloatWidth::from_type_name(&elem.name) {
        lower_native_float_expr(ctx, value, code)?;
        emit_movq_rax_from_xmm0(code, width);
    } else if deep_elem {
        lower_heap_slot_value(ctx, &elem_ty, value, code)?;
    } else {
        lower_native_expr(ctx, value, code)?;
    }
    ctx.scratch_next = saved_scratch;
    // rcx = list pointer (restored); the element value stays in rax.
    code.push(0x59); // pop rcx
    // r8 = len = [rcx + LIST_LEN_OFF]
    emit_mov_r8_from_rcx_disp(code, LIST_LEN_OFF);
    // Element slot address: rdx = rcx + LIST_DATA_OFF + r8*8.
    // lea rdx, [rcx + r8*8 + LIST_DATA_OFF]
    code.extend_from_slice(&[0x4A, 0x8D, 0x94, 0xC1]);
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    // mov [rdx], rax  (store the element word)
    code.extend_from_slice(&[0x48, 0x89, 0x02]);
    // len += 1: r8 += 1; mov [rcx + LIST_LEN_OFF], r8
    code.extend_from_slice(&[0x49, 0xFF, 0xC0]); // inc r8
    emit_mov_rcx_disp_from_r8(code, LIST_LEN_OFF);
    // Result: the list pointer.
    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
    Ok(())
}

/// Lower `set(l, i, x) -> list<T>` (value-semantic replace): deep-copy `l`, store
/// `x` into element slot `i` of the copy, leave the fresh list pointer in `rax`.
/// In-bounds writes match the interpreters; an out-of-range index writes past the
/// live elements into the (still-allocated) capacity or beyond, consistent with
/// the native no-bounds-check discipline for arrays.
fn lower_list_set(
    ctx: &mut NativeCtx,
    list: &BytecodeExpr,
    index: &BytecodeExpr,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let elem = supported_list_element(&list.ty).ok_or_else(|| {
        format!(
            "set expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let elem_ty = native_collection_slot(&elem, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element `{}` is not layable-out", elem.name))?;
    let deep_elem = native_slot_needs_deep_copy(&elem_ty);
    // rax = deep copy of the source list.
    lower_native_expr(ctx, list, code)?;
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_call_symbol(ctx, LIST_COPY_SYMBOL, code); // rax = fresh copy
    // Deep-copy the copied list's existing elements so the returned list is fully
    // independent of the source (the flat helper copy shared their pointers).
    let saved_scratch = ctx.scratch_next;
    let list_slot = ctx.alloc_scratch(1);
    if deep_elem {
        store_local(code, list_slot);
        emit_list_deep_fixup(ctx, list_slot, &elem_ty, code);
        load_local(code, list_slot);
    }
    code.push(0x50); // push rax (the copy pointer)
    // rax = index (i64).
    lower_native_expr(ctx, index, code)?;
    code.push(0x50); // push rax (index)
    // Evaluate the replacement value into rax (float via xmm0 -> rax; a MUTABLE
    // aggregate is built/deep-copied fresh so it is independent of its source).
    if let Some(width) = FloatWidth::from_type_name(&elem.name) {
        lower_native_float_expr(ctx, value, code)?;
        emit_movq_rax_from_xmm0(code, width);
    } else if deep_elem {
        lower_heap_slot_value(ctx, &elem_ty, value, code)?;
    } else {
        lower_native_expr(ctx, value, code)?;
    }
    ctx.scratch_next = saved_scratch;
    code.push(0x59); // pop rcx (index)
    code.push(0x5A); // pop rdx (list pointer)
    // Element slot address: rdx = rdx + LIST_DATA_OFF + rcx*8.
    // lea rdx, [rdx + rcx*8 + LIST_DATA_OFF]
    code.extend_from_slice(&[0x48, 0x8D, 0x94, 0xCA]);
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    // mov [rdx], rax  (store the element)
    code.extend_from_slice(&[0x48, 0x89, 0x02]);
    // Result: the (copied) list pointer. Recompute it: rax = rdx - (index*8 + DATA).
    // Simpler: the copy pointer was pushed first; recover by reloading is complex,
    // so instead keep it: we overwrote rdx with the slot address. Recompute the
    // base by subtracting the same offset.
    // rax = rdx ; rax -= rcx*8 ; rax -= LIST_DATA_OFF
    code.extend_from_slice(&[0x48, 0x89, 0xD0]); // mov rax, rdx
    code.extend_from_slice(&[0x48, 0xC1, 0xE1, 0x03]); // shl rcx, 3
    code.extend_from_slice(&[0x48, 0x29, 0xC8]); // sub rax, rcx
    emit_sub_rax_imm32(code, LIST_DATA_OFF);
    Ok(())
}

/// Lower `pop(l) -> list<T>` (value-semantic remove-last): deep-copy `l`,
/// decrement the copy's `len` (the slot stays allocated, like `Vec::pop`), leave
/// the fresh list pointer in `rax`. Popping an empty list is `L0413` on the
/// interpreters; the native path decrements `len` toward `-1`, so the program is
/// expected to keep the same non-empty precondition the interpreters require.
fn lower_list_pop(
    ctx: &mut NativeCtx,
    list: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let elem = supported_list_element(&list.ty).ok_or_else(|| {
        format!(
            "pop expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let elem_ty = native_collection_slot(&elem, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element `{}` is not layable-out", elem.name))?;
    lower_native_expr(ctx, list, code)?;
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_call_symbol(ctx, LIST_COPY_SYMBOL, code); // rax = fresh copy
    // Deep-copy the copied list's remaining elements so the returned list is
    // independent of the source (the flat helper copy shared their pointers).
    if native_slot_needs_deep_copy(&elem_ty) {
        let saved_scratch = ctx.scratch_next;
        let list_slot = ctx.alloc_scratch(1);
        store_local(code, list_slot);
        emit_list_deep_fixup(ctx, list_slot, &elem_ty, code);
        load_local(code, list_slot);
        ctx.scratch_next = saved_scratch;
    }
    // len -= 1: r8 = [rax + LIST_LEN_OFF]; r8 -= 1; [rax + LIST_LEN_OFF] = r8.
    // mov r8, [rax + LIST_LEN_OFF]
    code.extend_from_slice(&[0x4C, 0x8B, 0x80]);
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x49, 0xFF, 0xC8]); // dec r8
    // mov [rax + LIST_LEN_OFF], r8
    code.extend_from_slice(&[0x4C, 0x89, 0x80]);
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    // Result: the (copied) list pointer already in rax.
    Ok(())
}

/// Lower `get(l, i) -> T`: load element `i` from `l + LIST_DATA_OFF + i*8`. A
/// float element is loaded back into `xmm0` by the float-expr path; this integer
/// path loads the raw word into `rax` (a float `get` result is handled by
/// `lower_native_float_expr`'s list-get case).
///
/// For a MUTABLE-aggregate element (`HeapStruct`/nested `List`/`Map`) the loaded
/// element pointer is DEEP-COPIED (the interpreters' `values[i].clone()`), so the
/// returned value is independent of the list: mutating the retrieved copy never
/// touches the list's element. The result is a heap pointer word; a consumer that
/// wants the stack-flattened struct (a `Struct`-typed local or call argument)
/// bridges it via [`lower_aggregate_init`]'s heap-source path.
/// `a[i]` on a heap `array<string>` (a `list<string>`-layout block): load the
/// `i`-th slot's shared string pointer, bounds-checked against the `len` header.
/// An out-of-range index (including a negative one, caught by the unsigned compare)
/// traps with `ud2`, mirroring the interpreters' `L0413`.
fn lower_array_string_index(
    ctx: &mut NativeCtx,
    target: &BytecodeExpr,
    index: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, index, code)?; // rax = index
    code.push(0x50); // push rax (index)
    lower_native_expr(ctx, target, code)?; // rax = block pointer
    code.push(0x59); // pop rcx (index)
    // r10 = [rax + LIST_LEN_OFF] (element count).
    code.extend_from_slice(&[0x4C, 0x8B, 0x90]); // mov r10, [rax + disp32]
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    // if (unsigned) index >= len -> trap. cmp rcx, r10 ; jb ok ; ud2 ; ok:
    code.extend_from_slice(&[0x4C, 0x39, 0xD1]); // cmp rcx, r10
    code.extend_from_slice(&[0x72, 0x02]); // jb +2 (skip the ud2)
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2
    // rax = [rax + rcx*8 + LIST_DATA_OFF] (the shared string pointer word).
    code.extend_from_slice(&[0x48, 0x8B, 0x84, 0xC8]);
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    Ok(())
}

fn lower_list_get(
    ctx: &mut NativeCtx,
    list: &BytecodeExpr,
    index: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let elem = supported_list_element(&list.ty).ok_or_else(|| {
        format!(
            "get expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let elem_ty = native_collection_slot(&elem, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element `{}` is not layable-out", elem.name))?;
    // rax = index; push it; rax = list pointer; pop rcx = index.
    lower_native_expr(ctx, index, code)?;
    code.push(0x50); // push rax (index)
    lower_native_expr(ctx, list, code)?; // rax = list pointer
    code.push(0x59); // pop rcx (index)
    // rax = [rax + rcx*8 + LIST_DATA_OFF]
    code.extend_from_slice(&[0x48, 0x8B, 0x84, 0xC8]); // mov rax, [rax + rcx*8 + disp32]
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    // A mutable-aggregate element is returned as an independent deep copy.
    if native_slot_needs_deep_copy(&elem_ty) {
        emit_heap_slot_deep_copy(ctx, &elem_ty, code);
    }
    Ok(())
}

// -- Growable map op lowering (native) ---------------------------------------
//
// A `map<K, V>` value is a heap pointer to `[len i64][cap i64][entries]` (each
// entry a `(key, value)` word pair). The heavy lifting (allocate, deep-copy,
// grow, linear-scan) lives in four `.text` helpers (`__lullaby_map_new`/`_copy`/
// `_grow`/`_find`), so each call site stays small; the inline codegen below
// stages operands, calls them, and stores results. Every map op is a `Call` IR
// node, so the frame reserves shadow space and stays 16-byte aligned at each
// `call` exactly like other calls and the list ops.

/// `map_new()` -> a fresh `[len=0][cap=MAP_INITIAL_CAP][entries]` heap block
/// pointer in `rax`. Just calls the runtime helper.
fn lower_map_new(ctx: &mut NativeCtx, code: &mut Vec<u8>) {
    emit_call_symbol(ctx, MAP_NEW_SYMBOL, code);
}

/// Evaluate a map key expression into `rax`. Keys are integer-cell scalars
/// (`supported_map_kv` rejects float keys), so the ordinary integer expression
/// path yields the normalized key word directly.
fn lower_map_key(
    ctx: &mut NativeCtx,
    key: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, key, code)
}

/// Evaluate a map value expression into `rax` as a flat 8-byte word (a float value
/// is moved bit-for-bit from `xmm0` through `rax`, mirroring the list element
/// path), so it can be stored into an entry's value slot.
fn lower_map_value_word(
    ctx: &mut NativeCtx,
    value_ty: &TypeRef,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    if let Some(width) = FloatWidth::from_type_name(&value_ty.name) {
        lower_native_float_expr(ctx, value, code)?;
        emit_movq_rax_from_xmm0(code, width);
    } else {
        lower_native_expr(ctx, value, code)?;
    }
    Ok(())
}

/// Lower `map_set(m, k, v) -> map<K, V>` (value-semantic insert/update): deep-copy
/// `m`, scan the copy for `k`; if found, overwrite that entry's value slot in
/// place (preserving order); otherwise grow when full (capacity doubling) and
/// append a new `(k, v)` entry, bumping `len`. Leaves the fresh map pointer in
/// `rax`. Because `map_set` always returns a NEW map, `m = map_set(m, k, v)`
/// matches the interpreters' clone-then-mutate on the insertion-ordered list.
fn lower_map_set(
    ctx: &mut NativeCtx,
    map: &BytecodeExpr,
    key: &BytecodeExpr,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let (_key_ty, value_ty) = supported_map_kv(&map.ty).ok_or_else(|| {
        format!(
            "map_set expects a scalar-key, scalar-value map but got `{}`",
            map.ty.name
        )
    })?;
    let value_native = native_collection_slot(&value_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("map value `{}` is not layable-out", value_ty.name))?;
    let deep_value = native_slot_needs_deep_copy(&value_native);
    // rax = deep copy of the source map (value semantics).
    lower_native_expr(ctx, map, code)?;
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_call_symbol(ctx, MAP_COPY_SYMBOL, code); // rax = fresh copy
    // For a MUTABLE-aggregate value, deep-copy the copied map's existing entry
    // values so the returned map is fully independent of the source (the flat helper
    // copy shared their pointers).
    let saved_scratch = ctx.scratch_next;
    if deep_value {
        let map_slot = ctx.alloc_scratch(1);
        store_local(code, map_slot);
        emit_map_deep_fixup(ctx, map_slot, &value_native, code);
        load_local(code, map_slot);
    }
    code.push(0x50); // push rax (the copy pointer)
    // Evaluate the key into a saved word (any nested call balances its own stack).
    lower_map_key(ctx, key, code)?;
    code.push(0x50); // push rax (key)
    // Evaluate the value word (float via xmm0 -> rax) and save it. A MUTABLE
    // aggregate value is built/deep-copied fresh so it is independent of its source.
    if deep_value {
        lower_heap_slot_value(ctx, &value_native, value, code)?;
    } else {
        lower_map_value_word(ctx, &value_ty, value, code)?;
    }
    code.push(0x50); // push rax (value)
    // Restore into stable non-argument-clobbered registers: r8 = value, rdx = key,
    // rcx = map copy. All three pops complete before the find call, so rsp is back
    // to the frame-aligned base at that `call`. `__lullaby_map_find` reads rcx/rdx
    // as its args and clobbers only rax/r10/r11, so rcx/rdx/r8 survive the call and
    // need no save/restore around it.
    code.push(0x41);
    code.push(0x58); // pop r8 (value)
    code.push(0x5A); // pop rdx (key)
    code.push(0x59); // pop rcx (map copy)
    emit_call_symbol(ctx, MAP_FIND_SYMBOL, code); // rax = index or len (rcx=map, rdx=key)
    // r10 = len = [rcx + MAP_LEN_OFF]
    code.extend_from_slice(&[0x4C, 0x8B, 0x91]); // mov r10, [rcx + disp32]
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // if rax (found index) == r10 (len) -> append; else overwrite.
    code.extend_from_slice(&[0x4C, 0x39, 0xD0]); // cmp rax, r10
    code.extend_from_slice(&[0x0F, 0x84]); // je append (rel32)
    let append_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // --- overwrite branch: store value into entry `rax`'s value slot ---
    // entry addr = rcx + MAP_DATA_OFF + rax * MAP_ENTRY_SIZE (16). rax*16 = rax<<4.
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x04]); // shl rax, 4
    // lea r11, [rcx + rax + MAP_DATA_OFF]
    code.extend_from_slice(&[0x4C, 0x8D, 0x9C, 0x01]); // lea r11, [rcx + rax + disp32]
    code.extend_from_slice(&MAP_DATA_OFF.to_le_bytes());
    // mov [r11 + MAP_VALUE_OFF], r8
    code.extend_from_slice(&[0x4D, 0x89, 0x83]); // mov [r11 + disp32], r8
    code.extend_from_slice(&MAP_VALUE_OFF.to_le_bytes());
    // rax = rcx (result map pointer) ; jmp done
    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
    code.push(0xE9); // jmp done (rel32)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // --- append branch ---
    patch_rel32(code, append_site);
    // Grow if full: rcx = grow(map). Save key(rdx)/value(r8) across the call.
    code.push(0x52); // push rdx (key)
    code.push(0x41);
    code.push(0x50); // push r8 (value)
    emit_call_symbol(ctx, MAP_GROW_SYMBOL, code); // rax = grown map (rcx = map arg)
    code.push(0x41);
    code.push(0x58); // pop r8 (value)
    code.push(0x5A); // pop rdx (key)
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (rcx = grown map)
    // r10 = len = [rcx + MAP_LEN_OFF]
    code.extend_from_slice(&[0x4C, 0x8B, 0x91]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // entry addr of index `len`: r11 = rcx + MAP_DATA_OFF + len*16.
    // r10*16: shl r10, 4
    code.extend_from_slice(&[0x49, 0xC1, 0xE2, 0x04]); // shl r10, 4
    // lea r11, [rcx + r10 + MAP_DATA_OFF]  (REX.WRX: r11 dest, r10 index)
    code.extend_from_slice(&[0x4E, 0x8D, 0x9C, 0x11]); // lea r11, [rcx + r10 + disp32]
    code.extend_from_slice(&MAP_DATA_OFF.to_le_bytes());
    // mov [r11], rdx  (store key word at the entry's key slot, offset 0)
    code.extend_from_slice(&[0x49, 0x89, 0x13]); // mov [r11], rdx
    // mov [r11 + MAP_VALUE_OFF], r8  (store value word)
    code.extend_from_slice(&[0x4D, 0x89, 0x83]);
    code.extend_from_slice(&MAP_VALUE_OFF.to_le_bytes());
    // len += 1: r10 currently holds len<<4; reload len and bump it.
    // mov r10, [rcx + MAP_LEN_OFF]; inc r10; mov [rcx + MAP_LEN_OFF], r10
    code.extend_from_slice(&[0x4C, 0x8B, 0x91]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x49, 0xFF, 0xC2]); // inc r10
    code.extend_from_slice(&[0x4C, 0x89, 0x91]); // mov [rcx + disp32], r10
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // rax = rcx (result map pointer)
    code.extend_from_slice(&[0x48, 0x89, 0xC8]);
    // done:
    patch_rel32(code, done_site);
    ctx.scratch_next = saved_scratch;
    Ok(())
}

/// Lower `map_has(m, k) -> bool`: scan for `k`, leaving `found != len` (1 if
/// present, 0 if absent) in `rax`.
fn lower_map_has(
    ctx: &mut NativeCtx,
    map: &BytecodeExpr,
    key: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    supported_map_kv(&map.ty).ok_or_else(|| {
        format!(
            "map_has expects a scalar-key, scalar-value map but got `{}`",
            map.ty.name
        )
    })?;
    // Evaluate key first, save it; then map into rcx; restore key into rdx. Both
    // pushes are popped before the find call, so rsp is frame-aligned there.
    // `__lullaby_map_find` preserves rcx (map) and clobbers only rax/r10/r11, so
    // the map pointer survives for the post-call `len` reload.
    lower_map_key(ctx, key, code)?;
    code.push(0x50); // push rax (key)
    lower_native_expr(ctx, map, code)?; // rax = map pointer
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (map)
    code.push(0x5A); // pop rdx (key)
    emit_call_symbol(ctx, MAP_FIND_SYMBOL, code); // rax = index or len (rcx=map, rdx=key)
    // r10 = len = [rcx + MAP_LEN_OFF]
    code.extend_from_slice(&[0x4C, 0x8B, 0x91]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // result = (rax != len) ? 1 : 0. cmp rax, r10 ; setne al ; movzx eax, al.
    code.extend_from_slice(&[0x4C, 0x39, 0xD0]); // cmp rax, r10
    code.extend_from_slice(&[0x0F, 0x95, 0xC0]); // setne al
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
    Ok(())
}

/// Materialize `map_get(m, k) -> option<V>` into the aggregate words at
/// `base_slot`: word 0 = the option tag (`some`=0 / `none`=1), word 1 = the value
/// payload (for `some`). Deep-copy is NOT needed (read-only). Scans for `k`; when
/// found builds `some(value)` (loading the entry's value slot), else `none`,
/// reusing the native enum/option layout. `result_ty` is the call's `option<V>`
/// type, from which the `some`/`none` layout is resolved.
fn lower_map_get_into(
    ctx: &mut NativeCtx,
    base_slot: i32,
    result_ty: &TypeRef,
    map: &BytecodeExpr,
    key: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let (_k, value_ty) = supported_map_kv(&map.ty)
        .ok_or_else(|| format!("map_get expects a supported map but got `{}`", map.ty.name))?;
    let value_native = native_collection_slot(&value_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("map value `{}` is not layable-out", value_ty.name))?;
    let deep_value = native_slot_needs_deep_copy(&value_native);
    // Resolve the option layout to get the `some`/`none` tags (some=0, none=1).
    let layout = resolve_native_type(result_ty, ctx.structs, ctx.enums)?;
    let NativeType::Enum { variants, .. } = &layout else {
        return Err(format!(
            "map_get result type `{}` is not a supported option enum",
            result_ty.name
        ));
    };
    let some_tag = variants
        .iter()
        .find(|v| v.name == "some")
        .map(|v| v.tag)
        .ok_or_else(|| "map_get result option layout missing `some` variant".to_string())?;
    let none_tag = variants
        .iter()
        .find(|v| v.name == "none")
        .map(|v| v.tag)
        .ok_or_else(|| "map_get result option layout missing `none` variant".to_string())?;

    // Evaluate key, save; map into rcx; restore key into rdx; keep map across find.
    lower_map_key(ctx, key, code)?;
    code.push(0x50); // push rax (key)
    lower_native_expr(ctx, map, code)?; // rax = map pointer
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (map)
    code.push(0x5A); // pop rdx (key)
    // Both pushes are popped before the call; `__lullaby_map_find` preserves rcx
    // (map), so the pointer survives for the value load / `len` reload below.
    emit_call_symbol(ctx, MAP_FIND_SYMBOL, code); // rax = index or len (rcx=map, rdx=key)
    // r10 = len = [rcx + MAP_LEN_OFF]
    code.extend_from_slice(&[0x4C, 0x8B, 0x91]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // if rax == len -> none; else some(value).
    code.extend_from_slice(&[0x4C, 0x39, 0xD0]); // cmp rax, r10
    code.extend_from_slice(&[0x0F, 0x84]); // je none (rel32)
    let none_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // --- some branch: payload = entry(rax).value, then tag = some_tag ---
    // entry addr: r11 = rcx + MAP_DATA_OFF + rax*16.
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x04]); // shl rax, 4
    code.extend_from_slice(&[0x4C, 0x8D, 0x9C, 0x01]); // lea r11, [rcx + rax + disp32]
    code.extend_from_slice(&MAP_DATA_OFF.to_le_bytes());
    // rax = [r11 + MAP_VALUE_OFF]  (the value word); store it as the payload word.
    code.extend_from_slice(&[0x49, 0x8B, 0x83]); // mov rax, [r11 + disp32]
    code.extend_from_slice(&MAP_VALUE_OFF.to_le_bytes());
    // A MUTABLE-aggregate value is returned as an INDEPENDENT deep copy (the
    // interpreters' `values[i].clone()`), so mutating the retrieved `some` payload
    // never touches the map's entry.
    if deep_value {
        emit_heap_slot_deep_copy(ctx, &value_native, code);
    }
    store_local(code, base_slot + 8); // payload word (base_slot + 8)
    // tag word = some_tag at base_slot.
    emit_mov_rax_imm(code, some_tag);
    store_local(code, base_slot);
    code.push(0xE9); // jmp done (rel32)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // --- none branch: tag word = none_tag ---
    patch_rel32(code, none_site);
    emit_mov_rax_imm(code, none_tag);
    store_local(code, base_slot); // tag word (payload word left untouched)
    // done:
    patch_rel32(code, done_site);
    Ok(())
}

/// `parse_i64(s) -> result<i64, string>`: evaluate the string operand into `rcx`,
/// call the `__lullaby_parse_i64` helper (tag in `rax`, payload in `rdx`), and
/// store the two words into `base_slot` (tag) and `base_slot + 8` (payload). `ok`
/// is tag `0` with the parsed `i64` payload; `err` is tag `1` with a freshly-built
/// error-message string-record pointer payload, matching the `result<T, E>`
/// variant order (`ok` then `err`) the interpreters use.
fn lower_parse_i64_into(
    ctx: &mut NativeCtx,
    base_slot: i32,
    arg: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    if !is_string_type(&arg.ty) {
        return Err("parse_i64 expects a string argument".to_string());
    }
    lower_native_expr(ctx, arg, code)?; // rax = source string record pointer
    emit_mov_reg_reg(code, REG_RCX, REG_RAX); // rcx = source pointer (arg 0)
    emit_call_symbol(ctx, PARSE_I64_SYMBOL, code); // rax = tag, rdx = payload
    // Payload word at base_slot + 8: mov [rbp - (base_slot + 8)], rdx.
    code.extend_from_slice(&[0x48, 0x89, 0x95]);
    code.extend_from_slice(&(-(base_slot + 8)).to_le_bytes());
    // Tag word at base_slot (store_local writes rax, the tag).
    store_local(code, base_slot);
    Ok(())
}

/// `movq rax, xmm0` (f64) or `movd eax, xmm0` (f32, zero-extending the low four
/// bytes into rax) — move a float's bit pattern into `rax` so it can be stored as
/// a flat 8-byte list element word bit-for-bit.
fn emit_movq_rax_from_xmm0(code: &mut Vec<u8>, width: FloatWidth) {
    match width {
        // movq rax, xmm0 : 66 48 0F 7E C0
        FloatWidth::F64 => code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]),
        // movd eax, xmm0 : 66 0F 7E C0  (writing eax zero-extends into rax)
        FloatWidth::F32 => code.extend_from_slice(&[0x66, 0x0F, 0x7E, 0xC0]),
    }
}

/// `movq xmm0, rax` (f64) or `movd xmm0, eax` (f32) — move a raw list element
/// word's bit pattern from `rax` into `xmm0` at the element's float width, for a
/// float-element `get`.
fn emit_movq_xmm0_from_rax(code: &mut Vec<u8>, width: FloatWidth) {
    match width {
        // movq xmm0, rax : 66 48 0F 6E C0
        FloatWidth::F64 => code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]),
        // movd xmm0, eax : 66 0F 6E C0
        FloatWidth::F32 => code.extend_from_slice(&[0x66, 0x0F, 0x6E, 0xC0]),
    }
}

/// `mov rax, [rax + disp32]` — dereference a pointer in `rax` at a byte offset.
fn emit_mov_rax_from_rax_disp(code: &mut Vec<u8>, disp: i32) {
    code.extend_from_slice(&[0x48, 0x8B, 0x80]); // mov rax, [rax + disp32]
    code.extend_from_slice(&disp.to_le_bytes());
}

/// `mov r8, [rcx + disp32]`.
fn emit_mov_r8_from_rcx_disp(code: &mut Vec<u8>, disp: i32) {
    code.extend_from_slice(&[0x4C, 0x8B, 0x81]); // mov r8, [rcx + disp32]
    code.extend_from_slice(&disp.to_le_bytes());
}

/// `mov [rcx + disp32], r8`.
fn emit_mov_rcx_disp_from_r8(code: &mut Vec<u8>, disp: i32) {
    code.extend_from_slice(&[0x4C, 0x89, 0x81]); // mov [rcx + disp32], r8
    code.extend_from_slice(&disp.to_le_bytes());
}

/// `sub rax, imm32`.
fn emit_sub_rax_imm32(code: &mut Vec<u8>, imm: i32) {
    code.extend_from_slice(&[0x48, 0x2D]); // sub rax, imm32
    code.extend_from_slice(&imm.to_le_bytes());
}

/// `add rax, imm32`.
fn emit_add_rax_imm32(code: &mut Vec<u8>, imm: i32) {
    code.extend_from_slice(&[0x48, 0x05]); // add rax, imm32
    code.extend_from_slice(&imm.to_le_bytes());
}

/// `mov rcx, imm64` (10-byte form).
fn emit_mov_rcx_imm(code: &mut Vec<u8>, value: i64) {
    code.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
    code.extend_from_slice(&value.to_le_bytes());
}

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

// -- COFF object writer (multi-function, relocations, imports) ---------------
//
// The object has a single `.text` section holding the entry stub followed by
// every compiled function. External symbols name each function and the imported
// `ExitProcess`; REL32 relocations bind inter-function calls, the entry stub's
// call to `main`, and the entry stub's call to `ExitProcess`. Long symbol names
// (> 8 bytes) are stored in a string table, as COFF requires.

/// Write the COFF object for a native program. Programs that reference no string
/// constants keep the original single-`.text` layout byte-for-byte; programs
/// that do use string constants get the extended layout with `.rdata` (the
/// constants), `.bss` (the heap region + bump pointer), and the heap helper
/// functions. Splitting keeps the string-free path — and its structural tests —
/// unchanged.
fn write_native_program_object(
    functions: &[LoweredNativeFunction],
    strings: &StringPool,
    emit_stub: bool,
    debug: Option<&DebugOptions>,
) -> Vec<u8> {
    // The heap path (bump allocator + `.bss` region + helpers) is needed when the
    // program interns string constants OR references any growable-list/map or
    // string runtime helper. A program using none keeps the exact prior text-only
    // layout.
    if strings.is_empty() && !program_uses_heap_helpers(functions) {
        write_text_only_object(functions, emit_stub, debug)
    } else {
        write_object_with_data(functions, strings, emit_stub, debug)
    }
}

/// The ELF entry-point symbol. Linux's default linker entry is `_start`, so a
/// plain `ld`/`clang` link of the emitted object finds it without extra flags.
const ELF_ENTRY_SYMBOL: &str = "_start";

/// The Mach-O entry-point symbol. The macOS linker's default entry is `start`.
const MACHO_ENTRY_SYMBOL: &str = "start";

/// The freestanding platform ABI a non-Windows object targets: it selects the
/// entry-point symbol and the entry stub's process-exit mechanism (a raw `exit`
/// syscall) so the object needs no libc, mirroring the freestanding COFF path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlatformAbi {
    /// Linux System V: `_start`, `exit` via `syscall` rax=60.
    Linux,
    /// macOS: `start`, `exit` via `syscall` rax=0x2000001.
    MacOs,
}

impl PlatformAbi {
    /// The entry-point symbol name for this platform.
    fn entry_symbol(self) -> &'static str {
        match self {
            PlatformAbi::Linux => ELF_ENTRY_SYMBOL,
            PlatformAbi::MacOs => MACHO_ENTRY_SYMBOL,
        }
    }
}

/// A relocation carrying the target *symbol name*; resolved to a symbol index
/// once the neutral model's symbol table is assembled.
struct NamedTextReloc {
    offset: u64,
    symbol: String,
}

/// Build the target-neutral [`ObjectModel`] for a native program, used by the
/// ELF and Mach-O writers. It assembles exactly the same `.text` (functions +
/// heap/string helpers), `.rodata` (string constants), and `.bss` (bump heap)
/// content as the COFF path, but leads `.text` with a *freestanding* entry stub
/// that exits through a raw `exit` syscall instead of `kernel32!ExitProcess` —
/// the only machine-code difference between the platforms. The shared internal
/// calling convention is kept unchanged (see `documents/native_backend_contract.md`).
///
/// When `emit_stub` is true the object is a runnable program led by the entry
/// stub; when false it is a library object (no `main`) with no entry stub and no
/// exit dependency.
fn build_object_model(
    functions: &[LoweredNativeFunction],
    strings: &StringPool,
    emit_stub: bool,
    abi: PlatformAbi,
) -> ObjectModel {
    let use_heap = !strings.is_empty() || program_uses_heap_helpers(functions);

    // -- Assemble `.text`: entry stub, functions, then heap/string helpers ---
    let mut text: Vec<u8> = Vec::new();
    let mut relocations: Vec<NamedTextReloc> = Vec::new();

    if emit_stub {
        // Freestanding entry stub. `sub rsp, 32` reserves the internal ABI's
        // shadow space and lands `rsp` 16-byte aligned for the `call main`
        // (the OS enters `_start`/`start` with a 16-aligned stack). After `main`
        // returns its exit code in `eax`, the stub moves it to `edi` and issues
        // the platform `exit` syscall, so the object needs no libc.
        text.extend_from_slice(&[0x48, 0x83, 0xEC, 0x20]); // sub rsp, 32
        text.push(0xE8); // call main (rel32)
        relocations.push(NamedTextReloc {
            offset: text.len() as u64,
            symbol: "main".to_string(),
        });
        text.extend_from_slice(&[0, 0, 0, 0]);
        text.extend_from_slice(&[0x89, 0xC7]); // mov edi, eax (exit code)
        match abi {
            PlatformAbi::Linux => {
                // mov eax, 60 (SYS_exit); syscall.
                text.extend_from_slice(&[0xB8, 0x3C, 0x00, 0x00, 0x00]);
            }
            PlatformAbi::MacOs => {
                // mov eax, 0x2000001 (BSD `exit`); syscall.
                text.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x02]);
            }
        }
        text.extend_from_slice(&[0x0F, 0x05]); // syscall
        text.push(0xCC); // int3 (unreachable; exit does not return)
    }

    let mut func_offsets: HashMap<String, u64> = HashMap::new();
    let mut append_code = |text: &mut Vec<u8>,
                           relocations: &mut Vec<NamedTextReloc>,
                           name: &str,
                           code: &[u8],
                           relocs: &[CodeRelocation]| {
        while !text.len().is_multiple_of(16) {
            text.push(0xCC);
        }
        let start = text.len() as u64;
        func_offsets.insert(name.to_string(), start);
        for reloc in relocs {
            relocations.push(NamedTextReloc {
                offset: start + u64::from(reloc.offset),
                symbol: reloc.symbol.clone(),
            });
        }
        text.extend_from_slice(code);
    };

    for function in functions {
        append_code(
            &mut text,
            &mut relocations,
            &function.name,
            &function.code,
            &function.relocations,
        );
    }

    // The heap/string runtime helpers are emitted as one fixed set (identical to
    // the COFF data path) whenever the heap is used, so the symbol set matches.
    if use_heap {
        for helper in heap_runtime_helpers() {
            append_code(
                &mut text,
                &mut relocations,
                &helper.name,
                &helper.code,
                &helper.relocations,
            );
        }
    }

    // -- `.rodata`: NUL-terminated string constants --------------------------
    let mut rdata: Vec<u8> = Vec::new();
    let mut str_offsets: Vec<u64> = Vec::new();
    if use_heap {
        for text_value in &strings.entries {
            str_offsets.push(rdata.len() as u64);
            rdata.extend_from_slice(text_value.as_bytes());
            rdata.push(0);
        }
    }

    // -- Sections ------------------------------------------------------------
    // `.text` is section 0. `.rodata` (1) and `.bss` (2) follow when the heap is
    // used, matching the COFF data layout.
    let text_len = text.len() as u64;
    let mut sections: Vec<ObjectSection> = vec![ObjectSection {
        kind: ObjectSectionKind::Text,
        data: text,
        size: text_len,
        relocations: Vec::new(),
    }];
    let (rodata_section, bss_section) = if use_heap {
        let rdata_len = rdata.len() as u64;
        sections.push(ObjectSection {
            kind: ObjectSectionKind::ReadOnlyData,
            data: rdata,
            size: rdata_len,
            relocations: Vec::new(),
        });
        sections.push(ObjectSection {
            kind: ObjectSectionKind::Bss,
            data: Vec::new(),
            size: u64::from(16 + HEAP_REGION_SIZE),
            relocations: Vec::new(),
        });
        (Some(1usize), Some(2usize))
    } else {
        (None, None)
    };

    // -- Symbols (defined first, then undefined externs) ---------------------
    let mut symbols: Vec<ObjectSymbol> = Vec::new();
    if emit_stub {
        symbols.push(ObjectSymbol {
            name: abi.entry_symbol().to_string(),
            section: Some(0),
            value: 0,
            kind: ObjectSymbolKind::Function,
        });
    }
    for function in functions {
        symbols.push(ObjectSymbol {
            name: function.name.clone(),
            section: Some(0),
            value: func_offsets[&function.name],
            kind: ObjectSymbolKind::Function,
        });
    }
    if use_heap {
        for helper in heap_runtime_helpers() {
            symbols.push(ObjectSymbol {
                name: helper.name.clone(),
                section: Some(0),
                value: func_offsets[&helper.name],
                kind: ObjectSymbolKind::Function,
            });
        }
        for (index, offset) in str_offsets.iter().enumerate() {
            symbols.push(ObjectSymbol {
                name: format!("__str{index}"),
                section: rodata_section,
                value: *offset,
                kind: ObjectSymbolKind::Data,
            });
        }
        symbols.push(ObjectSymbol {
            name: HEAP_NEXT_SYMBOL.to_string(),
            section: bss_section,
            value: 0,
            kind: ObjectSymbolKind::Data,
        });
        symbols.push(ObjectSymbol {
            name: HEAP_FREE_HEAD_SYMBOL.to_string(),
            section: bss_section,
            value: 8,
            kind: ObjectSymbolKind::Data,
        });
        symbols.push(ObjectSymbol {
            name: HEAP_BASE_SYMBOL.to_string(),
            section: bss_section,
            value: 16,
            kind: ObjectSymbolKind::Data,
        });
    }
    // Undefined externals (an `extern fn` bound by the linker) come last so the
    // Mach-O `LC_DYSYMTAB` defined/undefined ranges stay contiguous.
    for reloc in &relocations {
        if !symbols.iter().any(|s| s.name == reloc.symbol) {
            symbols.push(ObjectSymbol {
                name: reloc.symbol.clone(),
                section: None,
                value: 0,
                kind: ObjectSymbolKind::Function,
            });
        }
    }

    // -- Resolve relocation symbol indices + classify branch vs data ---------
    let index_of = |name: &str| -> usize {
        symbols
            .iter()
            .position(|s| s.name == name)
            .expect("relocation symbol is defined")
    };
    let text_relocs: Vec<ObjectRelocation> = relocations
        .iter()
        .map(|reloc| {
            let symbol = index_of(&reloc.symbol);
            let kind = match symbols[symbol].kind {
                ObjectSymbolKind::Function => ObjectRelocationKind::Branch,
                ObjectSymbolKind::Data => ObjectRelocationKind::PcRel32,
            };
            ObjectRelocation {
                offset: reloc.offset,
                symbol,
                kind,
            }
        })
        .collect();
    sections[0].relocations = text_relocs;

    ObjectModel {
        sections,
        symbols,
        entry_symbol: emit_stub.then(|| abi.entry_symbol().to_string()),
        // This builder lowers x86-64 machine code (shared by the ELF and Mach-O
        // paths). The AArch64 ELF path has its own model builder in `aarch64`.
        machine: crate::object_model::ObjectMachine::X86_64,
    }
}

/// The fixed set of heap/string runtime helper functions emitted (in this order)
/// whenever a native program uses the heap. Shared by the COFF data path and the
/// neutral (ELF/Mach-O) model so all three object formats carry the same helper
/// symbol set.
fn heap_runtime_helpers() -> Vec<HelperFunction> {
    vec![
        emit_heap_alloc_helper(),
        emit_rc_free_helper(),
        emit_rc_dec_helper(),
        emit_drop_string_array_helper(),
        emit_heap_strlen_helper(),
        emit_list_new_helper(),
        emit_list_copy_helper(),
        emit_list_grow_helper(),
        emit_struct_copy_helper(),
        emit_map_new_helper(),
        emit_map_copy_helper(),
        emit_map_grow_helper(),
        emit_map_find_helper(),
        emit_str_lit_helper(),
        emit_str_concat_helper(),
        emit_str_concat_own_helper(),
        emit_str_from_int_helper(),
        emit_str_from_bool_helper(),
        emit_str_from_char_helper(),
        emit_str_substring_helper(),
        emit_str_char_at_helper(),
        emit_str_count_helper(),
        emit_str_repeat_helper(),
        emit_str_trim_helper(),
        emit_str_find_helper(),
        emit_str_contains_helper(),
        emit_str_starts_with_helper(),
        emit_str_ends_with_helper(),
        emit_str_split_helper(),
        emit_str_join_helper(),
        emit_parse_i64_helper(),
        emit_to_cstr_helper(),
    ]
}

/// Whether any lowered function references a runtime heap helper (a growable-list
/// or growable-map helper, or a string helper), which requires the heap sections +
/// helper `.text` even when the program interns no `.rdata` string constants (e.g.
/// a `to_string(i64)`-only program builds records without any literal).
fn program_uses_heap_helpers(functions: &[LoweredNativeFunction]) -> bool {
    functions.iter().any(|f| {
        f.relocations.iter().any(|r| {
            matches!(
                r.symbol.as_str(),
                LIST_NEW_SYMBOL
                    | LIST_COPY_SYMBOL
                    | LIST_GROW_SYMBOL
                    | STRUCT_COPY_SYMBOL
                    | MAP_NEW_SYMBOL
                    | MAP_COPY_SYMBOL
                    | MAP_GROW_SYMBOL
                    | MAP_FIND_SYMBOL
                    | STR_LIT_SYMBOL
                    | STR_CONCAT_SYMBOL
                    | STR_FROM_INT_SYMBOL
                    | STR_FROM_BOOL_SYMBOL
                    | STR_FROM_CHAR_SYMBOL
                    | STR_SUBSTRING_SYMBOL
                    | STR_CHAR_AT_SYMBOL
                    | STR_COUNT_SYMBOL
                    | STR_REPEAT_SYMBOL
                    | STR_TRIM_SYMBOL
                    | STR_FIND_SYMBOL
                    | STR_CONTAINS_SYMBOL
                    | STR_STARTS_WITH_SYMBOL
                    | STR_ENDS_WITH_SYMBOL
                    | STR_SPLIT_SYMBOL
                    | STR_JOIN_SYMBOL
                    | PARSE_I64_SYMBOL
                    | RC_DEC_SYMBOL
                    | DROP_STRING_ARRAY_SYMBOL
                    | TO_CSTR_SYMBOL
            )
        })
    })
}

/// Assemble the whole `.text` blob (entry stub + functions) and the section
/// relocations, then write the COFF headers, section data, symbol table, and
/// string table.
///
/// When `emit_stub` is true the object leads with the `_lullaby_start` entry stub
/// that calls `main` and forwards its result to `ExitProcess` (a runnable
/// program). When false, no stub is emitted and no `ExitProcess` dependency is
/// introduced: the object is a *library* whose exported functions a C `main` (or
/// another object) links against and calls. A string-free stubbed program keeps
/// its exact prior byte-for-byte layout.
fn write_text_only_object(
    functions: &[LoweredNativeFunction],
    emit_stub: bool,
    debug: Option<&DebugOptions>,
) -> Vec<u8> {
    // Lay out `.text`: entry stub first, then each function. Record each
    // function's start offset so relocations resolve.
    let mut text: Vec<u8> = Vec::new();
    let mut relocations: Vec<TextRelocation> = Vec::new();

    if emit_stub {
        // Entry stub: sub rsp, 40 (align + shadow); call main; mov ecx, eax;
        // call ExitProcess; (int3 padding). The `sub rsp,40` keeps rsp 16-aligned
        // at each `call` (return address makes 8; 40 = 0x28 restores alignment).
        text.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 40
        text.push(0xE8); // call main (rel32)
        relocations.push(TextRelocation {
            offset: (text.len()) as u32,
            symbol_index: 0, // filled after we know the symbol order (main is known)
            symbol_name: "main".to_string(),
        });
        text.extend_from_slice(&[0, 0, 0, 0]);
        text.extend_from_slice(&[0x89, 0xC1]); // mov ecx, eax (exit code = main's result)
        text.push(0xE8); // call ExitProcess (rel32)
        relocations.push(TextRelocation {
            offset: (text.len()) as u32,
            symbol_index: 0,
            symbol_name: EXIT_PROCESS_SYMBOL.to_string(),
        });
        text.extend_from_slice(&[0, 0, 0, 0]);
        text.push(0xCC); // int3 (unreachable; ExitProcess does not return)
    }

    // Each compiled function, remembering its start offset for symbol addresses.
    let mut func_offsets: HashMap<String, u32> = HashMap::new();
    for function in functions {
        // Align each function start to 16 bytes with int3 padding for tidy
        // disassembly (not required, but conventional).
        while !text.len().is_multiple_of(16) {
            text.push(0xCC);
        }
        let start = text.len() as u32;
        func_offsets.insert(function.name.clone(), start);
        let body_base = text.len();
        text.extend_from_slice(&function.code);
        // Translate each per-function relocation into a section relocation.
        for reloc in &function.relocations {
            relocations.push(TextRelocation {
                offset: body_base as u32 + reloc.offset,
                symbol_index: 0,
                symbol_name: reloc.symbol.clone(),
            });
        }
    }

    // Build the symbol table. Symbol 0 is the entry stub; then every function;
    // then the imported ExitProcess (undefined). Callers (relocations) reference
    // symbols by name, resolved to an index here.
    struct SymbolDef {
        name: String,
        section_number: i16, // 1 = .text, 0 = undefined (external import)
        value: u32,          // offset within the section
    }

    let mut symbols: Vec<SymbolDef> = Vec::new();
    if emit_stub {
        symbols.push(SymbolDef {
            name: NATIVE_ENTRY_SYMBOL.to_string(),
            section_number: 1,
            value: 0,
        });
    }
    for function in functions {
        symbols.push(SymbolDef {
            name: function.name.clone(),
            section_number: 1,
            value: *func_offsets.get(&function.name).expect("function offset"),
        });
    }
    if emit_stub {
        symbols.push(SymbolDef {
            name: EXIT_PROCESS_SYMBOL.to_string(),
            section_number: 0,
            value: 0,
        });
    }
    // Any relocation target not defined above is an undefined external symbol —
    // an `extern fn` C function bound by the linker (section 0), exactly like
    // `ExitProcess`. Add each such symbol once.
    for reloc in &relocations {
        if !symbols.iter().any(|s| s.name == reloc.symbol_name) {
            symbols.push(SymbolDef {
                name: reloc.symbol_name.clone(),
                section_number: 0,
                value: 0,
            });
        }
    }

    let symbol_index_of = |name: &str| -> u32 {
        symbols
            .iter()
            .position(|s| s.name == name)
            .expect("symbol exists") as u32
    };

    // Resolve relocation symbol indices now that the table is known.
    for reloc in &mut relocations {
        reloc.symbol_index = symbol_index_of(&reloc.symbol_name);
    }

    // -- Optional CodeView `.debug$S` line info -----------------------------
    // Built only when `--debug` is requested. Each user function contributes one
    // line record at its entry offset; helper/stub symbols carry no source line.
    let debug_built = debug.map(|options| {
        let entries: Vec<DebugFunctionLine<'_>> = functions
            .iter()
            .map(|function| DebugFunctionLine {
                symbol: &function.name,
                code_len: function.code.len() as u32,
                line: function.line,
            })
            .collect();
        let (data, relocs) = build_debug_section(&options.source_file, &entries);
        (data, relocs)
    });

    // -- Compute layout offsets ---------------------------------------------
    let num_relocs = relocations.len() as u32;
    let num_sections: u16 = if debug_built.is_some() { 2 } else { 1 };
    let headers_end = COFF_HEADER_SIZE + u32::from(num_sections) * SECTION_HEADER_SIZE;
    let raw_text_offset = headers_end;
    let debug_raw_offset = raw_text_offset + text.len() as u32;
    let debug_len = debug_built
        .as_ref()
        .map(|(data, _)| data.len() as u32)
        .unwrap_or(0);
    // `.text` relocations follow all raw section data; `.debug$S` relocations
    // follow the `.text` relocations.
    let reloc_table_offset = debug_raw_offset + debug_len;
    let num_debug_relocs = debug_built
        .as_ref()
        .map(|(_, relocs)| relocs.len() as u32)
        .unwrap_or(0);
    let debug_reloc_offset = reloc_table_offset + num_relocs * COFF_RELOC_SIZE;
    let symbol_table_offset = debug_reloc_offset + num_debug_relocs * COFF_RELOC_SIZE;
    let num_symbols = symbols.len() as u32;

    // -- Emit ----------------------------------------------------------------
    let mut bytes = Vec::new();

    // COFF header.
    push_u16(&mut bytes, AMD64_MACHINE);
    push_u16(&mut bytes, num_sections);
    push_u32(&mut bytes, 0); // timestamp
    push_u32(&mut bytes, symbol_table_offset);
    push_u32(&mut bytes, num_symbols);
    push_u16(&mut bytes, 0); // optional header size
    push_u16(&mut bytes, 0); // characteristics

    // Section header for `.text`.
    push_fixed_name(&mut bytes, ".text", 8);
    push_u32(&mut bytes, 0); // VirtualSize
    push_u32(&mut bytes, 0); // VirtualAddress
    push_u32(&mut bytes, text.len() as u32); // SizeOfRawData
    push_u32(&mut bytes, raw_text_offset); // PointerToRawData
    push_u32(
        &mut bytes,
        if num_relocs == 0 {
            0
        } else {
            reloc_table_offset
        },
    ); // PointerToRelocations
    push_u32(&mut bytes, 0); // PointerToLinenumbers
    push_u16(&mut bytes, num_relocs as u16); // NumberOfRelocations
    push_u16(&mut bytes, 0); // NumberOfLinenumbers
    push_u32(&mut bytes, TEXT_CHARACTERISTICS);

    // Section header for `.debug$S` (only when debug info is requested).
    if debug_built.is_some() {
        push_fixed_name(&mut bytes, ".debug$S", 8);
        push_u32(&mut bytes, 0); // VirtualSize
        push_u32(&mut bytes, 0); // VirtualAddress
        push_u32(&mut bytes, debug_len); // SizeOfRawData
        push_u32(&mut bytes, debug_raw_offset); // PointerToRawData
        push_u32(
            &mut bytes,
            if num_debug_relocs == 0 {
                0
            } else {
                debug_reloc_offset
            },
        ); // PointerToRelocations
        push_u32(&mut bytes, 0); // PointerToLinenumbers
        push_u16(&mut bytes, num_debug_relocs as u16); // NumberOfRelocations
        push_u16(&mut bytes, 0); // NumberOfLinenumbers
        push_u32(&mut bytes, DEBUG_S_CHARACTERISTICS);
    }

    // Section raw data: `.text`, then `.debug$S`.
    bytes.extend_from_slice(&text);
    if let Some((data, _)) = &debug_built {
        bytes.extend_from_slice(data);
    }

    // Relocation records: VirtualAddress (u32), SymbolTableIndex (u32), Type (u16).
    for reloc in &relocations {
        push_u32(&mut bytes, reloc.offset);
        push_u32(&mut bytes, reloc.symbol_index);
        push_u16(&mut bytes, IMAGE_REL_AMD64_REL32);
    }
    // `.debug$S` relocations reference the `.text` function symbols by index.
    if let Some((_, debug_relocs)) = &debug_built {
        for reloc in debug_relocs {
            push_u32(&mut bytes, reloc.offset);
            push_u32(&mut bytes, symbol_index_of(&reloc.symbol));
            push_u16(&mut bytes, reloc.reloc_type);
        }
    }

    // Symbol table + string table. Long names go to the string table, which is
    // appended immediately after the symbol records and begins with its own
    // 4-byte length field.
    let mut string_table: Vec<u8> = Vec::new();
    string_table.extend_from_slice(&[0, 0, 0, 0]); // placeholder for size
    for symbol in &symbols {
        if symbol.name.len() <= 8 {
            push_fixed_name(&mut bytes, &symbol.name, 8);
        } else {
            // Name field: 4 zero bytes then a 4-byte offset into the string table
            // (offset counts from the start of the string table, incl. the size).
            let offset = string_table.len() as u32;
            push_u32(&mut bytes, 0);
            push_u32(&mut bytes, offset);
            string_table.extend_from_slice(symbol.name.as_bytes());
            string_table.push(0);
        }
        push_u32(&mut bytes, symbol.value); // Value (section offset)
        push_u16(&mut bytes, section_number_field(symbol.section_number)); // SectionNumber
        push_u16(&mut bytes, 0x20); // Type: function
        bytes.push(2); // StorageClass: EXTERNAL
        bytes.push(0); // NumberOfAuxSymbols
    }

    // Patch and append the string table.
    let string_table_size = string_table.len() as u32;
    string_table[0..4].copy_from_slice(&string_table_size.to_le_bytes());
    bytes.extend_from_slice(&string_table);

    bytes
}

/// A relocation within the `.text` section.
struct TextRelocation {
    /// Byte offset of the 4-byte field within the section.
    offset: u32,
    /// Index into the symbol table.
    symbol_index: u32,
    /// Symbol name (resolved to `symbol_index` once the table is built).
    symbol_name: String,
}

/// Encode a signed COFF section number (`1` for `.text`, `0` for undefined) into
/// the unsigned 16-bit field.
fn section_number_field(section_number: i16) -> u16 {
    section_number as u16
}

const COFF_RELOC_SIZE: u32 = 10;

// ===========================================================================
// First heap step: `.rdata` string constants + `.bss` bump heap + helpers
// ===========================================================================
//
// When a program references string constants (`len("...")`), the object gains:
//   * `.rdata` — the NUL-terminated string bytes, each named `__str{i}`;
//   * `.bss`   — an 8-byte bump-pointer cell (`__lullaby_heap_next`) followed by
//                a fixed reserved heap region (`__lullaby_heap_base`);
//   * two helper functions in `.text` — `__lullaby_alloc` (a bump allocator) and
//     `__lullaby_strlen_copy` (allocate a heap copy of a `.rdata` string and
//     return its byte length by scanning the copy).
//
// This is the smallest end-to-end heap increment: a read-only constant, a REL32
// relocation to its address, a real bump allocation into a writable region, and
// per-byte reads of both `.rdata` and the heap — all observable through the i64
// `len` result and hence the process exit code.

/// A machine-code blob plus the symbols it references via REL32 relocations.
struct HelperFunction {
    name: String,
    code: Vec<u8>,
    relocations: Vec<CodeRelocation>,
}

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
fn emit_heap_alloc_helper() -> HelperFunction {
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
fn emit_rc_free_helper() -> HelperFunction {
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
fn emit_rc_dec_helper() -> HelperFunction {
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
fn emit_drop_string_array_helper() -> HelperFunction {
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
fn emit_heap_strlen_helper() -> HelperFunction {
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
fn emit_short_jmp_back(code: &mut Vec<u8>, target: usize) {
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
fn emit_list_word_copy_loop_rsi_rdi_rbx(code: &mut Vec<u8>) {
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
fn emit_helper_shadow_prologue(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 40
}
fn emit_helper_shadow_epilogue(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]); // add rsp, 40
}

/// `__lullaby_list_new() -> ptr in rax`: allocate a fresh empty list block with
/// `len = 0`, `cap = LIST_INITIAL_CAP`.
fn emit_list_new_helper() -> HelperFunction {
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
fn emit_list_copy_helper() -> HelperFunction {
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
fn emit_list_grow_helper() -> HelperFunction {
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
fn emit_struct_copy_helper() -> HelperFunction {
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
fn emit_map_new_helper() -> HelperFunction {
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
fn emit_map_copy_helper() -> HelperFunction {
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
fn emit_map_grow_helper() -> HelperFunction {
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
fn emit_map_find_helper() -> HelperFunction {
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
fn emit_helper_call_alloc(code: &mut Vec<u8>, relocations: &mut Vec<CodeRelocation>) {
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
fn emit_str_lit_helper() -> HelperFunction {
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
fn emit_to_cstr_helper() -> HelperFunction {
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
fn emit_str_concat_helper() -> HelperFunction {
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
fn emit_str_concat_own_helper() -> HelperFunction {
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

/// `__lullaby_str_from_int(rcx = value, rdx = signed_flag) -> rax = string record`.
///
/// Formats `value` in decimal. When `signed_flag` is nonzero the value is treated
/// as a signed `i64` (a leading `-` for a negative value, magnitude computed as a
/// `u64` so `i64::MIN` formats correctly); when zero it is an unsigned `u64`. Two
/// passes: pass 1 counts the digits (so the exact record size is known), pass 2
/// writes the digits backward directly into the freshly allocated heap record (no
/// stack buffer). `char_len == byte_len` (all ASCII). Matches the interpreters'
/// integer `Display`.
fn emit_str_from_int_helper() -> HelperFunction {
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
fn emit_str_from_bool_helper() -> HelperFunction {
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
fn emit_str_from_char_helper() -> HelperFunction {
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
fn emit_str_match_at_into_rax(code: &mut Vec<u8>) {
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
fn emit_str_byte_search(code: &mut Vec<u8>) {
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
fn emit_load_two_string_operands(code: &mut Vec<u8>) {
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
fn emit_str_find_helper() -> HelperFunction {
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
fn emit_str_contains_helper() -> HelperFunction {
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
fn emit_str_count_helper() -> HelperFunction {
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
fn emit_str_trim_helper() -> HelperFunction {
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
fn emit_str_repeat_helper() -> HelperFunction {
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
fn emit_str_starts_with_helper() -> HelperFunction {
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
fn emit_str_ends_with_helper() -> HelperFunction {
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
fn emit_parse_i64_helper() -> HelperFunction {
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
fn emit_helper_call(code: &mut Vec<u8>, relocations: &mut Vec<CodeRelocation>, symbol: &str) {
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
fn emit_str_split_helper() -> HelperFunction {
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
fn emit_str_join_helper() -> HelperFunction {
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
fn emit_char_to_byte_walk(code: &mut Vec<u8>) {
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
fn emit_str_char_at_helper() -> HelperFunction {
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
fn emit_str_substring_helper() -> HelperFunction {
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

/// Write the extended COFF object with `.text`, `.rdata`, and `.bss` sections.
/// Used only when the program references string constants.
fn write_object_with_data(
    functions: &[LoweredNativeFunction],
    strings: &StringPool,
    emit_stub: bool,
    debug: Option<&DebugOptions>,
) -> Vec<u8> {
    // -- Build .text: entry stub, user functions, heap helpers ---------------
    let mut text: Vec<u8> = Vec::new();
    let mut relocations: Vec<TextRelocation> = Vec::new();

    if emit_stub {
        // Entry stub (identical to the text-only path).
        text.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 40
        text.push(0xE8); // call main
        relocations.push(TextRelocation {
            offset: text.len() as u32,
            symbol_index: 0,
            symbol_name: "main".to_string(),
        });
        text.extend_from_slice(&[0, 0, 0, 0]);
        text.extend_from_slice(&[0x89, 0xC1]); // mov ecx, eax
        text.push(0xE8); // call ExitProcess
        relocations.push(TextRelocation {
            offset: text.len() as u32,
            symbol_index: 0,
            symbol_name: EXIT_PROCESS_SYMBOL.to_string(),
        });
        text.extend_from_slice(&[0, 0, 0, 0]);
        text.push(0xCC);
    }

    let mut func_offsets: HashMap<String, u32> = HashMap::new();

    // A closure-free helper to append a code blob with relocations, 16-aligned.
    let append_code = |text: &mut Vec<u8>,
                       relocations: &mut Vec<TextRelocation>,
                       func_offsets: &mut HashMap<String, u32>,
                       name: &str,
                       code: &[u8],
                       relocs: &[CodeRelocation]| {
        while !text.len().is_multiple_of(16) {
            text.push(0xCC);
        }
        let start = text.len() as u32;
        func_offsets.insert(name.to_string(), start);
        let body_base = text.len() as u32;
        text.extend_from_slice(code);
        for reloc in relocs {
            relocations.push(TextRelocation {
                offset: body_base + reloc.offset,
                symbol_index: 0,
                symbol_name: reloc.symbol.clone(),
            });
        }
    };

    for function in functions {
        append_code(
            &mut text,
            &mut relocations,
            &mut func_offsets,
            &function.name,
            &function.code,
            &function.relocations,
        );
    }
    let alloc = emit_heap_alloc_helper();
    append_code(
        &mut text,
        &mut relocations,
        &mut func_offsets,
        &alloc.name,
        &alloc.code,
        &alloc.relocations,
    );
    for rc_helper in [
        emit_rc_free_helper(),
        emit_rc_dec_helper(),
        emit_drop_string_array_helper(),
    ] {
        append_code(
            &mut text,
            &mut relocations,
            &mut func_offsets,
            &rc_helper.name,
            &rc_helper.code,
            &rc_helper.relocations,
        );
    }
    let strlen = emit_heap_strlen_helper();
    append_code(
        &mut text,
        &mut relocations,
        &mut func_offsets,
        &strlen.name,
        &strlen.code,
        &strlen.relocations,
    );
    // Growable-list runtime helpers (list_new / list_copy / list_grow). Emitted
    // unconditionally alongside the string helpers whenever the heap path runs; a
    // program that never references them simply carries three unused `.text`
    // functions (the linker's dead-strip removes them from the final image).
    for list_helper in [
        emit_list_new_helper(),
        emit_list_copy_helper(),
        emit_list_grow_helper(),
        emit_struct_copy_helper(),
    ] {
        append_code(
            &mut text,
            &mut relocations,
            &mut func_offsets,
            &list_helper.name,
            &list_helper.code,
            &list_helper.relocations,
        );
    }
    // Growable-map runtime helpers (map_new / map_copy / map_grow / map_find),
    // emitted alongside the list helpers whenever the heap path runs. A program
    // that never references them carries unused `.text` functions (dead-stripped
    // by the linker).
    for map_helper in [
        emit_map_new_helper(),
        emit_map_copy_helper(),
        emit_map_grow_helper(),
        emit_map_find_helper(),
    ] {
        append_code(
            &mut text,
            &mut relocations,
            &mut func_offsets,
            &map_helper.name,
            &map_helper.code,
            &map_helper.relocations,
        );
    }
    // String runtime helpers (str_lit / str_concat / str_from_int / str_from_bool
    // / str_from_char), emitted alongside the list/map helpers whenever the heap
    // path runs. A program that never references them carries unused `.text`
    // functions (dead-stripped by the linker).
    for str_helper in [
        emit_str_lit_helper(),
        emit_str_concat_helper(),
        emit_str_concat_own_helper(),
        emit_str_from_int_helper(),
        emit_str_from_bool_helper(),
        emit_str_from_char_helper(),
        emit_str_substring_helper(),
        emit_str_char_at_helper(),
        emit_str_count_helper(),
        emit_str_repeat_helper(),
        emit_str_trim_helper(),
        emit_str_find_helper(),
        emit_str_contains_helper(),
        emit_str_starts_with_helper(),
        emit_str_ends_with_helper(),
        emit_str_split_helper(),
        emit_str_join_helper(),
        emit_parse_i64_helper(),
        emit_to_cstr_helper(),
    ] {
        append_code(
            &mut text,
            &mut relocations,
            &mut func_offsets,
            &str_helper.name,
            &str_helper.code,
            &str_helper.relocations,
        );
    }

    // -- Build .rdata: NUL-terminated string constants -----------------------
    let mut rdata: Vec<u8> = Vec::new();
    let mut str_offsets: Vec<u32> = Vec::new();
    for text_value in &strings.entries {
        str_offsets.push(rdata.len() as u32);
        rdata.extend_from_slice(text_value.as_bytes());
        rdata.push(0);
    }

    // -- Symbol table --------------------------------------------------------
    // Sections: 1 = .text, 2 = .rdata, 3 = .bss.
    struct SymbolDef {
        name: String,
        section_number: i16,
        value: u32,
        is_function: bool,
    }

    let mut symbols: Vec<SymbolDef> = Vec::new();
    if emit_stub {
        symbols.push(SymbolDef {
            name: NATIVE_ENTRY_SYMBOL.to_string(),
            section_number: 1,
            value: 0,
            is_function: true,
        });
    }
    for function in functions {
        symbols.push(SymbolDef {
            name: function.name.clone(),
            section_number: 1,
            value: *func_offsets.get(&function.name).expect("function offset"),
            is_function: true,
        });
    }
    for helper in [
        HEAP_ALLOC_SYMBOL,
        RC_FREE_SYMBOL,
        RC_DEC_SYMBOL,
        DROP_STRING_ARRAY_SYMBOL,
        HEAP_STRLEN_SYMBOL,
        LIST_NEW_SYMBOL,
        LIST_COPY_SYMBOL,
        LIST_GROW_SYMBOL,
        STRUCT_COPY_SYMBOL,
        MAP_NEW_SYMBOL,
        MAP_COPY_SYMBOL,
        MAP_GROW_SYMBOL,
        MAP_FIND_SYMBOL,
        STR_LIT_SYMBOL,
        STR_CONCAT_SYMBOL,
        STR_CONCAT_OWN_SYMBOL,
        STR_FROM_INT_SYMBOL,
        STR_FROM_BOOL_SYMBOL,
        STR_FROM_CHAR_SYMBOL,
        STR_SUBSTRING_SYMBOL,
        STR_CHAR_AT_SYMBOL,
        STR_COUNT_SYMBOL,
        STR_REPEAT_SYMBOL,
        STR_TRIM_SYMBOL,
        STR_FIND_SYMBOL,
        STR_CONTAINS_SYMBOL,
        STR_STARTS_WITH_SYMBOL,
        STR_ENDS_WITH_SYMBOL,
        STR_SPLIT_SYMBOL,
        STR_JOIN_SYMBOL,
        PARSE_I64_SYMBOL,
        TO_CSTR_SYMBOL,
    ] {
        symbols.push(SymbolDef {
            name: helper.to_string(),
            section_number: 1,
            value: *func_offsets.get(helper).expect("helper offset"),
            is_function: true,
        });
    }
    if emit_stub {
        symbols.push(SymbolDef {
            name: EXIT_PROCESS_SYMBOL.to_string(),
            section_number: 0,
            value: 0,
            is_function: true,
        });
    }
    for (index, offset) in str_offsets.iter().enumerate() {
        symbols.push(SymbolDef {
            name: format!("__str{index}"),
            section_number: 2,
            value: *offset,
            is_function: false,
        });
    }
    // .bss: the bump pointer at offset 0, the free-list head at offset 8, the heap
    // region at offset 16.
    symbols.push(SymbolDef {
        name: HEAP_NEXT_SYMBOL.to_string(),
        section_number: 3,
        value: 0,
        is_function: false,
    });
    symbols.push(SymbolDef {
        name: HEAP_FREE_HEAD_SYMBOL.to_string(),
        section_number: 3,
        value: 8,
        is_function: false,
    });
    symbols.push(SymbolDef {
        name: HEAP_BASE_SYMBOL.to_string(),
        section_number: 3,
        value: 16,
        is_function: false,
    });
    // Undefined external symbols for any unresolved relocation target — the
    // `extern fn` C functions bound by the linker (section 0), like `ExitProcess`.
    for reloc in &relocations {
        if !symbols.iter().any(|s| s.name == reloc.symbol_name) {
            symbols.push(SymbolDef {
                name: reloc.symbol_name.clone(),
                section_number: 0,
                value: 0,
                is_function: true,
            });
        }
    }

    let symbol_index_of = |name: &str| -> u32 {
        symbols
            .iter()
            .position(|s| s.name == name)
            .expect("symbol exists") as u32
    };
    for reloc in &mut relocations {
        reloc.symbol_index = symbol_index_of(&reloc.symbol_name);
    }

    // -- Optional CodeView `.debug$S` line info -----------------------------
    let debug_built = debug.map(|options| {
        let entries: Vec<DebugFunctionLine<'_>> = functions
            .iter()
            .map(|function| DebugFunctionLine {
                symbol: &function.name,
                code_len: function.code.len() as u32,
                line: function.line,
            })
            .collect();
        build_debug_section(&options.source_file, &entries)
    });

    // -- Section layout ------------------------------------------------------
    // Sections: .text, .rdata, .bss, and (with --debug) .debug$S.
    let num_sections: u32 = if debug_built.is_some() { 4 } else { 3 };
    let bss_size = 16 + HEAP_REGION_SIZE;
    let num_relocs = relocations.len() as u32;

    let headers_end = COFF_HEADER_SIZE + num_sections * SECTION_HEADER_SIZE;
    let text_raw = headers_end;
    let rdata_raw = text_raw + text.len() as u32;
    // `.debug$S` raw data (if any) follows `.rdata`; `.bss` has no raw data.
    let debug_raw_offset = rdata_raw + rdata.len() as u32;
    let debug_len = debug_built
        .as_ref()
        .map(|(data, _)| data.len() as u32)
        .unwrap_or(0);
    // Relocations follow the raw section data: `.text` relocs, then `.debug$S`.
    let reloc_table_offset = debug_raw_offset + debug_len;
    let num_debug_relocs = debug_built
        .as_ref()
        .map(|(_, relocs)| relocs.len() as u32)
        .unwrap_or(0);
    let debug_reloc_offset = reloc_table_offset + num_relocs * COFF_RELOC_SIZE;
    let symbol_table_offset = debug_reloc_offset + num_debug_relocs * COFF_RELOC_SIZE;
    let num_symbols = symbols.len() as u32;

    // -- Emit ----------------------------------------------------------------
    let mut bytes = Vec::new();

    // COFF header.
    push_u16(&mut bytes, AMD64_MACHINE);
    push_u16(&mut bytes, num_sections as u16);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, symbol_table_offset);
    push_u32(&mut bytes, num_symbols);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);

    // .text section header.
    push_fixed_name(&mut bytes, ".text", 8);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, text.len() as u32);
    push_u32(&mut bytes, text_raw);
    push_u32(
        &mut bytes,
        if num_relocs == 0 {
            0
        } else {
            reloc_table_offset
        },
    );
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, num_relocs as u16);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, TEXT_CHARACTERISTICS);

    // .rdata section header.
    push_fixed_name(&mut bytes, ".rdata", 8);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, rdata.len() as u32);
    push_u32(&mut bytes, rdata_raw);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, RDATA_CHARACTERISTICS);

    // .bss section header. In a COFF *object*, an uninitialized-data section
    // carries its size in SizeOfRawData with PointerToRawData = 0 (there is no
    // raw data on disk); VirtualSize is 0. `IMAGE_SCN_CNT_UNINITIALIZED_DATA`
    // tells the linker to reserve zeroed space.
    push_fixed_name(&mut bytes, ".bss", 8);
    push_u32(&mut bytes, 0); // VirtualSize (0 for object files)
    push_u32(&mut bytes, 0); // VirtualAddress
    push_u32(&mut bytes, bss_size); // SizeOfRawData (reserved zeroed bytes)
    push_u32(&mut bytes, 0); // PointerToRawData (none for uninitialized data)
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, BSS_CHARACTERISTICS);

    // .debug$S section header (only when debug info is requested).
    if debug_built.is_some() {
        push_fixed_name(&mut bytes, ".debug$S", 8);
        push_u32(&mut bytes, 0); // VirtualSize
        push_u32(&mut bytes, 0); // VirtualAddress
        push_u32(&mut bytes, debug_len); // SizeOfRawData
        push_u32(&mut bytes, debug_raw_offset); // PointerToRawData
        push_u32(
            &mut bytes,
            if num_debug_relocs == 0 {
                0
            } else {
                debug_reloc_offset
            },
        ); // PointerToRelocations
        push_u32(&mut bytes, 0); // PointerToLinenumbers
        push_u16(&mut bytes, num_debug_relocs as u16); // NumberOfRelocations
        push_u16(&mut bytes, 0); // NumberOfLinenumbers
        push_u32(&mut bytes, DEBUG_S_CHARACTERISTICS);
    }

    // Section raw data: .text, .rdata, then .debug$S (.bss has none).
    bytes.extend_from_slice(&text);
    bytes.extend_from_slice(&rdata);
    if let Some((data, _)) = &debug_built {
        bytes.extend_from_slice(data);
    }

    // Relocations: .text relocations, then .debug$S relocations.
    for reloc in &relocations {
        push_u32(&mut bytes, reloc.offset);
        push_u32(&mut bytes, reloc.symbol_index);
        push_u16(&mut bytes, IMAGE_REL_AMD64_REL32);
    }
    if let Some((_, debug_relocs)) = &debug_built {
        for reloc in debug_relocs {
            push_u32(&mut bytes, reloc.offset);
            push_u32(&mut bytes, symbol_index_of(&reloc.symbol));
            push_u16(&mut bytes, reloc.reloc_type);
        }
    }

    // Symbol table + string table.
    let mut string_table: Vec<u8> = Vec::new();
    string_table.extend_from_slice(&[0, 0, 0, 0]);
    for symbol in &symbols {
        if symbol.name.len() <= 8 {
            push_fixed_name(&mut bytes, &symbol.name, 8);
        } else {
            let offset = string_table.len() as u32;
            push_u32(&mut bytes, 0);
            push_u32(&mut bytes, offset);
            string_table.extend_from_slice(symbol.name.as_bytes());
            string_table.push(0);
        }
        push_u32(&mut bytes, symbol.value);
        push_u16(&mut bytes, section_number_field(symbol.section_number));
        push_u16(&mut bytes, if symbol.is_function { 0x20 } else { 0x00 });
        bytes.push(2); // StorageClass: EXTERNAL
        bytes.push(0); // NumberOfAuxSymbols
    }

    let string_table_size = string_table.len() as u32;
    string_table[0..4].copy_from_slice(&string_table_size.to_le_bytes());
    bytes.extend_from_slice(&string_table);

    bytes
}

// ===========================================================================
// CodeView source-line debug info (`.debug$S`, `lullaby native --debug`)
// ===========================================================================
//
// When `--debug` is requested, the object gains a CodeView `.debug$S` section
// carrying a per-function line-number table that maps each compiled function's
// entry code offset to its `.lby` source declaration line, plus the source file
// name. rust-lld/link.exe fold `.debug$S` into a PDB; a debugger (or
// `llvm-pdbutil`) can then place a breakpoint at a function and show its source
// line. Line granularity is **per function** for this increment: one line record
// at each function's entry offset (its declaration line). Finer per-statement
// mapping is deferred (see the native backend contract).
//
// The `.debug$S` stream is: a C13 signature, a `DEBUG_S_SYMBOLS` subsection with
// a minimal `S_COMPILE3`, one `DEBUG_S_LINES` subsection per function (whose
// header's function-offset/segment fields are patched by SECREL/SECTION
// relocations against that function's `.text` symbol), a `DEBUG_S_FILECHKSMS`
// table with one file entry, and a `DEBUG_S_STRINGTABLE` holding the source file
// name. Emission is fully additive: without `--debug` no `.debug$S` section is
// produced and the object bytes are byte-for-byte unchanged.

/// A relocation the `.debug$S` section needs against a `.text` function symbol:
/// the `DEBUG_S_LINES` header's function-offset (`SECREL32`) and segment
/// (`SECTION`) fields must be fixed up by the linker.
struct DebugReloc {
    /// Byte offset of the 4-byte field within the `.debug$S` section.
    offset: u32,
    /// The `.text` function symbol referenced.
    symbol: String,
    /// COFF relocation type (`SECREL32` or `SECTION`).
    reloc_type: u16,
}

/// One compiled function's debug line entry: the `.text` symbol name, its code
/// length in bytes, and its 1-based source declaration line.
struct DebugFunctionLine<'a> {
    symbol: &'a str,
    code_len: u32,
    line: u32,
}

/// Build the CodeView `.debug$S` section bytes plus the relocations it needs
/// against the `.text` function symbols. `source_file` is recorded as the source
/// file name; `functions` provides one entry per compiled function.
fn build_debug_section(
    source_file: &str,
    functions: &[DebugFunctionLine<'_>],
) -> (Vec<u8>, Vec<DebugReloc>) {
    let mut relocs: Vec<DebugReloc> = Vec::new();
    let mut section: Vec<u8> = Vec::new();
    push_u32(&mut section, CV_SIGNATURE_C13);

    // -- DEBUG_S_STRINGTABLE contents (built first so file/line subsections can
    //    reference the source-file name by its offset). The table starts with a
    //    zero byte (offset 0 is the empty string), then each NUL-terminated name.
    let mut strtab: Vec<u8> = Vec::new();
    strtab.push(0);
    let source_name_offset = strtab.len() as u32;
    strtab.extend_from_slice(source_file.as_bytes());
    strtab.push(0);

    // -- DEBUG_S_FILECHKSMS contents. One 8-byte file entry: u32 offset into the
    //    string table, u8 checksum-byte-count (0 = no checksum), u8 checksum kind
    //    (0 = None), then 2 pad bytes to a 4-byte boundary. The byte offset of
    //    this entry within the table (0) is what DEBUG_S_LINES references.
    let mut filechksms: Vec<u8> = Vec::new();
    push_u32(&mut filechksms, source_name_offset);
    filechksms.push(0); // checksum size
    filechksms.push(0); // checksum kind: None
    filechksms.push(0); // pad
    filechksms.push(0); // pad
    let file_entry_offset: u32 = 0;

    // -- DEBUG_S_SYMBOLS: a minimal S_COMPILE3 so the stream is a well-formed
    //    CodeView symbol subsection. Record body: flags(u32)=0, machine(u16)=
    //    CV_CFL_X64(0xD0), front-end + back-end version words (all 0), then a
    //    NUL-terminated compiler version name. Each CV symbol record is prefixed
    //    by a u16 length that counts everything after the length field.
    {
        let mut sym: Vec<u8> = Vec::new();
        let mut record: Vec<u8> = Vec::new();
        push_u16(&mut record, S_COMPILE3);
        push_u32(&mut record, 0); // flags + language (Language=0, no flags)
        push_u16(&mut record, 0x00D0); // Machine: CV_CFL_X64
        for _ in 0..8 {
            push_u16(&mut record, 0); // FE/BE major/minor/build/QFE
        }
        record.extend_from_slice(b"lullaby native");
        record.push(0);
        // Record length prefix: the count of bytes after the u16 length field.
        push_u16(&mut sym, record.len() as u16);
        sym.extend_from_slice(&record);
        push_subsection(&mut section, DEBUG_S_SYMBOLS, &sym);
    }

    // -- DEBUG_S_LINES, one subsection per function. The header's first two fields
    //    (function offset, then segment) are relocated against the `.text` symbol.
    for function in functions {
        let sub_data_start = section.len() + 8; // after the subsection kind+length
        let mut lines: Vec<u8> = Vec::new();
        // Subsection header: offset(u32, SECREL32), segment(u16, SECTION),
        // flags(u16)=0, code size(u32).
        let off_field = lines.len();
        push_u32(&mut lines, 0); // offset (patched via SECREL32)
        let seg_field = lines.len();
        push_u16(&mut lines, 0); // segment (patched via SECTION)
        push_u16(&mut lines, 0); // flags
        push_u32(&mut lines, function.code_len); // code size covered

        // One file block: file offset into FILECHKSMS, line count, block size.
        push_u32(&mut lines, file_entry_offset);
        push_u32(&mut lines, 1); // one line entry
        // Block byte size: header(12) + one line pair(8) = 20.
        push_u32(&mut lines, 12 + 8);
        // Line entry: code offset within the function (0 = its entry), then the
        // packed line number. Bit 31 marks a statement (is-statement) line.
        push_u32(&mut lines, 0);
        push_u32(&mut lines, function.line | 0x8000_0000);

        // Record the two header relocations (offsets are relative to the section).
        relocs.push(DebugReloc {
            offset: (sub_data_start + off_field) as u32,
            symbol: function.symbol.to_string(),
            reloc_type: IMAGE_REL_AMD64_SECREL,
        });
        relocs.push(DebugReloc {
            offset: (sub_data_start + seg_field) as u32,
            symbol: function.symbol.to_string(),
            reloc_type: IMAGE_REL_AMD64_SECTION,
        });

        push_subsection(&mut section, DEBUG_S_LINES, &lines);
    }

    push_subsection(&mut section, DEBUG_S_FILECHKSMS, &filechksms);
    push_subsection(&mut section, DEBUG_S_STRINGTABLE, &strtab);

    (section, relocs)
}

/// Append a CodeView subsection (`u32 kind`, `u32 length`, then `data` padded to
/// a 4-byte boundary) to `section`. The caller computes any relocation offsets
/// before appending, since the LINES header field positions must be known
/// precisely to place `SECREL32`/`SECTION` relocations.
fn push_subsection(section: &mut Vec<u8>, kind: u32, data: &[u8]) {
    push_u32(section, kind);
    push_u32(section, data.len() as u32);
    section.extend_from_slice(data);
    while !section.len().is_multiple_of(4) {
        section.push(0);
    }
}

#[cfg(test)]
#[path = "native_object_coff_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "native_program_tests.rs"]
mod native_program_tests;
