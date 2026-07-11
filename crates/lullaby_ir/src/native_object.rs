use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use lullaby_parser::{AssignOp, BinaryOp, TypeRef};

use crate::native_contract::{
    NativeArchitecture, NativeObjectFormat, NativeTarget, alpha1_native_backend_contract,
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

pub fn emit_alpha1_coff_object(
    module: &BytecodeModule,
) -> Result<NativeObjectFile, NativeObjectError> {
    let contract = alpha1_native_backend_contract();
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
                    AssignOp::Replace | AssignOp::Divide => unreachable!(),
                }
            }
            AssignOp::Divide => {
                return self.unsupported(
                    "prototype emitter does not support native i64 division assignment".to_string(),
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
// The prototype `emit_alpha1_coff_object` above lowers a single literal-return
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

/// The heap region base symbol in `.bss` — a fixed reserved bump region.
const HEAP_BASE_SYMBOL: &str = "__lullaby_heap_base";

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
/// [`emit_alpha1_native_program_with_debug`] to additionally emit CodeView
/// source-line debug info.
pub fn emit_alpha1_native_program(
    module: &BytecodeModule,
) -> Result<NativeProgram, NativeProgramError> {
    emit_alpha1_native_program_with_debug(module, None)
}

/// Like [`emit_alpha1_native_program`], but when `debug` is `Some`, additionally
/// emits a CodeView `.debug$S` section with per-function source-line info (see
/// [`DebugOptions`]). When `debug` is `None` the emitted object bytes are exactly
/// those of [`emit_alpha1_native_program`].
pub fn emit_alpha1_native_program_with_debug(
    module: &BytecodeModule,
    debug: Option<&DebugOptions>,
) -> Result<NativeProgram, NativeProgramError> {
    emit_alpha1_native_program_for_target(module, &x86_64_windows_target(), debug)
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
pub fn emit_alpha1_native_program_for_target(
    module: &BytecodeModule,
    target: &NativeTarget,
    debug: Option<&DebugOptions>,
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

    // Return array: length taken from the function's returned array value(s).
    if function.return_type.name.starts_with("array<") {
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
        if !param.ty.name.starts_with("array<") {
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
                // Float fields inside aggregates are out of scope: the aggregate
                // load/store paths move whole 8-byte words through a GPR, which
                // would not keep an f32 field rounded. Reject so the function
                // skips gracefully rather than miscompiling.
                if matches!(native, NativeType::F64 | NativeType::F32) {
                    return Err(format!(
                        "struct `{name}` field `{field_name}` is a float; float struct fields are not in the native subset"
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
        })
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
    /// dynamic array; and the runtime index expression selects the element.
    Dynamic {
        base_slot: i32,
        const_words: i64,
        elem_words: i64,
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
fn lower_native_function(
    function: &BytecodeFunction,
    callable: &std::collections::HashSet<&str>,
    extern_sigs: &HashMap<&str, &crate::IrExternSignature>,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    strings: &mut StringPool,
    signatures: &HashMap<String, NativeSignature>,
    array_lengths: &ArrayLengths,
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
    let mut code = Vec::new();

    // Prologue: push rbp; mov rbp, rsp; sub rsp, frame_size.
    code.extend_from_slice(&[0x55, 0x48, 0x89, 0xE5]);
    emit_sub_rsp(&mut code, ctx.frame_size);

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
                if on_stack {
                    emit_mov_rax_from_rbp_pos(&mut code, stack_disp);
                    store_local(&mut code, local.slot);
                } else {
                    code.extend_from_slice(PARAM_STORE[reg]);
                    code.extend_from_slice(&(-local.slot).to_le_bytes());
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
    if tail_is_asm {
        let (head, tail) = instructions.split_at(instructions.len() - 1);
        lower_native_stmts(&mut ctx, head, &mut code, &mut loops)?;
        if let BytecodeInstruction::Asm { bytes, .. } = &tail[0] {
            code.extend_from_slice(bytes);
        }
        emit_native_epilogue(&mut code, ctx.frame_size);
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
        emit_native_epilogue(&mut code, ctx.frame_size);
    } else if tail_is_value_match {
        let (head, tail) = instructions.split_at(instructions.len() - 1);
        lower_native_stmts(&mut ctx, head, &mut code, &mut loops)?;
        if let BytecodeInstruction::Match {
            scrutinee, arms, ..
        } = &tail[0]
        {
            lower_native_match(&mut ctx, scrutinee, arms, true, &mut code, &mut loops)?;
        }
        emit_native_epilogue(&mut code, ctx.frame_size);
    } else {
        lower_native_stmts(&mut ctx, instructions, &mut code, &mut loops)?;
    }

    // Fallthrough epilogue: functions in this subset are non-void and expected to
    // return on every path, but emit a safe `xor eax,eax` + epilogue so a missing
    // tail return cannot run off the end of the section.
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax
    emit_native_epilogue(&mut code, ctx.frame_size);

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

/// Emit `add rsp, imm; pop rbp; ret`.
fn emit_native_epilogue(code: &mut Vec<u8>, frame_size: i32) {
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
                    store_local(code, slot); // mov [rbp - slot], rax
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
                            AssignOp::Replace => unreachable!(),
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
            let place = resolve_scalar_place(ctx, name, path)?;
            match op {
                AssignOp::Replace => {
                    // Evaluate the RHS, then store into the resolved scalar slot.
                    match place {
                        ScalarPlace::Const { slot } => {
                            // `x = x + rhs` / `x = x - rhs`, where the assigned
                            // local is also the left operand, folds into a
                            // memory-destination `add`/`sub [rbp-slot], …`,
                            // skipping the load of the target and the store back
                            // (the dominant per-iteration cost in a counting loop).
                            // Plain i64 only — fixed-width kinds need width
                            // re-normalization, floats/aggregates are handled
                            // above — and only when the left operand resolves to
                            // this exact slot. `add`/`sub` on memory keep the low
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
                                let disp = (-slot).to_le_bytes();
                                if let BytecodeExprKind::Integer(rhs) = &right.kind
                                    && let Ok(imm) = i32::try_from(*rhs)
                                {
                                    // add/sub qword ptr [rbp-slot], imm32
                                    let modrm = if matches!(bop, BinaryOp::Add) {
                                        0x85
                                    } else {
                                        0xAD
                                    };
                                    code.extend_from_slice(&[0x48, 0x81, modrm]);
                                    code.extend_from_slice(&disp);
                                    code.extend_from_slice(&imm.to_le_bytes());
                                } else {
                                    lower_native_expr(ctx, right, code)?; // rhs → rax
                                    // add/sub qword ptr [rbp-slot], rax
                                    let opcode = if matches!(bop, BinaryOp::Add) {
                                        0x01
                                    } else {
                                        0x29
                                    };
                                    code.extend_from_slice(&[0x48, opcode, 0x85]);
                                    code.extend_from_slice(&disp);
                                }
                                return Ok(());
                            }
                            lower_native_expr(ctx, value, code)?;
                            store_local(code, slot);
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
                        AssignOp::Replace => unreachable!(),
                    };
                    match place {
                        ScalarPlace::Const { slot } => {
                            load_local(code, slot); // rax = current
                            code.push(0x50); // push rax (left)
                            lower_native_expr(ctx, value, code)?; // rax = right
                            emit_i64_binop_from_stack(code, bin)?;
                            store_local(code, slot);
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
            emit_native_epilogue(code, ctx.frame_size);
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
    let local = ctx.local(root)?;
    let base_slot = local.slot;
    let mut ty = local.ty.clone();
    let mut const_words: i64 = 0;
    let mut dynamic: Option<(i64, BytecodeExpr)> = None;

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
            (PathStep::Index(index), NativeType::Array { elem, .. }) => {
                let stride = elem.words() as i64;
                if let BytecodeExprKind::Integer(literal) = index.kind {
                    const_words += literal * stride;
                } else if dynamic.is_none() {
                    dynamic = Some((stride, (*index).clone()));
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

    if ty != NativeType::I64 {
        return Err("native access must resolve to an i64 scalar".to_string());
    }

    match dynamic {
        None => Ok(ScalarPlace::Const {
            slot: base_slot + const_words as i32 * 8,
        }),
        Some((elem_words, index)) => Ok(ScalarPlace::Dynamic {
            base_slot,
            const_words,
            elem_words,
            index,
        }),
    }
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
        index,
    } = place
    else {
        return Err("expected a dynamic place".to_string());
    };
    // rax = index
    lower_native_expr(ctx, index, code)?;
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
        lower_native_expr(ctx, left, code)?;
        emit_cmp_rax_imm(code, *rhs); // cmp rax(left), right
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
fn lower_native_while(
    ctx: &mut NativeCtx,
    condition: &BytecodeExpr,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
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
    let loop_ctx = loops.pop().expect("loop pushed");

    emit_jmp_to(code, top);

    let end = code.len();
    for site in loop_ctx.break_sites {
        patch_rel32_to(code, site, end);
    }
    Ok(())
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
    // The counter and its two hidden slots (bound, step) were reserved during
    // frame planning, keyed by the counter name.
    let i_slot = ctx.local_slot(name)?;
    let end_slot = ctx.local_slot(&format!("{name}__end"))?;
    let step_slot = ctx.local_slot(&format!("{name}__step"))?;

    // i = start
    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
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
    emit_for_compare(code, i_slot, end_slot, 0x9E);
    code.push(0xE9); // jmp check
    let asc_done = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // Descending: cond = (i >= end)  ->  setge al
    patch_rel32(code, js_site);
    emit_for_compare(code, i_slot, end_slot, 0x9D);

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
    let loop_ctx = loops.pop().expect("loop pushed");

    // Step block (target of `continue`): i += step.
    let step_label = code.len();
    for site in loop_ctx.continue_sites {
        patch_rel32_to(code, site, step_label);
    }
    load_local(code, i_slot); // mov rax, [i]
    code.push(0x50); // push rax
    load_local(code, step_slot); // mov rax, [step]
    emit_i64_binop_from_stack(code, BinaryOp::Add)?;
    store_local(code, i_slot);

    emit_jmp_to(code, top);

    let end = code.len();
    patch_rel32_to(code, exit_site, end);
    for site in loop_ctx.break_sites {
        patch_rel32_to(code, site, end);
    }
    Ok(())
}

/// Emit `mov rax, [i]; cmp rax, [end]; set<cc> al` where `set_opcode` is the
/// second byte of the `0F` `setcc` form (e.g. `0x9E` = setle, `0x9D` = setge).
fn emit_for_compare(code: &mut Vec<u8>, i_slot: i32, end_slot: i32, set_opcode: u8) {
    load_local(code, i_slot); // mov rax, [rbp - i_slot]
    // cmp rax, [rbp - end_slot]  ->  48 3B 85 disp32
    code.extend_from_slice(&[0x48, 0x3B, 0x85]);
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    // set<cc> al
    code.extend_from_slice(&[0x0F, set_opcode, 0xC0]);
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
            load_local(code, slot);
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
                    BinaryOp::Add => Some(&[0x48, 0x05]),        // add rax, imm32
                    BinaryOp::Subtract => Some(&[0x48, 0x2D]),   // sub rax, imm32
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
        // Integer bitwise operators are deferred on the native backend; a
        // function using them is skipped and still runs on the interpreters.
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Shl | BinaryOp::Shr => {
            return Err("bitwise operators are not supported on the native backend".to_string());
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
    emit_call_symbol(ctx, STR_CONCAT_SYMBOL, code);
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
            size: u64::from(8 + HEAP_REGION_SIZE),
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
            name: HEAP_BASE_SYMBOL.to_string(),
            section: bss_section,
            value: 8,
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
        emit_str_from_int_helper(),
        emit_str_from_bool_helper(),
        emit_str_from_char_helper(),
        emit_str_substring_helper(),
        emit_str_find_helper(),
        emit_str_contains_helper(),
        emit_str_starts_with_helper(),
        emit_str_ends_with_helper(),
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
                    | STR_FIND_SYMBOL
                    | STR_CONTAINS_SYMBOL
                    | STR_STARTS_WITH_SYMBOL
                    | STR_ENDS_WITH_SYMBOL
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

/// Emit the bump allocator `__lullaby_alloc(size in rcx) -> ptr in rax`.
///
/// Reads the bump pointer from `.bss`; if it is still zero (first call) it seeds
/// it to the base of the reserved heap region. Returns the current pointer and
/// advances it past an 8-byte-rounded allocation.
fn emit_heap_alloc_helper() -> HelperFunction {
    let mut code: Vec<u8> = Vec::new();
    let mut relocations: Vec<CodeRelocation> = Vec::new();

    // mov rax, [rip + __lullaby_heap_next]
    code.extend_from_slice(&[0x48, 0x8B, 0x05]);
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_NEXT_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    // test rax, rax
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jnz +7 (skip the lea that seeds the pointer)
    code.extend_from_slice(&[0x75, 0x07]);
    // lea rax, [rip + __lullaby_heap_base]  (7 bytes)
    code.extend_from_slice(&[0x48, 0x8D, 0x05]);
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_BASE_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rdx = rax (the pointer we will return)
    code.extend_from_slice(&[0x48, 0x89, 0xC2]);
    // rax = rax + rcx (advance past the requested size)
    code.extend_from_slice(&[0x48, 0x01, 0xC8]);
    // rax = (rax + 7) & ~7  (round the new next up to 8 bytes)
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x07]);
    code.extend_from_slice(&[0x48, 0x83, 0xE0, 0xF8]);
    // mov [rip + __lullaby_heap_next], rax
    code.extend_from_slice(&[0x48, 0x89, 0x05]);
    relocations.push(CodeRelocation {
        offset: code.len() as u32,
        symbol: HEAP_NEXT_SYMBOL.to_string(),
    });
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rax = rdx (return the pre-advance pointer)
    code.extend_from_slice(&[0x48, 0x89, 0xD0]);
    // ret
    code.push(0xC3);

    HelperFunction {
        name: HEAP_ALLOC_SYMBOL.to_string(),
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
        emit_str_from_int_helper(),
        emit_str_from_bool_helper(),
        emit_str_from_char_helper(),
        emit_str_substring_helper(),
        emit_str_find_helper(),
        emit_str_contains_helper(),
        emit_str_starts_with_helper(),
        emit_str_ends_with_helper(),
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
        STR_FROM_INT_SYMBOL,
        STR_FROM_BOOL_SYMBOL,
        STR_FROM_CHAR_SYMBOL,
        STR_SUBSTRING_SYMBOL,
        STR_FIND_SYMBOL,
        STR_CONTAINS_SYMBOL,
        STR_STARTS_WITH_SYMBOL,
        STR_ENDS_WITH_SYMBOL,
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
    // .bss: the bump pointer cell at offset 0, the heap region at offset 8.
    symbols.push(SymbolDef {
        name: HEAP_NEXT_SYMBOL.to_string(),
        section_number: 3,
        value: 0,
        is_function: false,
    });
    symbols.push(SymbolDef {
        name: HEAP_BASE_SYMBOL.to_string(),
        section_number: 3,
        value: 8,
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
    let bss_size = 8 + HEAP_REGION_SIZE;
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
mod tests {
    use lullaby_diagnostics::Span;
    use lullaby_parser::{BinaryOp, TypeRef};

    use super::*;

    #[test]
    fn emits_minimal_coff_object_for_i64_literal_main() {
        let module = literal_return_module("i64", BytecodeExprKind::Integer(42));
        let object = emit_alpha1_coff_object(&module).expect("emit object");

        assert_eq!(object.target.triple, "x86_64-pc-windows-msvc");
        assert_eq!(object.format, NativeObjectFormat::Coff);
        assert_eq!(object.entry_symbol, "main");
        assert_eq!(&object.bytes[0..2], &AMD64_MACHINE.to_le_bytes());
        assert_eq!(
            object.sections[0].offset,
            COFF_HEADER_SIZE + SECTION_HEADER_SIZE
        );
        assert_eq!(
            &object.bytes[object.sections[0].offset as usize..][..11],
            &[0x48, 0xB8, 42, 0, 0, 0, 0, 0, 0, 0, 0xC3]
        );

        let symbol_table_offset = u32::from_le_bytes(
            object.bytes[8..12]
                .try_into()
                .expect("symbol table pointer"),
        );
        assert_eq!(symbol_table_offset, object.sections[0].offset + 11);
        assert_eq!(
            &object.bytes[symbol_table_offset as usize..symbol_table_offset as usize + 8],
            b"main\0\0\0\0"
        );
    }

    #[test]
    fn rejects_non_literal_entry_body() {
        let module = literal_return_module("i64", BytecodeExprKind::Variable("value".to_string()));
        let error = emit_alpha1_coff_object(&module).expect_err("reject variable return");

        assert!(matches!(error, NativeObjectError::UnsupportedBody { .. }));
    }

    #[test]
    fn emits_stack_backed_i64_locals_and_addition() {
        let span = Span { line: 1, column: 1 };
        let i64_type = TypeRef::new("i64");
        let module = BytecodeModule {
            structs: Vec::new(),
            enums: Vec::new(),
            impls: Vec::new(),
            trait_methods: Vec::new(),
            async_functions: Vec::new(),
            extern_functions: Vec::new(),
            extern_signatures: Vec::new(),
            export_functions: Vec::new(),
            closures: Vec::new(),
            functions: vec![BytecodeFunction {
                name: "main".to_string(),
                params: Vec::new(),
                return_type: i64_type.clone(),
                instructions: vec![
                    BytecodeInstruction::Let {
                        name: "left".to_string(),
                        ty: i64_type.clone(),
                        value: bytecode_expr(BytecodeExprKind::Integer(40), "i64"),
                        span,
                    },
                    BytecodeInstruction::Let {
                        name: "right".to_string(),
                        ty: i64_type.clone(),
                        value: bytecode_expr(BytecodeExprKind::Integer(2), "i64"),
                        span,
                    },
                    BytecodeInstruction::Return(Some(bytecode_expr(
                        BytecodeExprKind::Binary {
                            left: Box::new(bytecode_expr(
                                BytecodeExprKind::Variable("left".to_string()),
                                "i64",
                            )),
                            op: BinaryOp::Add,
                            right: Box::new(bytecode_expr(
                                BytecodeExprKind::Variable("right".to_string()),
                                "i64",
                            )),
                        },
                        "i64",
                    ))),
                ],
                span,
            }],
        };

        let object = emit_alpha1_coff_object(&module).expect("emit object");
        let text =
            &object.bytes[object.sections[0].offset as usize..][..object.sections[0].size as usize];

        assert_eq!(&text[..8], &[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 16]);
        assert!(text.windows(3).any(|window| window == [0x48, 0x01, 0xC8]));
        assert_eq!(&text[text.len() - 6..], &[0x48, 0x83, 0xC4, 16, 0x5D, 0xC3]);
    }

    #[test]
    fn emits_i64_local_assignments() {
        let span = Span { line: 1, column: 1 };
        let i64_type = TypeRef::new("i64");
        let module = BytecodeModule {
            structs: Vec::new(),
            enums: Vec::new(),
            impls: Vec::new(),
            trait_methods: Vec::new(),
            async_functions: Vec::new(),
            extern_functions: Vec::new(),
            extern_signatures: Vec::new(),
            export_functions: Vec::new(),
            closures: Vec::new(),
            functions: vec![BytecodeFunction {
                name: "main".to_string(),
                params: Vec::new(),
                return_type: i64_type.clone(),
                instructions: vec![
                    BytecodeInstruction::Let {
                        name: "value".to_string(),
                        ty: i64_type.clone(),
                        value: bytecode_expr(BytecodeExprKind::Integer(40), "i64"),
                        span,
                    },
                    BytecodeInstruction::Assign {
                        name: "value".to_string(),
                        path: Vec::new(),
                        op: AssignOp::Add,
                        value: bytecode_expr(BytecodeExprKind::Integer(2), "i64"),
                        span,
                    },
                    BytecodeInstruction::Assign {
                        name: "value".to_string(),
                        path: Vec::new(),
                        op: AssignOp::Multiply,
                        value: bytecode_expr(BytecodeExprKind::Integer(2), "i64"),
                        span,
                    },
                    BytecodeInstruction::Assign {
                        name: "value".to_string(),
                        path: Vec::new(),
                        op: AssignOp::Subtract,
                        value: bytecode_expr(BytecodeExprKind::Integer(42), "i64"),
                        span,
                    },
                    BytecodeInstruction::Return(Some(bytecode_expr(
                        BytecodeExprKind::Variable("value".to_string()),
                        "i64",
                    ))),
                ],
                span,
            }],
        };

        let object = emit_alpha1_coff_object(&module).expect("emit object");
        let text =
            &object.bytes[object.sections[0].offset as usize..][..object.sections[0].size as usize];

        assert!(text.windows(3).any(|window| window == [0x48, 0x01, 0xC8]));
        assert!(
            text.windows(4)
                .any(|window| window == [0x48, 0x0F, 0xAF, 0xC1])
        );
        assert!(text.windows(3).any(|window| window == [0x48, 0x29, 0xC8]));
    }

    fn literal_return_module(return_type: &str, kind: BytecodeExprKind) -> BytecodeModule {
        BytecodeModule {
            structs: Vec::new(),
            enums: Vec::new(),
            impls: Vec::new(),
            trait_methods: Vec::new(),
            async_functions: Vec::new(),
            extern_functions: Vec::new(),
            extern_signatures: Vec::new(),
            export_functions: Vec::new(),
            closures: Vec::new(),
            functions: vec![BytecodeFunction {
                name: "main".to_string(),
                params: Vec::new(),
                return_type: TypeRef::new(return_type),
                instructions: vec![BytecodeInstruction::Return(Some(BytecodeExpr {
                    kind,
                    ty: TypeRef::new(return_type),
                    span: Span { line: 1, column: 1 },
                }))],
                span: Span { line: 1, column: 1 },
            }],
        }
    }

    fn bytecode_expr(kind: BytecodeExprKind, ty: &str) -> BytecodeExpr {
        BytecodeExpr {
            kind,
            ty: TypeRef::new(ty),
            span: Span { line: 1, column: 1 },
        }
    }
}

#[cfg(test)]
mod native_program_tests {
    use super::*;
    use crate::{lower, lower_to_bytecode};
    use lullaby_lexer::lex;
    use lullaby_parser::parse;
    use lullaby_semantics::validate_executable;

    /// Compile source through the full frontend into a `BytecodeModule`.
    fn module_for(source: &str) -> BytecodeModule {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let checked = validate_executable(&program).expect("semantic");
        let ir = lower(&checked).expect("lower");
        lower_to_bytecode(&ir)
    }

    /// Compile source as a *library* (no `main` required) into a `BytecodeModule`.
    /// Used for the C-callable-export path where the program may have only
    /// `export fn` functions.
    fn library_module_for(source: &str) -> BytecodeModule {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let checked = lullaby_semantics::validate(&program).expect("semantic");
        let ir = lower(&checked).expect("lower");
        lower_to_bytecode(&ir)
    }

    /// Walk the COFF symbol table, returning `(section_number, storage_class)` for
    /// the first symbol whose name matches `name`. The COFF header stores the
    /// symbol-table pointer at byte 8 and the symbol count at byte 12; each record
    /// is 18 bytes (8-byte name, u32 value, i16 section, u16 type, u8 storage
    /// class, u8 aux count). A name <= 8 bytes is stored inline; a longer name is
    /// stored in the string table (which follows the symbol records) and its
    /// record's name field is four zero bytes then a u32 offset into that table.
    fn coff_symbol(bytes: &[u8], name: &str) -> Option<(i16, u8)> {
        let sym_table = read_u32(bytes, 8) as usize;
        let count = read_u32(bytes, 12) as usize;
        let string_table = sym_table + count * 18;
        for i in 0..count {
            let rec = sym_table + i * 18;
            let matches = if name.len() <= 8 {
                let mut padded = [0u8; 8];
                padded[..name.len()].copy_from_slice(name.as_bytes());
                bytes[rec..rec + 8] == padded
            } else if bytes[rec..rec + 4] == [0, 0, 0, 0] {
                let str_offset = read_u32(bytes, rec + 4) as usize;
                let start = string_table + str_offset;
                let end = start + name.len();
                end <= bytes.len()
                    && bytes[start..end] == *name.as_bytes()
                    && bytes.get(end) == Some(&0)
            } else {
                false
            };
            if matches {
                let section = i16::from_le_bytes(bytes[rec + 12..rec + 14].try_into().unwrap());
                let storage = bytes[rec + 16];
                return Some((section, storage));
            }
        }
        None
    }

    /// Parse the little-endian u32 at `offset` in `bytes`.
    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    /// Parse the little-endian u16 at `offset` in `bytes`.
    fn read_u16(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
    }

    #[test]
    fn emits_object_for_add_and_main() {
        let program = emit_alpha1_native_program(&module_for(
            "fn add a i64 b i64 -> i64\n    a + b\n\nfn main -> i64\n    return add(20, 22)\n",
        ))
        .expect("emit native program");

        assert_eq!(program.target.triple, "x86_64-pc-windows-msvc");
        assert_eq!(program.entry_symbol, NATIVE_ENTRY_SYMBOL);
        assert_eq!(
            program.compiled,
            vec!["add".to_string(), "main".to_string()]
        );
        assert!(program.skipped.is_empty());

        // COFF header: AMD64 machine, one section.
        assert_eq!(read_u16(&program.bytes, 0), AMD64_MACHINE);
        assert_eq!(read_u16(&program.bytes, 2), 1, "one section");

        // `.text` section header begins right after the COFF header.
        let sec = COFF_HEADER_SIZE as usize;
        assert_eq!(&program.bytes[sec..sec + 5], b".text");
        let num_relocs = read_u16(&program.bytes, sec + 32);
        // Three relocations: stub->main, stub->ExitProcess, main->add.
        assert_eq!(num_relocs, 3, "expected three relocations");

        // The entry stub is the first bytes of `.text`: `sub rsp, 40` then a call.
        let text_offset = read_u32(&program.bytes, sec + 20) as usize;
        assert_eq!(
            &program.bytes[text_offset..text_offset + 5],
            &[0x48, 0x83, 0xEC, 0x28, 0xE8]
        );
    }

    #[test]
    fn emits_object_for_if_based_function() {
        // A recursive `if`-based `fib` plus a `main` calling it. Every function is
        // i64-scalar, so all compile.
        let program = emit_alpha1_native_program(&module_for(
            "fn fib n i64 -> i64\n    if n < 2\n        return n\n    return fib(n - 1) + fib(n - 2)\n\nfn main -> i64\n    return fib(6)\n",
        ))
        .expect("emit native program");
        assert_eq!(
            program.compiled,
            vec!["fib".to_string(), "main".to_string()]
        );

        // The `if n < 2` condition lowers as a fused compare-and-branch against
        // the immediate: `cmp rax, 2` (48 3D 02 00 00 00) then `jge rel32`
        // (0F 8D ..) — the inverse of `<`, taken when the condition is false.
        // This replaces the old `setl al; movzx; test rax,rax; jz` sequence, so
        // the boolean is never materialized: there is no `setl al` for this `<`.
        let sec = COFF_HEADER_SIZE as usize;
        let text_offset = read_u32(&program.bytes, sec + 20) as usize;
        let text_size = read_u32(&program.bytes, sec + 16) as usize;
        let text = &program.bytes[text_offset..text_offset + text_size];
        assert!(
            text.windows(6)
                .any(|w| w == [0x48, 0x3D, 0x02, 0x00, 0x00, 0x00]),
            "expected a fused `cmp rax, 2` against the immediate"
        );
        assert!(
            text.windows(2).any(|w| w == [0x0F, 0x8D]),
            "expected a fused `jge` branch (inverted `<`)"
        );
        assert!(
            !text.windows(3).any(|w| w == [0x0F, 0x9C, 0xC0]),
            "the `<` should be fused into the branch, not materialized as `setl al`"
        );
    }

    #[test]
    fn emits_object_for_while_loop() {
        let program = emit_alpha1_native_program(&module_for(
            "fn main -> i64\n    let n i64 = 0\n    let sum i64 = 0\n    while n < 5\n        n += 1\n        sum += n\n    return sum\n",
        ))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);

        // Only the entry stub references external symbols (main, ExitProcess); the
        // loop is self-contained, so exactly two relocations exist.
        let sec = COFF_HEADER_SIZE as usize;
        assert_eq!(read_u16(&program.bytes, sec + 32), 2);

        // A `while` loop closes with a backward `jmp` (E9) whose rel32 is negative
        // (it jumps back to the loop top). Confirm at least one such backward jump
        // appears in the compiled `.text`.
        let text_offset = read_u32(&program.bytes, sec + 20) as usize;
        let text_size = read_u32(&program.bytes, sec + 16) as usize;
        let text = &program.bytes[text_offset..text_offset + text_size];
        assert!(
            has_backward_jmp(text),
            "expected a backward `jmp` closing the while loop"
        );
    }

    /// Whether `code` contains a near `jmp rel32` (opcode `0xE9`) whose signed
    /// 32-bit displacement is negative — i.e. a backward branch, as a loop's
    /// closing jump must be. Scans every `0xE9` and decodes the following four
    /// bytes as a little-endian `i32`.
    fn has_backward_jmp(code: &[u8]) -> bool {
        code.windows(5)
            .any(|w| w[0] == 0xE9 && i32::from_le_bytes([w[1], w[2], w[3], w[4]]) < 0)
    }

    #[test]
    fn emits_for_loops_and_inter_function_calls() {
        // A `for`-sum, a `for`-product, and a `combine` that calls all three
        // helpers plus itself feeds `main`. Every function is i64-scalar, so all
        // compile — none is skipped — and the emitter must produce real `call`
        // relocations (inter-function calls) and backward `jmp`s (the loops).
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn for_sum n i64 -> i64\n",
            "    let total i64 = 0\n",
            "    for i from 1 to n\n",
            "        total += i\n",
            "    return total\n\n",
            "fn for_product n i64 -> i64\n",
            "    let product i64 = 1\n",
            "    for i from 1 to n\n",
            "        product *= i\n",
            "    return product\n\n",
            "fn combine a i64 b i64 -> i64\n",
            "    return for_sum(a) + for_product(b)\n\n",
            "fn main -> i64\n",
            "    return combine(4, 3)\n",
        )))
        .expect("emit native program");
        assert_eq!(
            program.compiled,
            vec![
                "for_sum".to_string(),
                "for_product".to_string(),
                "combine".to_string(),
                "main".to_string(),
            ],
            "every i64-scalar function compiles"
        );
        assert!(
            program.skipped.is_empty(),
            "no function should be skipped: {:?}",
            program.skipped
        );

        // The `.text` holds the entry stub plus every function. The two intra-body
        // `call` relocations (combine->for_sum, combine->for_product) join the
        // stub's two (stub->main, stub->ExitProcess) and main->combine, so at least
        // three `call` relocations to compiled functions are present.
        let sec = COFF_HEADER_SIZE as usize;
        let num_relocs = read_u16(&program.bytes, sec + 32) as usize;
        assert!(
            num_relocs >= 5,
            "expected the inter-function call relocations, got {num_relocs}"
        );

        // The compiled `.text` must contain a backward `jmp` (each `for` loop
        // closes with one) — direct evidence the loops were lowered natively.
        let text_offset = read_u32(&program.bytes, sec + 20) as usize;
        let text_size = read_u32(&program.bytes, sec + 16) as usize;
        let text = &program.bytes[text_offset..text_offset + text_size];
        assert!(
            has_backward_jmp(text),
            "expected a backward `jmp` closing a for loop"
        );
        // And an `imul rax, rcx` (48 0F AF C1), the `for`-product multiply.
        assert!(
            text.windows(4).any(|w| w == [0x48, 0x0F, 0xAF, 0xC1]),
            "expected an `imul rax, rcx` for the product loop"
        );
    }

    #[test]
    fn compiles_match_over_enum_parameter() {
        // `match` over an enum passed as a *parameter* now compiles: a scalar-
        // payload enum crosses the boundary by pointer (copied into the callee's
        // frame), and the callee matches the local copy. `double`/`main` still
        // compile too.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn classify o option<i64> -> i64\n",
            "    match o\n",
            "        some(v) -> v\n",
            "        none -> 0\n\n",
            "fn double x i64 -> i64\n",
            "    x + x\n\n",
            "fn main -> i64\n",
            "    return double(21) + classify(some(1))\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"classify".to_string())
                && program.compiled.contains(&"double".to_string())
                && program.compiled.contains(&"main".to_string()),
            "enum-parameter match must compile: {:?} / {:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    }

    #[test]
    fn skips_non_i64_functions_but_compiles_the_rest() {
        // `stringify` uses `to_string(f64)` (dtoa, deferred) so it is skipped;
        // `main` and `add` are i64 (compiled). (Plain string values are now in the
        // native subset, so the skipped example uses the still-deferred float
        // `to_string` rather than an identity string function.)
        let program = emit_alpha1_native_program(&module_for(
            "fn stringify -> string\n    to_string(1.5)\n\nfn add a i64 b i64 -> i64\n    a + b\n\nfn main -> i64\n    return add(1, 2)\n",
        ))
        .expect("emit native program");
        assert_eq!(
            program.compiled,
            vec!["add".to_string(), "main".to_string()]
        );
        assert_eq!(program.skipped.len(), 1);
        assert_eq!(program.skipped[0].name, "stringify");
    }

    #[test]
    fn compiles_fixed_width_integer_function_natively() {
        // A `main` whose signature is `-> i64` but which uses the fixed-width
        // integer types internally (u32 wrapping subtraction, an unsigned
        // comparison, and the `to_u32`/`to_i64` conversions) now compiles
        // natively instead of being skipped.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let a u32 = to_u32(0)\n",
            "    let b u32 = to_u32(1)\n",
            "    let wrapped i64 = to_i64(a - b)\n",
            "    let flag i64 = 0\n",
            "    if a > b\n",
            "        flag = 1\n",
            "    wrapped + flag\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(
            program.skipped.is_empty(),
            "no function should be skipped: {:?}",
            program.skipped
        );

        // The compiled body must contain a `mov eax, eax` (89 C0) — the u32
        // zero-extend that re-normalizes each width-producing op — and a `setb
        // al` (0F 92 C0), the unsigned `>` (a > b) condition code.
        let sec = COFF_HEADER_SIZE as usize;
        let text_offset = read_u32(&program.bytes, sec + 20) as usize;
        let text_size = read_u32(&program.bytes, sec + 16) as usize;
        let text = &program.bytes[text_offset..text_offset + text_size];
        assert!(
            text.windows(2).any(|w| w == [0x89, 0xC0]),
            "expected a `mov eax, eax` u32 normalization"
        );
        assert!(
            text.windows(3).any(|w| w == [0x0F, 0x97, 0xC0]),
            "expected a `seta al` for the unsigned `>`"
        );
    }

    #[test]
    fn compiles_fixed_width_bitwise_and_shifts_natively() {
        // Bitwise and shift operators on fixed-width kinds compile natively: a u8
        // AND, a signed i32 arithmetic right shift, and one's-complement `~`.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let a u8 = to_u8(200)\n",
            "    let b u8 = to_u8(15)\n",
            "    let band u8 = a & b\n",
            "    let notv u8 = ~a\n",
            "    let s i32 = to_i32(0 - 8)\n",
            "    let sar i32 = s >> to_i32(1)\n",
            "    to_i64(band) + to_i64(notv) + to_i64(sar)\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);

        let sec = COFF_HEADER_SIZE as usize;
        let text_offset = read_u32(&program.bytes, sec + 20) as usize;
        let text_size = read_u32(&program.bytes, sec + 16) as usize;
        let text = &program.bytes[text_offset..text_offset + text_size];
        // `sar rax, cl` (48 D3 F8) for the signed right shift, and `not rax`
        // (48 F7 D0) for `~`.
        assert!(
            text.windows(3).any(|w| w == [0x48, 0xD3, 0xF8]),
            "expected a `sar rax, cl` for the signed i32 `>>`"
        );
        assert!(
            text.windows(3).any(|w| w == [0x48, 0xF7, 0xD0]),
            "expected a `not rax` for `~`"
        );
    }

    #[test]
    fn signed_division_guards_min_over_neg_one_overflow() {
        // `idiv` raises a hardware #DE on `i64::MIN / -1`, but the interpreters
        // wrap it to `i64::MIN` (`wrapping_div`). Both the plain-`i64` and the
        // fixed-width signed division paths must emit the wrapping guard —
        // `cmp r8, -1` (49 83 F8 FF) followed by `neg rax` (48 F7 D8) — so the
        // native backend matches the interpreters instead of trapping.
        //
        // `a / b` is plain i64; `to_isize(a) / to_isize(b)` is the fixed-width
        // signed (isize) path. Both go through `emit_signed_idiv_r8`.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let a i64 = 0 - 9223372036854775807 - 1\n",
            "    let b i64 = 0 - 1\n",
            "    let q64 i64 = a / b\n",
            "    let qsz isize = to_isize(a) / to_isize(b)\n",
            "    to_i64(qsz) - q64\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);

        let sec = COFF_HEADER_SIZE as usize;
        let text_offset = read_u32(&program.bytes, sec + 20) as usize;
        let text_size = read_u32(&program.bytes, sec + 16) as usize;
        let text = &program.bytes[text_offset..text_offset + text_size];
        // Two signed divisions -> two guards. Count `cmp r8, -1` occurrences.
        let guards = text
            .windows(4)
            .filter(|w| *w == [0x49, 0x83, 0xF8, 0xFF])
            .count();
        assert_eq!(
            guards, 2,
            "expected a `cmp r8, -1` guard before each of the two signed divisions"
        );
        assert!(
            text.windows(3).any(|w| w == [0x48, 0xF7, 0xD8]),
            "expected a `neg rax` implementing the wrapping `x / -1`"
        );
    }

    #[test]
    fn skips_float_math_builtin_gracefully() {
        // f64/f32 arithmetic, comparison, and `to_f32`/`to_f64` are now native,
        // but the transcendental/math builtins (`sqrt`, `sin`, `floor`, …) remain
        // deferred. A `-> i64` function that calls one must skip gracefully and
        // report why, leaving nothing eligible.
        let err = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let r f64 = sqrt(16.0)\n",
            "    let flag i64 = 0\n",
            "    if r > 3.0\n",
            "        flag = 1\n",
            "    flag\n",
        )))
        .expect_err("float math builtin is deferred");
        assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
        assert!(err.skipped.iter().any(|s| s.name == "main"));
    }

    #[test]
    fn compiles_overflow_builtins() {
        // The overflow-aware builtins are now emitted natively (not deferred):
        // `wrapping_*`/`saturating_*` produce a fixed-width scalar and `checked_*`
        // an `option<T>` matched in place. A `main` exercising all three compiles.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn chk m i8 n i8 -> i64\n",
            "    match checked_add(m, n)\n",
            "        some(v) -> to_i64(v)\n",
            "        none -> 0 - 1\n\n",
            "fn main -> i64\n",
            "    let s u8 = saturating_add(to_u8(200), to_u8(100))\n",
            "    let w u8 = wrapping_mul(to_u8(16), to_u8(16))\n",
            "    to_i64(s) + to_i64(w) + chk(to_i8(127), to_i8(1))\n",
        )))
        .expect("overflow builtins compile natively");
        assert_eq!(
            program.compiled,
            vec!["chk".to_string(), "main".to_string()]
        );
        assert!(program.skipped.is_empty(), "nothing should skip");
    }

    #[test]
    fn emits_extern_call_as_undefined_external_symbol() {
        // An `extern fn` C function is called from `main`. The call lowers to a
        // REL32 relocation against an undefined external symbol named after the C
        // function, and the C runtime import library is requested for linking.
        let program = emit_alpha1_native_program(&module_for(
            "extern fn llabs x i64 -> i64\n\nfn main -> i64\n    llabs(-7)\n",
        ))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert_eq!(
            program.import_libs,
            vec![C_RUNTIME_IMPORT_LIB.to_string()],
            "extern calls require the C runtime import library"
        );

        // Three relocations: stub->main, stub->ExitProcess, main->llabs.
        let sec = COFF_HEADER_SIZE as usize;
        assert_eq!(read_u16(&program.bytes, sec + 32), 3, "three relocations");

        // The undefined external symbol `llabs` (<= 8 bytes) is stored inline in a
        // symbol record's name field; scan the object bytes for it.
        let needle = b"llabs\0\0\0";
        assert!(
            program.bytes.windows(8).any(|w| w == needle),
            "expected an `llabs` external symbol record"
        );
    }

    #[test]
    fn emits_i32_extern_call_with_import_and_return_normalization() {
        // An `extern fn` with an `i32` C signature (e.g. `toupper(int) -> int`)
        // now compiles: the call lowers to a REL32 relocation against an undefined
        // external symbol, requests the C runtime import library, and — because
        // Win64 leaves the upper bits of a narrow integer return undefined — the
        // emitter normalizes the `i32` return with `movsxd rax, eax` (48 63 C0).
        let program = emit_alpha1_native_program(&module_for(
            "extern fn toupper c i32 -> i32\n\nfn main -> i64\n    to_i64(toupper(to_i32(97)))\n",
        ))
        .expect("emit native program for an i32 extern");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert_eq!(
            program.import_libs,
            vec![C_RUNTIME_IMPORT_LIB.to_string()],
            "an i32 extern call still requires the C runtime import library"
        );

        // The undefined external symbol `toupper` (<= 8 bytes) is stored inline.
        assert!(
            program.bytes.windows(8).any(|w| w == b"toupper\0"),
            "expected a `toupper` external symbol record"
        );

        // `main`'s text contains `movsxd rax, eax` (48 63 C0) — the i32 C return
        // normalization emitted after the `call`.
        let text = text_bytes(&program);
        assert!(
            text.windows(3).any(|w| w == [0x48, 0x63, 0xC0]),
            "expected an i32 return normalization (`movsxd rax, eax`) after the extern call"
        );
    }

    #[test]
    fn emits_u8_extern_call_with_zero_extend_return_normalization() {
        // A `u8`/`byte`-class C return is zero-extended (`movzx rax, al` = 48 0F
        // B6 C0). This also exercises the `bool`/`byte` -> u8 marshalling class.
        let program = emit_alpha1_native_program(&module_for(
            "extern fn tolower c u8 -> u8\n\nfn main -> i64\n    to_i64(tolower(to_u8(65)))\n",
        ))
        .expect("emit native program for a u8 extern");
        let text = text_bytes(&program);
        assert!(
            text.windows(4).any(|w| w == [0x48, 0x0F, 0xB6, 0xC0]),
            "expected a u8 return normalization (`movzx rax, al`) after the extern call"
        );
    }

    #[test]
    fn float_extern_arg_and_return_route_through_xmm0() {
        // A `f64`-taking/returning C extern now marshals across the Win64 C ABI:
        // its single float argument is loaded into `xmm0` (the position-0 SSE
        // register) — `movsd xmm0, [rsp+0]` = `F2 0F 10 44 24 00` — and the `f64`
        // return is consumed from `xmm0`. The caller compiles natively instead of
        // being demoted.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "extern fn cfloor x f64 -> f64\n\n",
            "fn main -> i64\n",
            "    let r f64 = cfloor(3.7)\n",
            "    let flag i64 = 0\n",
            "    if r > 3.0\n",
            "        flag = 1\n",
            "    flag\n",
        )))
        .expect("float extern now marshals natively");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        let text = text_bytes(&program);
        assert!(
            text.windows(6)
                .any(|w| w == [0xF2, 0x0F, 0x10, 0x44, 0x24, 0x00]),
            "expected the float argument loaded into xmm0 (`movsd xmm0, [rsp]`)"
        );
    }

    #[test]
    fn mixed_float_then_int_extern_routes_xmm0_and_rdx() {
        // Win64 positional routing: for `f(double a, int b)` the float at position
        // 0 goes to `xmm0` and the integer at position 1 goes to integer register 1
        // (`rdx`), never both sequences for one argument. `ldexp(double, int)`
        // exercises exactly this mixed signature.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "extern fn ldexp x f64 e i32 -> f64\n\n",
            "fn main -> i64\n",
            "    let r f64 = ldexp(1.5, to_i32(3))\n",
            "    let flag i64 = 0\n",
            "    if r > 11.0\n",
            "        flag = 1\n",
            "    flag\n",
        )))
        .expect("mixed float/int extern marshals natively");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        let text = text_bytes(&program);
        // Position 0 float -> xmm0: `movsd xmm0, [rsp + disp]` (`F2 0F 10 /r` with
        // an rsp SIB and reg 0). Match the fixed opcode+SIB prefix, any disp8.
        assert!(
            text.windows(5)
                .any(|w| w[..3] == [0xF2, 0x0F, 0x10] && w[3] == 0x44 && w[4] == 0x24),
            "expected the position-0 float argument loaded into xmm0"
        );
        // Position 1 integer -> rdx: `mov rdx, [rsp + disp]` (`48 8B /r`, reg=rdx=2,
        // rsp SIB). ModRM for disp8 with reg=2 = 0x54, SIB 0x24.
        assert!(
            text.windows(5)
                .any(|w| w[..2] == [0x48, 0x8B] && w[2] == 0x54 && w[3] == 0x24),
            "expected the position-1 integer argument loaded into rdx"
        );
    }

    #[test]
    fn int_then_float_extern_routes_rcx_and_xmm1() {
        // The mirror case: `f(int a, double b)` puts the integer at position 0 in
        // `rcx` and the float at position 1 in `xmm1` — each position consumes its
        // slot in exactly one register sequence.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "extern fn scalbn_like a i32 b f64 -> f64\n\n",
            "fn main -> i64\n",
            "    let r f64 = scalbn_like(to_i32(2), 4.0)\n",
            "    let flag i64 = 0\n",
            "    if r > 3.0\n",
            "        flag = 1\n",
            "    flag\n",
        )))
        .expect("int/float extern marshals natively");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        let text = text_bytes(&program);
        // Position 0 integer -> rcx: `mov rcx, [rsp + disp]` (48 8B, reg=rcx=1,
        // ModRM disp8 = 0x4C, SIB 0x24).
        assert!(
            text.windows(4)
                .any(|w| w[..2] == [0x48, 0x8B] && w[2] == 0x4C && w[3] == 0x24),
            "expected the position-0 integer argument loaded into rcx"
        );
        // Position 1 float -> xmm1: `movsd xmm1, [rsp + disp]` (F2 0F 10, reg=1,
        // ModRM disp8 = 0x4C, SIB 0x24).
        assert!(
            text.windows(5)
                .any(|w| w[..3] == [0xF2, 0x0F, 0x10] && w[3] == 0x4C && w[4] == 0x24),
            "expected the position-1 float argument loaded into xmm1"
        );
    }

    #[test]
    fn cstr_extern_materializes_string_and_emits_helper() {
        // An `extern fn` with a `cstr` parameter accepts a Lullaby `string`: the
        // caller evaluates the string to a record pointer, then calls
        // `__lullaby_to_cstr` to materialize a NUL-terminated buffer before the C
        // `call`. The distinctive tail of that helper is the NUL-terminator write
        // `mov byte [rdi], 0` (`C6 07 00`); its presence proves the helper is
        // emitted into `.text`.
        let program = emit_alpha1_native_program(&module_for(
            "extern fn strlen s cstr -> usize\n\nfn main -> i64\n    to_i64(strlen(\"hi\"))\n",
        ))
        .expect("cstr extern compiles natively");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        assert_eq!(
            program.import_libs,
            vec![C_RUNTIME_IMPORT_LIB.to_string()],
            "a cstr extern call requires the C runtime import library"
        );
        // The `__lullaby_to_cstr` helper symbol is a defined external in the object.
        assert!(
            program
                .bytes
                .windows(TO_CSTR_SYMBOL.len())
                .any(|w| w == TO_CSTR_SYMBOL.as_bytes()),
            "expected the `__lullaby_to_cstr` helper symbol in the object"
        );
        let text = text_bytes(&program);
        // The helper's NUL-terminator write (`mov byte [rdi], 0`).
        assert!(
            text.windows(3).any(|w| w == [0xC6, 0x07, 0x00]),
            "expected the cstr NUL-terminator write in `__lullaby_to_cstr`"
        );
    }

    #[test]
    fn pointer_extern_param_and_return_compile() {
        // Raw pointers cross the FFI boundary as machine-address words: `malloc`
        // returns a `ptr<byte>` bound to a native local, and `free` takes a
        // `ptr<byte>` parameter. Both compile natively (a pointer is an `i64`-class
        // word), so the caller is not demoted.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "extern fn malloc n usize -> ptr<byte>\n\n",
            "extern fn free p ptr<byte> -> void\n\n",
            "fn main -> i64\n",
            "    let p ptr<byte> = malloc(to_usize(8))\n",
            "    free(p)\n",
            "    0\n",
        )))
        .expect("pointer extern params/returns compile natively");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        // Both C symbols are undefined externals in the object.
        assert!(
            program.bytes.windows(7).any(|w| w == b"malloc\0"),
            "expected a `malloc` external symbol record"
        );
        assert!(
            program.bytes.windows(5).any(|w| w == b"free\0"),
            "expected a `free` external symbol record"
        );
    }

    #[test]
    fn six_arg_extern_call_spills_fifth_and_sixth_to_stack() {
        // An `extern fn` with six arguments spills its 5th and 6th (0-indexed
        // positions 4 and 5) into the outgoing stack-argument area above the 32-byte
        // shadow space, exactly like an internal call. During staging (six pushed
        // words), position 4's outgoing slot is `[rsp + 8*6 + 32 + 0] = [rsp+0x50]`;
        // the write is `mov [rsp+0x50], rax` (`48 89 84 24 50 00 00 00`).
        let program = emit_alpha1_native_program(&module_for(concat!(
            "extern fn take6 a i64 b i64 c i64 d i64 e i64 f i64 -> i64\n\n",
            "fn main -> i64\n",
            "    take6(1, 2, 3, 4, 5, 6)\n",
        )))
        .expect("a >4-arg extern call compiles natively");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        let text = text_bytes(&program);
        assert!(
            text.windows(8)
                .any(|w| w == [0x48, 0x89, 0x84, 0x24, 0x50, 0x00, 0x00, 0x00]),
            "expected the 5th argument written to the outgoing stack slot `[rsp+0x50]`"
        );
        assert!(
            text.windows(8)
                .any(|w| w == [0x48, 0x89, 0x84, 0x24, 0x58, 0x00, 0x00, 0x00]),
            "expected the 6th argument written to the outgoing stack slot `[rsp+0x58]`"
        );
    }

    #[test]
    fn export_fn_with_float_params_spills_from_xmm() {
        // An `export fn` with a float parameter receives it in the positional SSE
        // register and spills it into the parameter slot with `movsd [rbp-slot],
        // xmm0` (`F2 0F 11 85 <disp32>` for xmm0 at position 0). The exported
        // symbol compiles natively as a library object.
        let program = emit_alpha1_native_program(&library_module_for(
            "export fn scale x f64 -> f64\n    x * x\n",
        ))
        .expect("float export compiles natively");
        assert!(
            program.compiled.contains(&"scale".to_string()),
            "expected `scale` compiled: {:?}",
            program.compiled
        );
        let text = text_bytes(&program);
        // Prologue spill of the xmm0 float parameter: `movsd [rbp+disp32], xmm0`.
        assert!(
            text.windows(4).any(|w| w == [0xF2, 0x0F, 0x11, 0x85]),
            "expected the float parameter spilled from xmm0 (`movsd [rbp-slot], xmm0`)"
        );
    }

    #[test]
    fn errors_when_no_i64_scalar_function_is_eligible() {
        // `main` uses `to_string(f64)` (dtoa, deferred), so nothing is eligible for
        // native. (Plain string values are now in the subset, so the not-eligible
        // example uses the still-deferred float `to_string`.)
        let err =
            emit_alpha1_native_program(&module_for("fn main -> i64\n    len(to_string(1.5))\n"))
                .expect_err("no eligible");
        assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
        assert!(err.skipped.iter().any(|s| s.name == "main"));
    }

    #[test]
    fn string_table_holds_long_symbol_names() {
        // A function name longer than eight bytes must live in the COFF string
        // table; the emitter must still produce a valid object.
        let program = emit_alpha1_native_program(&module_for(
            "fn accumulate_total n i64 -> i64\n    n + 1\n\nfn main -> i64\n    return accumulate_total(41)\n",
        ))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"accumulate_total".to_string()),
            "long-named function compiled"
        );
        // The long name appears verbatim in the string table at the tail.
        assert!(
            program
                .bytes
                .windows("accumulate_total".len())
                .any(|w| w == b"accumulate_total"),
            "long symbol name stored"
        );
    }

    #[test]
    fn compiles_all_i64_struct_locals() {
        // A `main` that builds a struct positionally and by name, mutates a
        // field, and reads fields is eligible: it compiles with no skips.
        let program = emit_alpha1_native_program(&module_for(
            "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(3, 4)\n    let q Point = Point(y: 10, x: 20)\n    p.x = p.x + 5\n    return p.x + q.y\n",
        ))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(
            program.skipped.is_empty(),
            "no skips: {:?}",
            program.skipped
        );
    }

    #[test]
    fn compiles_fixed_i64_array_with_const_and_dynamic_index() {
        // Fixed array: constant-index write, compound-index write, `len`, and a
        // dynamic-index read inside a `for` loop. All in the native subset.
        let program = emit_alpha1_native_program(&module_for(
            "fn main -> i64\n    let xs array<i64> = [1, 2, 3, 4]\n    xs[0] = 10\n    xs[3] += 6\n    let total i64 = 0\n    for i from 0 to len(xs) - 1\n        total += xs[i]\n    return total\n",
        ))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(
            program.skipped.is_empty(),
            "no skips: {:?}",
            program.skipped
        );
    }

    #[test]
    fn skips_struct_with_non_i64_field() {
        // A struct with a `string` field is not an all-i64 native type; a `main`
        // that constructs it is demoted to skipped, and since nothing else is
        // eligible the emitter reports `L0339`.
        let err = emit_alpha1_native_program(&module_for(
            "struct Tagged\n    id i64\n    name string\n\nfn main -> i64\n    let t Tagged = Tagged(1, \"x\")\n    return t.id\n",
        ))
        .expect_err("string-field struct is not native");
        assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
        assert!(err.skipped.iter().any(|s| s.name == "main"));
    }

    #[test]
    fn emits_rdata_and_bss_for_string_len() {
        // A `main` deriving an i64 from `len` over string literals is eligible for
        // native codegen and gains `.rdata` (the constants) + `.bss` (the heap).
        let program = emit_alpha1_native_program(&module_for(
            "fn main -> i64\n    let a i64 = len(\"hello\")\n    let b i64 = len(\"native\")\n    return a + b\n",
        ))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(
            program.skipped.is_empty(),
            "no skips: {:?}",
            program.skipped
        );

        // Three sections now: .text, .rdata, .bss.
        assert_eq!(read_u16(&program.bytes, 2), 3, "three sections");
        let sec1 = COFF_HEADER_SIZE as usize;
        let sec2 = sec1 + SECTION_HEADER_SIZE as usize;
        let sec3 = sec2 + SECTION_HEADER_SIZE as usize;
        assert_eq!(&program.bytes[sec1..sec1 + 5], b".text");
        assert_eq!(&program.bytes[sec2..sec2 + 6], b".rdata");
        assert_eq!(&program.bytes[sec3..sec3 + 4], b".bss");

        // `.bss` is uninitialized: SizeOfRawData is the reserved size, no raw
        // pointer.
        let bss_size = read_u32(&program.bytes, sec3 + 16);
        assert_eq!(
            bss_size,
            8 + HEAP_REGION_SIZE,
            "bss reserves heap + pointer"
        );
        assert_eq!(
            read_u32(&program.bytes, sec3 + 20),
            0,
            "bss has no raw data"
        );

        // The interned string bytes appear verbatim in `.rdata`.
        assert!(
            program.bytes.windows(5).any(|w| w == b"hello"),
            "hello constant stored"
        );
        assert!(
            program.bytes.windows(6).any(|w| w == b"native"),
            "native constant stored"
        );

        // Identical string literals are interned once (dedup): a symbol `__str1`
        // exists (two distinct strings) but not `__str2`.
        assert!(
            program.bytes.windows(6).any(|w| w == b"__str1"),
            "second string symbol present"
        );
    }

    #[test]
    fn dedups_repeated_string_literals() {
        // The same literal used twice interns to a single `.rdata` constant, so
        // only `__str0` exists.
        let program = emit_alpha1_native_program(&module_for(
            "fn main -> i64\n    return len(\"hi\") + len(\"hi\")\n",
        ))
        .expect("emit native program");
        assert!(
            program.bytes.windows(6).any(|w| w == b"__str0"),
            "first string symbol present"
        );
        assert!(
            !program.bytes.windows(6).any(|w| w == b"__str1"),
            "no second symbol for a repeated literal"
        );
    }

    #[test]
    fn native_type_words_flatten_nested_aggregates() {
        // Layout sizing: a nested all-i64 struct and a fixed array flatten to the
        // expected word counts.
        let structs = vec![
            IrStructDef {
                name: "Pair".to_string(),
                fields: vec![
                    ("a".to_string(), TypeRef::new("i64")),
                    ("b".to_string(), TypeRef::new("i64")),
                ],
            },
            IrStructDef {
                name: "Line".to_string(),
                fields: vec![
                    ("start".to_string(), TypeRef::new("Pair")),
                    ("end".to_string(), TypeRef::new("Pair")),
                ],
            },
        ];
        let line = resolve_native_type(&TypeRef::new("Line"), &structs, &[]).expect("resolve Line");
        assert_eq!(line.words(), 4, "Line flattens to four i64 words");

        let array = NativeType::Array {
            elem: Box::new(NativeType::I64),
            len: 5,
        };
        assert_eq!(array.words(), 5);
    }

    #[test]
    fn exports_function_as_external_defined_text_symbol() {
        // An `export fn` with a body, no `main`. It compiles as a library object:
        // no entry stub, and the exported function appears in the COFF symbol
        // table as an EXTERNAL (storage class 2) symbol DEFINED in `.text`
        // (section number 1) under its plain C name, so a C caller declaring
        // `extern long long add_seven(long long);` links against it.
        let program = emit_alpha1_native_program(&library_module_for(
            "export fn add_seven x i64 -> i64\n    x + 7\n",
        ))
        .expect("emit native program");

        assert_eq!(program.compiled, vec!["add_seven".to_string()]);
        assert!(
            program.skipped.is_empty(),
            "no skips: {:?}",
            program.skipped
        );
        // A library object has no entry point.
        assert!(
            program.entry_symbol.is_empty(),
            "an export-only object is a library with no entry symbol"
        );

        // The exported symbol is external + defined in `.text`.
        let (section, storage) =
            coff_symbol(&program.bytes, "add_seven").expect("add_seven symbol present");
        assert_eq!(section, 1, "add_seven is defined in `.text` (section 1)");
        assert_eq!(storage, 2, "add_seven has EXTERNAL storage class");

        // No entry stub / ExitProcess symbol exists in a library object.
        assert!(
            coff_symbol(&program.bytes, NATIVE_ENTRY_SYMBOL).is_none(),
            "a library object omits the `_lullaby_start` entry stub"
        );
        assert!(
            coff_symbol(&program.bytes, EXIT_PROCESS_SYMBOL).is_none(),
            "a library object has no `ExitProcess` dependency"
        );
    }

    #[test]
    fn export_alongside_main_keeps_the_entry_stub() {
        // When a program has both `main` and an `export fn`, the entry stub is
        // still emitted (a runnable program) and the export is additionally
        // external + defined in `.text`.
        let program = emit_alpha1_native_program(&module_for(
            "export fn doubled x i64 -> i64\n    x * 2\n\nfn main -> i64\n    doubled(21)\n",
        ))
        .expect("emit native program");

        assert_eq!(program.entry_symbol, NATIVE_ENTRY_SYMBOL);
        let (section, storage) =
            coff_symbol(&program.bytes, "doubled").expect("doubled symbol present");
        assert_eq!(section, 1, "export defined in `.text`");
        assert_eq!(storage, 2, "export is EXTERNAL");
        // The stub is present because `main` exists.
        assert!(
            coff_symbol(&program.bytes, NATIVE_ENTRY_SYMBOL).is_some(),
            "entry stub present when `main` exists"
        );
    }

    #[test]
    fn asm_bytes_are_emitted_verbatim_into_text() {
        // A `main` whose `unsafe` `asm` block emits the seven bytes of
        // `mov rax, 42`. The emitter must copy those bytes verbatim into `.text`.
        let program = emit_alpha1_native_program(&module_for(
            "fn main -> i64\n    unsafe\n        asm 72, 199, 192, 42, 0, 0, 0\n",
        ))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(program.skipped.is_empty());

        // The exact `mov rax, 42` byte pattern must appear in the object bytes
        // (inside `.text`), proving the raw bytes were emitted verbatim.
        let needle = [0x48u8, 0xC7, 0xC0, 0x2A, 0x00, 0x00, 0x00];
        let found = program
            .bytes
            .windows(needle.len())
            .any(|window| window == needle);
        assert!(
            found,
            "expected the raw `mov rax, 42` asm bytes verbatim in the emitted object"
        );
    }

    /// The `.text` bytes of an emitted program (the single `.text` section's
    /// raw data range).
    fn text_bytes(program: &NativeProgram) -> &[u8] {
        let sec = COFF_HEADER_SIZE as usize;
        let text_offset = read_u32(&program.bytes, sec + 20) as usize;
        let text_size = read_u32(&program.bytes, sec + 16) as usize;
        &program.bytes[text_offset..text_offset + text_size]
    }

    #[test]
    fn compiles_float_arithmetic_function_natively() {
        // A `main` whose signature is `-> i64` but which computes with `f64`/`f32`
        // internals (arithmetic, comparison, and the `to_f32`/`to_f64`
        // conversions) now compiles natively instead of being skipped.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let a f64 = 1.5\n",
            "    let b f64 = 2.0\n",
            "    let s f64 = a + b\n",
            "    let half f32 = to_f32(1.0) / to_f32(2.0)\n",
            "    let flag i64 = 0\n",
            "    if s > 3.0\n",
            "        flag = 1\n",
            "    if to_f64(half) < 1.0\n",
            "        flag = flag + 1\n",
            "    flag\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(
            program.skipped.is_empty(),
            "no function should be skipped: {:?}",
            program.skipped
        );

        let text = text_bytes(&program);
        // `addsd xmm0, xmm1` (F2 0F 58 C1) — the f64 addition.
        assert!(
            text.windows(4).any(|w| w == [0xF2, 0x0F, 0x58, 0xC1]),
            "expected an `addsd xmm0, xmm1`"
        );
        // `divss xmm0, xmm1` (F3 0F 5E C1) — the f32 division (single precision).
        assert!(
            text.windows(4).any(|w| w == [0xF3, 0x0F, 0x5E, 0xC1]),
            "expected a single-precision `divss xmm0, xmm1`"
        );
        // `ucomisd xmm0, xmm1` (66 0F 2E C1) — the f64 `>` compare.
        assert!(
            text.windows(4).any(|w| w == [0x66, 0x0F, 0x2E, 0xC1]),
            "expected a `ucomisd` for the f64 comparison"
        );
        // `cvtsd2ss xmm0, xmm0` (F2 0F 5A C0) — the `to_f32` rounding.
        assert!(
            text.windows(4).any(|w| w == [0xF2, 0x0F, 0x5A, 0xC0]),
            "expected a `cvtsd2ss` for `to_f32`"
        );
        // `cvtss2sd xmm0, xmm0` (F3 0F 5A C0) — the `to_f64` widening.
        assert!(
            text.windows(4).any(|w| w == [0xF3, 0x0F, 0x5A, 0xC0]),
            "expected a `cvtss2sd` for `to_f64`"
        );
    }

    #[test]
    fn f32_operations_round_to_single_precision() {
        // An f32 add must use `addss` (single precision), not `addsd`. This is the
        // rounding guarantee that keeps native f32 bit-identical to the
        // interpreter's real `f32` storage.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let a f32 = to_f32(1.0)\n",
            "    let b f32 = to_f32(2.0)\n",
            "    let s f32 = a + b\n",
            "    let flag i64 = 0\n",
            "    if s > to_f32(2.0)\n",
            "        flag = 1\n",
            "    flag\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);

        let text = text_bytes(&program);
        // `addss xmm0, xmm1` (F3 0F 58 C1) — single-precision add.
        assert!(
            text.windows(4).any(|w| w == [0xF3, 0x0F, 0x58, 0xC1]),
            "f32 add must be single-precision `addss`"
        );
        // No `addsd` (F2 0F 58 C1) — the f32 path must never widen to double.
        assert!(
            !text.windows(4).any(|w| w == [0xF2, 0x0F, 0x58, 0xC1]),
            "f32 add must not use the double-precision `addsd`"
        );
        // `ucomiss xmm0, xmm1` (0F 2E C1) — single-precision compare for `>`.
        assert!(
            text.windows(3).any(|w| w == [0x0F, 0x2E, 0xC1]),
            "f32 comparison must use `ucomiss`"
        );
    }

    #[test]
    fn function_with_float_signature_compiles_natively() {
        // A function with a float parameter and float return is now a register
        // scalar routed through the SSE registers: it compiles natively (its float
        // parameter is spilled from `xmm0` and its float return is left in `xmm0`)
        // rather than being demoted to the interpreters.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn scale x f64 -> f64\n",
            "    x * 2.0\n\n",
            "fn main -> i64\n",
            "    let n i64 = 7\n",
            "    n\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"scale".to_string()),
            "expected `scale` to compile natively: {:?}",
            program.compiled
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        let text = text_bytes(&program);
        // `scale`'s prologue spills its `f64` parameter from xmm0: `movsd
        // [rbp+disp32], xmm0` = F2 0F 11 85 ...
        assert!(
            text.windows(4).any(|w| w == [0xF2, 0x0F, 0x11, 0x85]),
            "expected the float parameter spilled from xmm0"
        );
    }

    #[test]
    fn function_with_six_i64_params_compiles_with_stack_args() {
        // A six-parameter i64 function is no longer demoted: its 5th and 6th
        // arguments pass on the stack (Win64 stack-argument ABI). Both the callee
        // (`six`) and the caller (`main`) must compile natively.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn six a i64 b i64 c i64 d i64 e i64 f i64 -> i64\n",
            "    a + b + c + d + e + f\n\n",
            "fn main -> i64\n",
            "    six(1, 2, 3, 4, 5, 6)\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"six".to_string())
                && program.compiled.contains(&"main".to_string()),
            "expected `six` and `main` compiled: {:?} / skipped {:?}",
            program.compiled,
            program.skipped,
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);

        let text = text_bytes(&program);
        // Callee prologue: the 5th parameter (0-indexed stack slot 0) is loaded
        // from `[rbp + 48]` (16 for saved rbp + return address, then 32 shadow) =
        // `48 8B 85 30 00 00 00` (mov rax, [rbp+0x30]).
        assert!(
            text.windows(7)
                .any(|w| w == [0x48, 0x8B, 0x85, 0x30, 0x00, 0x00, 0x00]),
            "expected the 5th parameter loaded from [rbp+48] in the callee prologue"
        );
        // The 6th parameter is loaded from `[rbp + 56]` = `48 8B 85 38 00 00 00`.
        assert!(
            text.windows(7)
                .any(|w| w == [0x48, 0x8B, 0x85, 0x38, 0x00, 0x00, 0x00]),
            "expected the 6th parameter loaded from [rbp+56] in the callee prologue"
        );
        // Caller call site: the 5th argument is written into the outgoing area at
        // `[rsp + 32 + ...]` after staging six words (n=6): disp = 8*6 + 32 = 80 =
        // 0x50 => `48 89 84 24 50 00 00 00` (mov [rsp+0x50], rax).
        assert!(
            text.windows(8)
                .any(|w| w == [0x48, 0x89, 0x84, 0x24, 0x50, 0x00, 0x00, 0x00]),
            "expected the 5th argument stored to the outgoing stack area [rsp+0x50]"
        );
        // The 6th argument is written to `[rsp + 88]` = `[rsp + 0x58]`.
        assert!(
            text.windows(8)
                .any(|w| w == [0x48, 0x89, 0x84, 0x24, 0x58, 0x00, 0x00, 0x00]),
            "expected the 6th argument stored to the outgoing stack area [rsp+0x58]"
        );
    }

    #[test]
    fn function_with_eight_i64_params_compiles_with_stack_args() {
        // Eight i64 parameters: arguments 5..=8 spill to the stack. Verifies the
        // arity is not capped and the callee reads its four stack parameters from
        // ascending `[rbp + 16 + 8*k]` offsets.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn eight a i64 b i64 c i64 d i64 e i64 f i64 g i64 h i64 -> i64\n",
            "    a + b + c + d + e + f + g + h\n\n",
            "fn main -> i64\n",
            "    eight(1, 2, 3, 4, 5, 6, 7, 8)\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"eight".to_string())
                && program.compiled.contains(&"main".to_string()),
            "expected `eight` and `main` compiled: {:?} / skipped {:?}",
            program.compiled,
            program.skipped,
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);

        let text = text_bytes(&program);
        // The 8th (last) parameter is at `[rbp + 48 + 8*3]` = `[rbp + 72]` = 0x48.
        assert!(
            text.windows(7)
                .any(|w| w == [0x48, 0x8B, 0x85, 0x48, 0x00, 0x00, 0x00]),
            "expected the 8th parameter loaded from [rbp+72] in the callee prologue"
        );
    }

    #[test]
    fn function_with_mixed_int_float_params_beyond_four_compiles() {
        // A six-parameter signature mixing i64 and f64: the integer and float
        // registers are consumed positionally (`rcx`/`rdx`, `xmm2`, `r8`; then
        // stack), and the 5th/6th arguments spill onto the stack. It must compile,
        // proving float and integer stack arguments coexist.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn scale a i64 b i64 x f64 c i64 d i64 y f64 -> i64\n",
            "    let base i64 = a + b + c + d\n",
            "    if x < y\n",
            "        return base + 1\n",
            "    return base\n\n",
            "fn main -> i64\n",
            "    scale(10, 20, 1.5, 5, 5, 2.5)\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"scale".to_string())
                && program.compiled.contains(&"main".to_string()),
            "expected `scale` and `main` compiled: {:?} / skipped {:?}",
            program.compiled,
            program.skipped,
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    }

    #[test]
    fn f32_precision_loss_matches_interpreter_semantics() {
        // The `run_f32.lby` scenario in miniature: 2^24 + 1 rounds back to 2^24 in
        // f32 (single precision cannot represent the extra bit), so the equality
        // holds and the function compiles natively. This is the exact case that
        // would fail if the f32 add were done in double precision.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let big f32 = to_f32(16777216.0)\n",
            "    let bumped f32 = big + to_f32(1.0)\n",
            "    let same i64 = 0\n",
            "    if bumped == big\n",
            "        same = 1\n",
            "    same\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);

        let text = text_bytes(&program);
        // The equality compare emits `sete al` + `setnp cl` + `and al, cl` so a
        // NaN operand yields the interpreter's `false`; here it also proves the
        // ordered-equality lowering is present.
        assert!(
            text.windows(3).any(|w| w == [0x0F, 0x94, 0xC0])
                && text.windows(3).any(|w| w == [0x0F, 0x9B, 0xC1]),
            "expected ordered f32 equality lowering (sete + setnp)"
        );
    }

    /// Locate a COFF section by name, returning its raw-data byte range
    /// `(start, len)` in the object. Reads the section-header table
    /// (`NumberOfSections` at header offset 2) and matches the 8-byte name field.
    fn coff_section(bytes: &[u8], name: &str) -> Option<(usize, usize)> {
        let num_sections = read_u16(bytes, 2) as usize;
        let mut padded = [0u8; 8];
        padded[..name.len()].copy_from_slice(name.as_bytes());
        for i in 0..num_sections {
            let hdr = COFF_HEADER_SIZE as usize + i * SECTION_HEADER_SIZE as usize;
            if bytes[hdr..hdr + 8] == padded {
                let size = read_u32(bytes, hdr + 16) as usize;
                let ptr = read_u32(bytes, hdr + 20) as usize;
                return Some((ptr, size));
            }
        }
        None
    }

    #[test]
    fn debug_off_is_byte_for_byte_identical_to_default() {
        // `--debug` must be strictly additive: with debug info off (or via the
        // non-debug entry point) the object bytes are exactly as before, so the
        // existing structural/snapshot tests are unaffected.
        let module = module_for("fn add x i64 -> i64\n    x + 1\n\nfn main -> i64\n    add(41)\n");
        let default = emit_alpha1_native_program(&module).expect("default emit");
        let no_debug = emit_alpha1_native_program_with_debug(&module, None).expect("no-debug emit");
        assert_eq!(default.bytes, no_debug.bytes, "no-debug path is unchanged");
        // And the default object carries no `.debug$S` section.
        assert!(
            coff_section(&default.bytes, ".debug$S").is_none(),
            "default object has no debug section"
        );
    }

    #[test]
    fn emits_codeview_debug_section_with_per_function_line_info() {
        // A small multi-function program. `add` is declared on source line 1 and
        // `main` on line 4 (1-based; line 3 is the blank separator). With
        // `--debug` the object must carry a well-formed CodeView `.debug$S`
        // section: the C13 signature, a DEBUG_S_LINES subsection per function, the
        // source file name in the string table, and a line record mapping each
        // function's entry offset (0) to its declaration line.
        let src = "fn add x i64 -> i64\n    x + 1\n\nfn main -> i64\n    add(41)\n";
        let module = module_for(src);
        let debug = DebugOptions {
            source_file: "sample.lby".to_string(),
        };
        let program =
            emit_alpha1_native_program_with_debug(&module, Some(&debug)).expect("debug emit");

        // The `.debug$S` section exists.
        let (start, len) = coff_section(&program.bytes, ".debug$S").expect("debug section present");
        let section = &program.bytes[start..start + len];

        // CodeView C13 signature leads the section.
        assert_eq!(read_u32(section, 0), CV_SIGNATURE_C13, "C13 signature");

        // Walk the subsections; collect the kinds seen and every DEBUG_S_LINES's
        // recorded line number (the packed value with the statement bit cleared).
        let mut kinds: Vec<u32> = Vec::new();
        let mut lines_records: Vec<u32> = Vec::new();
        let mut cursor = 4usize;
        while cursor + 8 <= section.len() {
            let kind = read_u32(section, cursor);
            let sub_len = read_u32(section, cursor + 4) as usize;
            let data_start = cursor + 8;
            kinds.push(kind);
            if kind == DEBUG_S_LINES {
                // Header is 12 bytes: offset(4) seg(2) flags(2) codesize(4). Then
                // the file block header is 12 bytes, then line pairs of 8 bytes.
                let first_line_off = data_start + 12 + 12;
                let packed = read_u32(section, first_line_off + 4);
                lines_records.push(packed & 0x7FFF_FFFF);
                // The line entry's function-relative offset is 0 (function entry).
                assert_eq!(
                    read_u32(section, first_line_off),
                    0,
                    "line record sits at the function entry offset"
                );
            }
            cursor = data_start + sub_len;
            // Subsections are 4-byte aligned.
            while !cursor.is_multiple_of(4) {
                cursor += 1;
            }
        }

        assert!(
            kinds.contains(&DEBUG_S_SYMBOLS),
            "a DEBUG_S_SYMBOLS subsection is present"
        );
        assert!(
            kinds.contains(&DEBUG_S_FILECHKSMS),
            "a DEBUG_S_FILECHKSMS subsection is present"
        );
        assert!(
            kinds.contains(&DEBUG_S_STRINGTABLE),
            "a DEBUG_S_STRINGTABLE subsection is present"
        );
        // One DEBUG_S_LINES per compiled function (add + main).
        assert_eq!(
            kinds.iter().filter(|&&k| k == DEBUG_S_LINES).count(),
            2,
            "one line subsection per function"
        );

        // The recorded lines are exactly the two declaration lines: `add` on line
        // 1 and `main` on line 4.
        lines_records.sort_unstable();
        assert_eq!(lines_records, vec![1, 4], "per-function declaration lines");

        // The source file name is recorded in the CodeView string table.
        assert!(
            section
                .windows(b"sample.lby".len())
                .any(|w| w == b"sample.lby"),
            "source file name recorded in the debug section"
        );

        // The section carries relocations against the `.text` function symbols
        // (SECREL32 + SECTION per function = 4 total for two functions).
        let num_sections = read_u16(&program.bytes, 2) as usize;
        let mut debug_hdr = None;
        for i in 0..num_sections {
            let hdr = COFF_HEADER_SIZE as usize + i * SECTION_HEADER_SIZE as usize;
            if &program.bytes[hdr..hdr + 8] == b".debug\x24S" {
                debug_hdr = Some(hdr);
            }
        }
        let hdr = debug_hdr.expect("debug section header");
        assert_eq!(
            read_u16(&program.bytes, hdr + 32),
            4,
            "two SECREL32+SECTION relocation pairs for two functions"
        );
    }

    /// The compiled `.text` bytes contain a tag load followed by a conditional
    /// branch — the signature of a native `match` dispatch (`cmp rax, imm32`
    /// then `jne rel32`). Used by the enum tests below.
    fn has_tag_dispatch(text: &[u8]) -> bool {
        // `cmp rax, imm32` is `48 3D` + 4 bytes; a `jne rel32` is `0F 85` + 4.
        text.windows(2).any(|w| w == [0x48, 0x3D]) && text.windows(2).any(|w| w == [0x0F, 0x85])
    }

    #[test]
    fn compiles_option_match_natively() {
        // A function that builds an `option<i64>` local and matches both arms is
        // compiled to native code (tag dispatch + payload binding), not skipped.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn some_path -> i64\n",
            "    let hit option<i64> = some(40)\n",
            "    match hit\n",
            "        some(v) -> v + 2\n",
            "        none -> 7\n\n",
            "fn main -> i64\n",
            "    some_path()\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"some_path".to_string()),
            "expected `some_path` compiled: {:?} / skipped {:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.compiled.contains(&"main".to_string()));
        assert!(
            has_tag_dispatch(text_bytes(&program)),
            "expected a tag load + conditional branch for the option match"
        );
    }

    #[test]
    fn compiles_result_scalar_match_natively() {
        // A `result<i64, i64>` (both arms scalar) compiles natively: `ok`/`err`
        // are tags 0/1, and each arm binds its scalar payload.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn ok_path -> i64\n",
            "    let r result<i64, i64> = ok(30)\n",
            "    match r\n",
            "        ok(q) -> q + 5\n",
            "        err(e) -> e\n\n",
            "fn main -> i64\n",
            "    ok_path()\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"ok_path".to_string()),
            "expected `ok_path` compiled: {:?} / skipped {:?}",
            program.compiled,
            program.skipped
        );
        assert!(has_tag_dispatch(text_bytes(&program)));
    }

    #[test]
    fn compiles_user_enum_match_with_wildcard_natively() {
        // A user enum with scalar payloads and a wildcard arm compiles natively.
        // The match is inside an i64-only function so the whole function is
        // native-eligible.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "enum Signal\n",
            "    Steady\n",
            "    Pulse i64\n",
            "    Burst i64\n\n",
            "fn score kind i64 amount i64 -> i64\n",
            "    let s Signal = Steady\n",
            "    if kind == 1\n",
            "        s = Pulse(amount)\n",
            "    match s\n",
            "        Pulse(n) -> n + 1\n",
            "        _ -> 100\n\n",
            "fn main -> i64\n",
            "    score(1, 5)\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"score".to_string()),
            "expected `score` compiled: {:?} / skipped {:?}",
            program.compiled,
            program.skipped
        );
        let text = text_bytes(&program);
        assert!(
            has_tag_dispatch(text),
            "expected a tag load + conditional branch for the user-enum match"
        );
        // A wildcard covers the fallthrough, so no unreachable `ud2` (0F 0B) trap
        // is emitted for this match.
        assert!(
            !text.windows(2).any(|w| w == [0x0F, 0x0B]),
            "a wildcard-terminated match should not emit an unreachable trap"
        );
    }

    #[test]
    fn exhaustive_variant_match_emits_unreachable_trap() {
        // An exhaustive variant match with no wildcard (e.g. `option` some/none)
        // ends with a `ud2` (0F 0B) on the impossible fallthrough, since
        // exhaustiveness guarantees a variant arm matched.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn some_path -> i64\n",
            "    let hit option<i64> = some(40)\n",
            "    match hit\n",
            "        some(v) -> v + 2\n",
            "        none -> 7\n\n",
            "fn main -> i64\n",
            "    some_path()\n",
        )))
        .expect("emit native program");
        let text = text_bytes(&program);
        assert!(
            text.windows(2).any(|w| w == [0x0F, 0x0B]),
            "expected a `ud2` trap for the wildcard-free exhaustive match"
        );
    }

    #[test]
    fn compiles_enum_returning_call_and_match_on_it() {
        // A function that returns an enum (by the hidden-pointer aggregate return
        // ABI) and a caller that matches that call result now both compile.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn lookup key i64 -> option<i64>\n",
            "    if key == 1\n",
            "        return some(11)\n",
            "    none\n\n",
            "fn use_lookup key i64 -> i64\n",
            "    match lookup(key)\n",
            "        some(v) -> v + 3\n",
            "        none -> 1\n\n",
            "fn main -> i64\n",
            "    use_lookup(1)\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"lookup".to_string())
                && program.compiled.contains(&"use_lookup".to_string())
                && program.compiled.contains(&"main".to_string()),
            "enum-returning call + match must compile: {:?} / {:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    }

    #[test]
    fn compiles_result_with_string_payload_natively() {
        // A `result<i64, string>` now COMPILES: the `err` payload is an immutable
        // string pointer stored in one payload word, matched and bound as a flat
        // word (shared, never deep-recursed). Both arms are exercised; the tag
        // dispatch is the same as any other native match.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn classify n i64 -> i64\n",
            "    let r result<i64, string> = ok(n)\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> len(m)\n\n",
            "fn main -> i64\n",
            "    classify(3)\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"classify".to_string())
                && program.compiled.contains(&"main".to_string()),
            "string-payload result must compile: {:?} / skipped {:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        assert!(
            has_tag_dispatch(text_bytes(&program)),
            "expected tag load + conditional branch for the string-payload result match"
        );
    }

    #[test]
    fn compiles_option_string_and_user_string_enum_natively() {
        // `option<string>` (the shape `map_get` on a `map<K, string>` returns) and a
        // user enum with a `string` payload both compile: the `some`/`Named` payload
        // slot is the immutable string pointer, bound as a flat word.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "enum Tag\n",
            "    Named string\n",
            "    Anon\n\n",
            "fn opt_len o option<string> -> i64\n",
            "    match o\n",
            "        some(s) -> len(s)\n",
            "        none -> 0\n\n",
            "fn tag_len t Tag -> i64\n",
            "    match t\n",
            "        Named(name) -> len(name)\n",
            "        Anon -> 0\n\n",
            "fn main -> i64\n",
            "    opt_len(some(\"ab\")) + tag_len(Named(\"cde\"))\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"opt_len".to_string())
                && program.compiled.contains(&"tag_len".to_string())
                && program.compiled.contains(&"main".to_string()),
            "option<string>/user string-payload enum must compile: {:?} / skipped {:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    }

    #[test]
    fn compiles_result_with_one_level_mutable_heap_payload() {
        // A `result<i64, list<i64>>` carries a ONE-LEVEL mutable heap payload in
        // `err`; the payload is deep-copied on the enum's value-semantic copy, so it
        // is now IN the native subset (mirroring the WASM backend) and compiles.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn classify n i64 -> i64\n",
            "    let xs list<i64> = list_new()\n",
            "    let r result<i64, list<i64>> = err(push(xs, n))\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> len(m)\n\n",
            "fn main -> i64\n",
            "    classify(3)\n",
        )))
        .expect("program with a one-level mutable enum payload compiles");
        assert!(
            program.compiled.iter().any(|n| n == "classify"),
            "expected `classify` to compile: skipped {:?}",
            program.skipped
        );
    }

    #[test]
    fn defers_enum_with_two_level_mutable_heap_payload_gracefully() {
        // A `result<i64, list<list<list<i64>>>>` payload nests MUTABLE aggregates
        // past the one-level bound (`list<list<list<…>>>`), so it is still out of the
        // native subset and the function skips gracefully rather than miscompiling.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn classify n i64 -> i64\n",
            "    let r result<i64, list<list<list<i64>>>> = ok(n)\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> len(m)\n\n",
            "fn main -> i64\n",
            "    classify(3)\n",
        )));
        match program {
            Err(err) => assert!(
                err.skipped.iter().any(|s| s.name == "classify"),
                "expected `classify` skipped for its over-deep payload: {:?}",
                err.skipped
            ),
            Ok(program) => assert!(
                program.skipped.iter().any(|s| s.name == "classify"),
                "expected `classify` skipped for its over-deep payload: {:?}",
                program.skipped
            ),
        }
    }

    // -- Mutable-heap collection elements (list<struct>, list<list>, map<K,struct>) --

    #[test]
    fn compiles_list_of_structs_natively() {
        // A `list<struct>` now COMPILES: each element is a heap-struct pointer,
        // deep-copied per element on the list's value-semantic copy. `push`/`get`/
        // `set` cross the heap<->stack bridge (a struct field-block behind a pointer
        // for the element; a stack-flattened struct for the returned/consumed value).
        let program = emit_alpha1_native_program(&module_for(concat!(
            "struct Point\n",
            "    x i64\n",
            "    y i64\n\n",
            "fn sum p Point -> i64\n",
            "    p.x + p.y\n\n",
            "fn build -> i64\n",
            "    let ps list<Point> = list_new()\n",
            "    ps = push(ps, Point(1, 2))\n",
            "    ps = push(ps, Point(3, 4))\n",
            "    sum(get(ps, 1))\n\n",
            "fn main -> i64\n",
            "    build()\n",
        )))
        .expect("emit native program for list<struct>");
        assert!(
            program.compiled.contains(&"build".to_string())
                && program.compiled.contains(&"main".to_string()),
            "list<struct> must compile: compiled {:?} / skipped {:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        // The recursive per-element deep copy routes through the heap-struct copy
        // helper, so the object references `__lullaby_struct_copy`.
        assert!(
            coff_symbol(&program.bytes, STRUCT_COPY_SYMBOL).is_some(),
            "list<struct> deep copy must reference the heap-struct copy helper"
        );
    }

    #[test]
    fn compiles_nested_list_of_lists_natively() {
        // A `list<list<i64>>` COMPILES: the outer element is a list pointer (already
        // one word); the outer copy deep-copies each inner list via the list-copy
        // helper (one mutable level, inner elements scalar). `get` returns an
        // independent inner list.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn build -> i64\n",
            "    let a list<i64> = list_new()\n",
            "    a = push(a, 5)\n",
            "    let rows list<list<i64>> = list_new()\n",
            "    rows = push(rows, a)\n",
            "    let r list<i64> = get(rows, 0)\n",
            "    get(r, 0)\n\n",
            "fn main -> i64\n",
            "    build()\n",
        )))
        .expect("emit native program for list<list<i64>>");
        assert!(
            program.compiled.contains(&"build".to_string()),
            "list<list<i64>> must compile: skipped {:?}",
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        assert!(
            coff_symbol(&program.bytes, LIST_COPY_SYMBOL).is_some(),
            "nested list deep copy must reference the list-copy helper"
        );
    }

    #[test]
    fn compiles_map_of_structs_natively() {
        // A `map<i64, struct>` COMPILES: each entry value is a heap-struct pointer,
        // deep-copied per value on the map's value-semantic copy; `map_get` returns
        // `option<struct>` whose `some` payload is an independent heap struct.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "struct Point\n",
            "    x i64\n",
            "    y i64\n\n",
            "fn sum p Point -> i64\n",
            "    p.x + p.y\n\n",
            "fn build -> i64\n",
            "    let m map<i64, Point> = map_new()\n",
            "    m = map_set(m, 1, Point(2, 3))\n",
            "    match map_get(m, 1)\n",
            "        some(p) -> sum(p)\n",
            "        none -> 0\n\n",
            "fn main -> i64\n",
            "    build()\n",
        )))
        .expect("emit native program for map<K, struct>");
        assert!(
            program.compiled.contains(&"build".to_string()),
            "map<i64, struct> must compile: skipped {:?}",
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        assert!(
            coff_symbol(&program.bytes, STRUCT_COPY_SYMBOL).is_some(),
            "map<K, struct> deep copy must reference the heap-struct copy helper"
        );
    }

    #[test]
    fn heap_struct_copy_helper_is_emitted_and_recurses_via_alloc() {
        // The `__lullaby_struct_copy` helper is a real function in `.text` that calls
        // the bump allocator (a fresh, independent block) — the machine-code proof
        // that a heap-struct element deep copy is recursive, not a shared pointer.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "struct Cell\n",
            "    v i64\n\n",
            "fn read c Cell -> i64\n",
            "    c.v\n\n",
            "fn build -> i64\n",
            "    let xs list<Cell> = list_new()\n",
            "    xs = push(xs, Cell(7))\n",
            "    read(get(xs, 0))\n\n",
            "fn main -> i64\n",
            "    build()\n",
        )))
        .expect("emit native program");
        assert!(
            coff_symbol(&program.bytes, STRUCT_COPY_SYMBOL).is_some(),
            "the heap-struct copy helper must be emitted"
        );
        assert!(
            coff_symbol(&program.bytes, HEAP_ALLOC_SYMBOL).is_some(),
            "the heap-struct copy helper allocates a fresh block via the bump allocator"
        );
    }

    #[test]
    fn defers_list_of_maps_gracefully() {
        // A `list<map<i64, i64>>` element is a MUTABLE `map` — outside the accepted
        // one-level struct/nested-list element set — so the function skips gracefully
        // (still runs on the interpreters) rather than miscompiling.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn build -> i64\n",
            "    let xs list<map<i64, i64>> = list_new()\n",
            "    len(xs)\n\n",
            "fn main -> i64\n",
            "    build()\n",
        )));
        match program {
            Err(err) => assert!(
                err.skipped.iter().any(|s| s.name == "build"),
                "list<map<..>> must skip: {:?}",
                err.skipped
            ),
            Ok(program) => assert!(
                program.skipped.iter().any(|s| s.name == "build"),
                "list<map<..>> must skip: {:?}",
                program.skipped
            ),
        }
    }

    // -- Aggregate parameter / return / call-argument ABI --------------------

    #[test]
    fn compiles_struct_parameter_and_return_with_by_pointer_abi() {
        // A function that TAKES a struct and returns an i64, a function that
        // RETURNS a struct, and a `main` that passes/receives both compile (not
        // skip). The by-pointer argument (`lea rax/rcx, [rbp+disp]` staged into an
        // argument register) and the hidden-return-pointer copy (`mov [rax-disp],
        // rcx` writing result words) must appear in the emitted code.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "struct Point\n    x i64\n    y i64\n\n",
            "fn taxicab p Point -> i64\n    p.x + p.y\n\n",
            "fn shift p Point d i64 -> Point\n    Point(p.x + d, p.y + d)\n\n",
            "fn main -> i64\n",
            "    let base Point = Point(3, 4)\n",
            "    let moved Point = shift(base, 10)\n",
            "    taxicab(base) + taxicab(moved)\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"taxicab".to_string())
                && program.compiled.contains(&"shift".to_string())
                && program.compiled.contains(&"main".to_string()),
            "struct param/return functions must compile: compiled={:?} skipped={:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);

        let text = text_bytes(&program);
        // Hidden-return-pointer write: `mov [rax - 8], rcx` (48 89 88 F8 FF FF FF)
        // — `shift` writes result word 1 through the caller-supplied pointer.
        assert!(
            text.windows(7)
                .any(|w| w == [0x48, 0x89, 0x88, 0xF8, 0xFF, 0xFF, 0xFF]),
            "expected a hidden-return-pointer word write (`mov [rax-8], rcx`)"
        );
        // By-pointer argument: `lea rax, [rbp+disp]` (48 8D 85 ..) stages the
        // address of a materialized aggregate argument copy before it is pushed.
        assert!(
            text.windows(3).any(|w| w == [0x48, 0x8D, 0x85]),
            "expected a `lea rax, [rbp+disp]` staging an aggregate argument address"
        );
        // Hidden return pointer passed in rcx: `lea rcx, [rbp+disp]` (48 8D 8D ..).
        assert!(
            text.windows(3).any(|w| w == [0x48, 0x8D, 0x8D]),
            "expected a `lea rcx, [rbp+disp]` for the hidden return pointer"
        );
    }

    #[test]
    fn compiles_fixed_array_parameter_and_return() {
        // A function taking a fixed array and one returning a fixed array compile;
        // the array lengths are inferred from the call sites / returned literal.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn sum_array xs array<i64> -> i64\n",
            "    let total i64 = 0\n",
            "    for i from 0 to len(xs) - 1\n",
            "        total += xs[i]\n",
            "    total\n\n",
            "fn doubled xs array<i64> -> array<i64>\n",
            "    let out array<i64> = [0, 0, 0]\n",
            "    for i from 0 to len(xs) - 1\n",
            "        out[i] = xs[i] * 2\n",
            "    out\n\n",
            "fn main -> i64\n",
            "    let data array<i64> = [1, 2, 3]\n",
            "    let d array<i64> = doubled(data)\n",
            "    sum_array(data) + d[0] + d[1] + d[2]\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"sum_array".to_string())
                && program.compiled.contains(&"doubled".to_string())
                && program.compiled.contains(&"main".to_string()),
            "array param/return functions must compile: compiled={:?} skipped={:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    }

    #[test]
    fn aggregate_parameter_copies_in_for_value_semantics() {
        // A struct parameter is copied into the callee's frame in the prologue
        // (`mov rcx, [rax - disp]` then `mov [rbp - slot], rcx`), so mutating the
        // parameter cannot affect the caller's copy. The prologue copy-in loads
        // from the argument pointer via `mov rcx, [rax + disp]` (48 8B 88 ..).
        let program = emit_alpha1_native_program(&module_for(concat!(
            "struct Box\n    a i64\n    b i64\n\n",
            "fn clobber s Box -> i64\n",
            "    s.a = s.a + 1\n",
            "    s.a + s.b\n\n",
            "fn main -> i64\n",
            "    let box Box = Box(10, 20)\n",
            "    let inside i64 = clobber(box)\n",
            "    inside + box.a + box.b\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"clobber".to_string()),
            "clobber must compile: {:?} / {:?}",
            program.compiled,
            program.skipped
        );
        let text = text_bytes(&program);
        // Copy-in read from the argument pointer: `mov rcx, [rax + disp32]`
        // (48 8B 88 ..) — the callee reads the caller's snapshot word-by-word.
        assert!(
            text.windows(3).any(|w| w == [0x48, 0x8B, 0x88]),
            "expected an aggregate-parameter copy-in read (`mov rcx, [rax+disp]`)"
        );
    }

    #[test]
    fn compiles_enum_parameter_and_return_and_match_on_call() {
        // An `option<i64>` (a scalar-payload enum) as a parameter and a return
        // type compiles, including a `match` on an enum-returning call.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn classify n i64 -> option<i64>\n",
            "    if n > 0\n",
            "        return some(n)\n",
            "    none\n\n",
            "fn unwrap_or o option<i64> d i64 -> i64\n",
            "    match o\n",
            "        some(v) -> v\n",
            "        none -> d\n\n",
            "fn direct n i64 -> i64\n",
            "    match classify(n)\n",
            "        some(v) -> v + 1\n",
            "        none -> 0\n\n",
            "fn main -> i64\n",
            "    unwrap_or(classify(2), 9) + direct(0)\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"classify".to_string())
                && program.compiled.contains(&"unwrap_or".to_string())
                && program.compiled.contains(&"direct".to_string()),
            "enum param/return/match-on-call must compile: {:?} / {:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    }

    #[test]
    fn defers_heap_containing_aggregate_parameter() {
        // A struct field of a heap type (`string`) is not a native scalar-field
        // aggregate, so a function taking it by value skips gracefully.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "struct Named\n    id i64\n    label string\n\n",
            "fn id_of n Named -> i64\n    n.id\n\n",
            "fn main -> i64\n    7\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(
            program.skipped.iter().any(|s| s.name == "id_of"),
            "heap-field aggregate parameter must skip: {:?}",
            program.skipped
        );
    }

    #[test]
    fn aggregate_return_with_four_params_uses_a_stack_argument() {
        // Four scalar parameters plus a hidden return pointer make five effective
        // register arguments; the fifth (the last parameter) now spills to the
        // stack rather than demoting the function. The callee reads its last
        // parameter from `[rbp+16]` and `main`'s call passes the hidden result
        // pointer in `rcx` and the 5th effective argument on the stack.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "struct Quad\n    a i64\n    b i64\n\n",
            "fn build w i64 x i64 y i64 z i64 -> Quad\n    Quad(w + x, y + z)\n\n",
            "fn main -> i64\n    let q Quad = build(1, 2, 3, 4)\n    q.a + q.b\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"build".to_string())
                && program.compiled.contains(&"main".to_string()),
            "aggregate return with four params must compile via a stack argument: {:?} / {:?}",
            program.compiled,
            program.skipped,
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);

        let text = text_bytes(&program);
        // `build`'s prologue reads its 4th (0-indexed effective position 4, since
        // the hidden return pointer is position 0) parameter `z` from `[rbp+48]`.
        assert!(
            text.windows(7)
                .any(|w| w == [0x48, 0x8B, 0x85, 0x30, 0x00, 0x00, 0x00]),
            "expected the 5th effective argument (param `z`) loaded from [rbp+48]"
        );
    }

    // -- Growable list<T> (scalar element) native codegen --------------------

    #[test]
    fn compiles_growable_list_function_natively() {
        // A function that builds a scalar-element `list<i64>` via `list_new`/
        // `push`/`set`/`pop`/`get`/`len` — including a signature returning
        // `list<i64>` and one taking it — now compiles natively (not skipped).
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn build -> list<i64>\n",
            "    let xs list<i64> = list_new()\n",
            "    xs = push(xs, 10)\n",
            "    xs = push(xs, 20)\n",
            "    xs\n\n",
            "fn total xs list<i64> -> i64\n",
            "    let ys list<i64> = set(xs, 0, 5)\n",
            "    let zs list<i64> = pop(ys)\n",
            "    get(ys, 0) + len(zs)\n\n",
            "fn main -> i64\n",
            "    let xs list<i64> = build()\n",
            "    total(xs)\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"build".to_string())
                && program.compiled.contains(&"total".to_string())
                && program.compiled.contains(&"main".to_string()),
            "growable list functions must compile: {:?} / {:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    }

    #[test]
    fn list_object_emits_grow_and_copy_helpers() {
        // A list-using program (no string constants) still emits the heap path:
        // the object must contain the `__lullaby_list_new`/`_copy`/`_grow`
        // runtime-helper symbols and the bump allocator, proving grow/copy codegen
        // is present.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let xs list<i64> = list_new()\n",
            "    xs = push(xs, 1)\n",
            "    xs = push(xs, 2)\n",
            "    len(xs)\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        // The three list helpers + the bump allocator are external-defined symbols.
        for symbol in [
            LIST_NEW_SYMBOL,
            LIST_COPY_SYMBOL,
            LIST_GROW_SYMBOL,
            HEAP_ALLOC_SYMBOL,
        ] {
            let (section, _storage) =
                coff_symbol(&program.bytes, symbol).unwrap_or_else(|| panic!("missing {symbol}"));
            assert_eq!(section, 1, "{symbol} must be defined in .text");
        }
        // The bump-heap `.bss` cell/region symbols must be present too.
        assert!(
            coff_symbol(&program.bytes, HEAP_BASE_SYMBOL).is_some(),
            "the .bss heap region must be present for a list-using object"
        );
    }

    #[test]
    fn push_call_site_calls_copy_then_grow() {
        // A `push` call site deep-copies the source list (value semantics) and then
        // grows it, so `main`'s text carries relocations against BOTH the copy and
        // grow helpers.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let xs list<i64> = list_new()\n",
            "    xs = push(xs, 7)\n",
            "    get(xs, 0)\n",
        )))
        .expect("emit native program");
        let main = program
            .compiled
            .iter()
            .position(|n| n == "main")
            .expect("main compiled");
        assert_eq!(main, 0);
        // The object references the copy + grow helpers (proving push emits both).
        assert!(
            coff_symbol(&program.bytes, LIST_COPY_SYMBOL).is_some(),
            "push must reference the list-copy helper"
        );
        assert!(
            coff_symbol(&program.bytes, LIST_GROW_SYMBOL).is_some(),
            "push must reference the list-grow helper"
        );
    }

    #[test]
    fn compiles_string_element_list_natively() {
        // A `list<string>` now COMPILES: a `string` element is an immutable heap
        // pointer stored in one slot exactly like a scalar, appended by `push`,
        // loaded back by `get`, and shared (not deep-recursed) on the flat word-copy
        // deep copy. The list header and grow/copy helpers are the same as a scalar
        // list.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn names n i64 -> list<string>\n",
            "    let xs list<string> = list_new()\n",
            "    xs = push(xs, \"a\")\n",
            "    xs = push(xs, to_string(n))\n",
            "    xs\n\n",
            "fn head l list<string> -> i64\n",
            "    len(get(l, 0))\n\n",
            "fn main -> i64\n",
            "    head(names(3)) + len(names(3))\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"names".to_string())
                && program.compiled.contains(&"head".to_string())
                && program.compiled.contains(&"main".to_string()),
            "list<string> functions must compile: {:?} / skipped {:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        assert!(
            coff_symbol(&program.bytes, LIST_COPY_SYMBOL).is_some()
                && coff_symbol(&program.bytes, LIST_GROW_SYMBOL).is_some(),
            "list<string> value-semantics still reference the list copy/grow helpers"
        );
    }

    #[test]
    fn defers_mutable_heap_element_list_gracefully() {
        // A `list<array<i64>>` (MUTABLE heap element) is still DEFERRED: it would
        // need a recursive per-element deep copy, so the enclosing function skips
        // with a clear reason and still runs on the interpreters — never
        // miscompiled.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn grid -> list<array<i64>>\n",
            "    list_new()\n\n",
            "fn main -> i64\n",
            "    len(grid())\n",
        )));
        // `main` calls `grid` (which is skipped), so `main` demotes too; the whole
        // program has no eligible function -> the L0339 "nothing eligible" error.
        let err = program.expect_err("mutable-heap-element list must not compile");
        assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
        assert!(
            err.skipped.iter().any(|s| s.name == "grid"),
            "the mutable-heap-element list function must be recorded as skipped: {:?}",
            err.skipped
        );
    }

    #[test]
    fn compiles_float_element_list_natively() {
        // A `list<f64>` (float scalar element) compiles: elements are stored as
        // bit-preserving 8-byte words, and a float `get` moves the word back into
        // an XMM register.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let xs list<f64> = list_new()\n",
            "    xs = push(xs, 1.5)\n",
            "    xs = push(xs, 2.5)\n",
            "    let a f64 = get(xs, 0)\n",
            "    let b f64 = get(xs, 1)\n",
            "    let flag i64 = 0\n",
            "    if a + b > 3.9\n",
            "        flag = 1\n",
            "    flag + len(xs)\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    }

    // -- Growable map<K, V> (scalar key/value) native codegen ----------------

    #[test]
    fn compiles_growable_map_function_natively() {
        // A program that builds a scalar `map<i64, i64>` via `map_new`/`map_set`
        // (including a signature returning `map<i64, i64>`, one taking it, and a
        // `match map_get(...)`), reads it via `map_get`/`map_has`/`map_len` — all
        // compile natively (nothing skipped).
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn build -> map<i64, i64>\n",
            "    let m map<i64, i64> = map_new()\n",
            "    m = map_set(m, 1, 10)\n",
            "    m = map_set(m, 2, 20)\n",
            "    m = map_set(m, 2, 99)\n",
            "    m\n\n",
            "fn lookup m map<i64, i64> k i64 -> i64\n",
            "    match map_get(m, k)\n",
            "        some(v) -> v\n",
            "        none -> 0\n\n",
            "fn main -> i64\n",
            "    let m map<i64, i64> = build()\n",
            "    let has1 i64 = 0\n",
            "    if map_has(m, 1)\n",
            "        has1 = 1\n",
            "    map_len(m) + lookup(m, 2) + has1\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"build".to_string())
                && program.compiled.contains(&"lookup".to_string())
                && program.compiled.contains(&"main".to_string()),
            "growable map functions must compile: {:?} / {:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    }

    #[test]
    fn map_object_emits_map_and_alloc_helpers() {
        // A map-using program (no string constants) still emits the heap path: the
        // object must contain the `__lullaby_map_new`/`_copy`/`_grow`/`_find`
        // runtime-helper symbols and the bump allocator, proving map codegen is
        // present and defined in `.text`.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let m map<i64, i64> = map_new()\n",
            "    m = map_set(m, 1, 5)\n",
            "    m = map_set(m, 2, 6)\n",
            "    map_len(m)\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        for symbol in [
            MAP_NEW_SYMBOL,
            MAP_COPY_SYMBOL,
            MAP_GROW_SYMBOL,
            MAP_FIND_SYMBOL,
            HEAP_ALLOC_SYMBOL,
        ] {
            let (section, _storage) =
                coff_symbol(&program.bytes, symbol).unwrap_or_else(|| panic!("missing {symbol}"));
            assert_eq!(section, 1, "{symbol} must be defined in .text");
        }
        assert!(
            coff_symbol(&program.bytes, HEAP_BASE_SYMBOL).is_some(),
            "the .bss heap region must be present for a map-using object"
        );
    }

    #[test]
    fn map_set_call_site_calls_copy_then_find() {
        // A `map_set` call site deep-copies the source map (value semantics) and
        // then scans it, so `main`'s object carries relocations against BOTH the
        // map-copy and the map-find helpers.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let m map<i64, i64> = map_new()\n",
            "    m = map_set(m, 3, 7)\n",
            "    map_len(m)\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert!(
            coff_symbol(&program.bytes, MAP_COPY_SYMBOL).is_some(),
            "map_set must reference the map-copy helper"
        );
        assert!(
            coff_symbol(&program.bytes, MAP_FIND_SYMBOL).is_some(),
            "map_set must reference the map-find helper"
        );
    }

    #[test]
    fn compiles_float_value_map_natively() {
        // A `map<i64, f64>` (float value) compiles: values are stored/loaded as
        // bit-preserving 8-byte words, and a `some(v)` float payload round-trips
        // through the option layout.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn probe m map<i64, f64> k i64 -> i64\n",
            "    match map_get(m, k)\n",
            "        some(v) -> 1\n",
            "        none -> 0\n\n",
            "fn main -> i64\n",
            "    let m map<i64, f64> = map_new()\n",
            "    m = map_set(m, 1, 1.5)\n",
            "    m = map_set(m, 2, 2.5)\n",
            "    probe(m, 2) + map_len(m)\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"probe".to_string())
                && program.compiled.contains(&"main".to_string()),
            "float-value map functions must compile: {:?} / {:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    }

    #[test]
    fn defers_string_key_map_gracefully() {
        // A `map<string, i64>` (heap key) is DEFERRED: the enclosing function skips
        // with a clear reason and still runs on the interpreters — never
        // miscompiled. String-key equality needs the string heap (content
        // comparison), matching the WASM map's first increment.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn build -> map<string, i64>\n",
            "    map_set(map_new(), \"a\", 1)\n\n",
            "fn main -> i64\n",
            "    map_len(build())\n",
        )));
        let err = program.expect_err("string-key map must not compile");
        assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
        assert!(
            err.skipped
                .iter()
                .any(|s| s.name == "build" && s.reason.contains("map")),
            "the skip reason must cite the deferred map key/value: {:?}",
            err.skipped
        );
    }

    #[test]
    fn compiles_string_value_map_natively() {
        // A `map<i64, string>` (scalar key, `string` value) now COMPILES: the value
        // slot holds an immutable string pointer, shared on the flat two-word entry
        // copy. `map_set` inserts/updates the pointer, `map_get` returns
        // `option<string>` (the `some` payload slot is the string pointer), and
        // `map_has`/`map_len` work unchanged.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn build n i64 -> map<i64, string>\n",
            "    let m map<i64, string> = map_new()\n",
            "    m = map_set(m, 1, \"a\")\n",
            "    m = map_set(m, 2, to_string(n))\n",
            "    m = map_set(m, 1, \"zz\")\n",
            "    m\n\n",
            "fn probe n i64 -> i64\n",
            "    let m map<i64, string> = build(n)\n",
            "    let seen i64 = 0\n",
            "    if map_has(m, 2)\n",
            "        seen = 1\n",
            "    match map_get(m, 1)\n",
            "        some(s) -> len(s) + seen + map_len(m)\n",
            "        none -> 0\n\n",
            "fn main -> i64\n",
            "    probe(3)\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"build".to_string())
                && program.compiled.contains(&"probe".to_string())
                && program.compiled.contains(&"main".to_string()),
            "map<i64, string> functions must compile: {:?} / skipped {:?}",
            program.compiled,
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        assert!(
            coff_symbol(&program.bytes, MAP_COPY_SYMBOL).is_some()
                && coff_symbol(&program.bytes, MAP_FIND_SYMBOL).is_some(),
            "map<i64, string> value-semantics still reference the map copy/find helpers"
        );
    }

    #[test]
    fn defers_mutable_heap_value_map_gracefully() {
        // A `map<i64, array<i64>>` (MUTABLE heap value) is still DEFERRED: it would
        // need a recursive per-value deep copy, so the enclosing function skips with
        // a clear reason and still runs on the interpreters — never miscompiled.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn build -> map<i64, array<i64>>\n",
            "    map_new()\n\n",
            "fn main -> i64\n",
            "    map_len(build())\n",
        )));
        let err = program.expect_err("mutable-heap-value map must not compile");
        assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
        assert!(
            err.skipped
                .iter()
                .any(|s| s.name == "build" && s.reason.contains("map")),
            "the skip reason must cite the deferred map key/value: {:?}",
            err.skipped
        );
    }

    #[test]
    fn string_value_functions_compile_natively() {
        // A program using first-class string values — a string literal as a value,
        // `+` concatenation, `to_string`, `len` on a string, and a string
        // parameter/return crossing a function boundary — compiles natively (not
        // skipped) across all its functions.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn greeting name string -> string\n",
            "    \"hi \" + name\n\n",
            "fn measure s string -> i64\n",
            "    len(s)\n\n",
            "fn main -> i64\n",
            "    let m string = greeting(\"x\")\n",
            "    let labeled string = m + to_string(2)\n",
            "    measure(labeled) + len(to_string(true))\n",
        )))
        .expect("emit native program");
        for func in ["greeting", "measure", "main"] {
            assert!(
                program.compiled.contains(&func.to_string()),
                "expected `{func}` compiled: {:?} / skipped {:?}",
                program.compiled,
                program.skipped
            );
        }
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    }

    #[test]
    fn string_object_emits_string_runtime_helpers() {
        // A string-using program emits the string runtime helpers + the bump
        // allocator as external-defined `.text` symbols, proving the literal /
        // concat / to_string codegen is present.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let s string = \"a\" + to_string(1)\n",
            "    len(s)\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        for symbol in [
            STR_LIT_SYMBOL,
            STR_CONCAT_SYMBOL,
            STR_FROM_INT_SYMBOL,
            STR_FROM_BOOL_SYMBOL,
            STR_FROM_CHAR_SYMBOL,
            STR_SUBSTRING_SYMBOL,
            STR_FIND_SYMBOL,
            STR_CONTAINS_SYMBOL,
            STR_STARTS_WITH_SYMBOL,
            STR_ENDS_WITH_SYMBOL,
            HEAP_ALLOC_SYMBOL,
        ] {
            let (section, _storage) =
                coff_symbol(&program.bytes, symbol).unwrap_or_else(|| panic!("missing {symbol}"));
            assert_eq!(section, 1, "{symbol} must be defined in .text");
        }
        // The `.bss` heap region must be present for a string-using object.
        assert!(
            coff_symbol(&program.bytes, HEAP_BASE_SYMBOL).is_some(),
            "the .bss heap region must be present for a string-using object"
        );
    }

    #[test]
    fn concat_call_site_calls_the_concat_helper() {
        // A `s + t` concatenation lowers to a `call __lullaby_str_concat`, so a
        // concatenating function carries a relocation against the concat helper.
        // (The helper function is named `cat`, not `join`, to avoid the `join`
        // builtin, whose registered signature would type the arguments as
        // `array<string>`.)
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn cat a string b string -> string\n",
            "    a + b\n\n",
            "fn main -> i64\n",
            "    len(cat(\"x\", \"y\"))\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"cat".to_string()),
            "cat must compile: {:?}",
            program.skipped
        );
        // The concat helper is emitted as a symbol (references it via a relocation).
        assert!(
            coff_symbol(&program.bytes, STR_CONCAT_SYMBOL).is_some(),
            "the concat helper symbol must be present"
        );
    }

    #[test]
    fn index_string_op_call_sites_call_their_helpers() {
        // Each index-based string op lowers to a `call` of its `.text` helper, so a
        // function using them all references every helper symbol via a relocation
        // and compiles natively (never skips to the interpreters). The bool results
        // are folded to i64 through a tiny helper so `main` stays i64-returning.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn b2i x bool -> i64\n",
            "    if x\n",
            "        return 1\n",
            "    return 0\n\n",
            "fn main -> i64\n",
            "    let s string = \"café\"\n",
            "    let head string = substring(s, 0, 2)\n",
            "    let idx i64 = find(s, \"f\")\n",
            "    let c i64 = b2i(contains(s, \"af\"))\n",
            "    let sw i64 = b2i(starts_with(s, \"ca\"))\n",
            "    let ew i64 = b2i(ends_with(s, \"é\"))\n",
            "    len(head) + idx + c + sw + ew\n",
        )))
        .expect("emit native program");
        assert!(
            program.compiled.contains(&"main".to_string()),
            "main must compile natively: skipped {:?}",
            program.skipped
        );
        assert!(program.skipped.is_empty(), "{:?}", program.skipped);
        for symbol in [
            STR_SUBSTRING_SYMBOL,
            STR_FIND_SYMBOL,
            STR_CONTAINS_SYMBOL,
            STR_STARTS_WITH_SYMBOL,
            STR_ENDS_WITH_SYMBOL,
        ] {
            let (section, _storage) = coff_symbol(&program.bytes, symbol)
                .unwrap_or_else(|| panic!("missing helper symbol {symbol}"));
            assert_eq!(section, 1, "{symbol} must be defined in .text");
        }
    }

    #[test]
    fn substring_helper_emits_the_l0413_trap() {
        // The `substring` helper bounds-checks the char range and traps with `ud2`
        // (0F 0B) on a violation, mirroring the interpreters' `L0413` — it must not
        // silently produce a wrong slice. Assert the helper body carries a `ud2`.
        let helper = emit_str_substring_helper();
        assert!(
            helper.code.windows(2).any(|w| w == [0x0F, 0x0B]),
            "the substring helper must carry a `ud2` trap for out-of-bounds ranges"
        );
        // It allocates a fresh record, so it references the bump allocator.
        assert!(
            helper
                .relocations
                .iter()
                .any(|r| r.symbol == HEAP_ALLOC_SYMBOL),
            "the substring helper must call the bump allocator"
        );
    }

    #[test]
    fn index_string_scan_helpers_are_leaf_functions() {
        // `find`/`contains`/`starts_with`/`ends_with` are pure scans: no allocation,
        // so they carry no relocations, and each returns via a single `ret` (0xC3).
        for helper in [
            emit_str_find_helper(),
            emit_str_contains_helper(),
            emit_str_starts_with_helper(),
            emit_str_ends_with_helper(),
        ] {
            assert!(
                helper.relocations.is_empty(),
                "{} must be a leaf (no calls/relocations)",
                helper.name
            );
            assert_eq!(
                helper.code.last(),
                Some(&0xC3),
                "{} must end in `ret`",
                helper.name
            );
        }
    }

    #[test]
    fn to_string_of_float_skips_gracefully() {
        // `to_string(f64)` needs dtoa, which is deferred: the function skips (runs
        // on the interpreters) rather than miscompiling. With no other eligible
        // function, the emitter returns the `L0339` no-eligible error naming the
        // skip.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    len(to_string(1.5))\n",
        )));
        let err = program.expect_err("float to_string must not compile");
        assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
        assert!(
            err.skipped
                .iter()
                .any(|s| s.name == "main" && s.reason.contains("to_string")),
            "the skip reason must cite the deferred float to_string: {:?}",
            err.skipped
        );
    }

    #[test]
    fn string_is_not_a_by_pointer_aggregate() {
        // A `string` is a single immutable pointer word, so it is classified as a
        // scalar (register value), not a by-pointer aggregate — unlike a struct or
        // enum. This keeps a string parameter/return in an integer register with no
        // deep copy.
        assert!(!NativeType::String.is_aggregate());
        assert_eq!(NativeType::String.words(), 1);
        let native =
            resolve_native_type(&TypeRef::new("string"), &[], &[]).expect("resolve string");
        assert_eq!(native, NativeType::String);
        assert!(
            !native_signature_type_is_aggregate(&TypeRef::new("string"), &[], &[])
                .expect("string classifies"),
            "a string signature slot is a scalar (register), not an aggregate"
        );
    }

    // -- Cross-format object emission (ELF / Mach-O) -------------------------
    //
    // These exercise the object-format abstraction end-to-end: the same lowered
    // x86-64 program is re-serialized into an ELF64 or Mach-O container. The bytes
    // are checked structurally (link+run is deferred to Phase 9 CI on the native
    // platform), and the default Windows COFF path is confirmed unchanged.

    const ADD_AND_MAIN: &str =
        "fn add a i64 b i64 -> i64\n    a + b\n\nfn main -> i64\n    return add(20, 22)\n";

    #[test]
    fn elf_target_emits_relocatable_elf64() {
        let program = emit_alpha1_native_program_for_target(
            &module_for(ADD_AND_MAIN),
            &crate::native_contract::x86_64_linux_target(),
            None,
        )
        .expect("emit ELF program");

        assert_eq!(program.target.triple, "x86_64-unknown-linux-gnu");
        assert_eq!(program.entry_symbol, "_start");
        assert_eq!(&program.bytes[0..4], &[0x7f, b'E', b'L', b'F'], "ELF magic");
        assert_eq!(program.bytes[4], 2, "ELFCLASS64");
        // e_type = ET_REL, e_machine = EM_X86_64.
        assert_eq!(read_u16(&program.bytes, 16), 1);
        assert_eq!(read_u16(&program.bytes, 18), 62);
        assert_eq!(
            program.compiled,
            vec!["add".to_string(), "main".to_string()]
        );
    }

    #[test]
    fn macho_target_emits_relocatable_macho64() {
        let program = emit_alpha1_native_program_for_target(
            &module_for(ADD_AND_MAIN),
            &crate::native_contract::x86_64_macos_target(),
            None,
        )
        .expect("emit Mach-O program");

        assert_eq!(program.target.triple, "x86_64-apple-darwin");
        assert_eq!(program.entry_symbol, "start");
        // MH_MAGIC_64 little-endian.
        assert_eq!(read_u32(&program.bytes, 0), 0xFEED_FACF);
        // filetype = MH_OBJECT.
        assert_eq!(read_u32(&program.bytes, 12), 1);
    }

    #[test]
    fn elf_entry_stub_exits_via_the_linux_syscall() {
        // The freestanding `_start` stub must end in `mov eax, 60` (SYS_exit) then
        // `syscall`, and must NOT reference `ExitProcess` (a Windows-only import).
        let program = emit_alpha1_native_program_for_target(
            &module_for(ADD_AND_MAIN),
            &crate::native_contract::x86_64_linux_target(),
            None,
        )
        .expect("emit ELF program");
        // Locate `.text` and confirm the exit-syscall byte sequence appears.
        let needle = [0xB8, 0x3C, 0x00, 0x00, 0x00, 0x0F, 0x05]; // mov eax,60; syscall
        assert!(
            program.bytes.windows(needle.len()).any(|w| w == needle),
            "ELF entry stub issues the Linux exit syscall"
        );
    }

    #[test]
    fn default_target_is_unchanged_windows_coff() {
        // The default emit path and the explicit Windows target produce identical
        // COFF bytes, proving the abstraction did not disturb the default.
        let module = module_for(ADD_AND_MAIN);
        let default = emit_alpha1_native_program(&module).expect("default");
        let windows = emit_alpha1_native_program_for_target(
            &module,
            &crate::native_contract::x86_64_windows_target(),
            None,
        )
        .expect("windows");
        assert_eq!(
            default.bytes, windows.bytes,
            "COFF bytes byte-for-byte equal"
        );
        assert_eq!(default.target.triple, "x86_64-pc-windows-msvc");
        assert_eq!(read_u16(&default.bytes, 0), AMD64_MACHINE);
    }

    #[test]
    fn elf_string_program_carries_rodata_and_bss() {
        // A program that interns a string constant must produce the data sections
        // in the ELF object too (.rodata for the constant, .bss for the heap).
        let program = emit_alpha1_native_program_for_target(
            &module_for(
                "fn main -> i64\n    let a i64 = len(\"hello\")\n    let b i64 = len(\"native\")\n    return a + b\n",
            ),
            &crate::native_contract::x86_64_linux_target(),
            None,
        )
        .expect("emit ELF program with strings");
        assert_eq!(&program.bytes[0..4], &[0x7f, b'E', b'L', b'F']);
        // More than the text-only section count (null,.text,.symtab,.strtab,.shstrtab
        // = 5); the data path adds .rodata, .bss, and .rela.text.
        assert!(read_u16(&program.bytes, 60) > 5, "data sections present");
    }
}
