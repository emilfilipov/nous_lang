use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use lullaby_parser::{AssignOp, BinaryOp, TypeRef};

use crate::native_contract::{NativeObjectFormat, NativeTarget, alpha1_native_backend_contract};
use crate::{
    BytecodeExpr, BytecodeExprKind, BytecodeFunction, BytecodeIfBranch, BytecodeInstruction,
    BytecodeMatchArm, BytecodeMatchPattern, BytecodeModule, BytecodePlace, IntKind, IrEnumDef,
    IrStructDef,
};

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

/// Size in bytes of the fixed reserved native heap region.
const HEAP_REGION_SIZE: u32 = 64 * 1024;

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
    let contract = alpha1_native_backend_contract();
    let target = contract.first_target;

    // First pass: decide signature eligibility. Calls resolve against the set of
    // names we intend to compile.
    let mut skipped: Vec<NativeSkippedFunction> = Vec::new();
    let mut eligible_names: Vec<String> = Vec::new();
    for function in &module.functions {
        match native_signature_eligibility(function) {
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

        for name in &eligible_names {
            let function = module
                .functions
                .iter()
                .find(|f| &f.name == name)
                .expect("eligible name exists");
            match lower_native_function(
                function,
                &callable,
                &extern_sigs,
                &module.structs,
                &module.enums,
                &mut strings,
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
        // (no `main`) omits the stub entirely, so it carries no `ExitProcess`
        // dependency and does not collide with a C `main` at link time.
        let bytes = write_native_program_object(&lowered, &strings, has_main, debug);
        // When the program declares any `extern fn`, the C runtime import library
        // must be linked so the external C symbols resolve.
        let import_libs = if module.extern_functions.is_empty() {
            Vec::new()
        } else {
            vec![C_RUNTIME_IMPORT_LIB.to_string()]
        };
        // A library object (no stub) has no `/entry:` symbol; the C runtime
        // provides the entry point when it is linked with a C `main`.
        let entry_symbol = if has_main {
            NATIVE_ENTRY_SYMBOL.to_string()
        } else {
            String::new()
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

/// Whether a function's signature is entirely `i64` with at most four params
/// (Win64 register arguments; stack args are deferred).
fn native_signature_eligibility(function: &BytecodeFunction) -> Result<(), String> {
    if function.params.len() > 4 {
        return Err(format!(
            "native subset supports at most four i64 parameters; `{}` has {}",
            function.name,
            function.params.len()
        ));
    }
    for param in &function.params {
        if param.ty.name != "i64" {
            return Err(format!(
                "parameter `{}` has non-i64 type `{}`",
                param.name, param.ty.name
            ));
        }
    }
    if function.return_type.name != "i64" {
        return Err(format!(
            "return type `{}` is not i64",
            function.return_type.name
        ));
    }
    Ok(())
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
    /// A named struct whose fields are all supported native types, in order.
    Struct {
        name: String,
        fields: Vec<(String, NativeType)>,
    },
    /// A fixed-length array of a supported element type.
    Array { elem: Box<NativeType>, len: usize },
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
    /// The number of 8-byte words this value occupies on the stack.
    fn words(&self) -> usize {
        match self {
            NativeType::I64 | NativeType::F64 | NativeType::F32 => 1,
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
        name if name.starts_with("array<") => Err(format!(
            "array length for `{name}` is unknown from its type"
        )),
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
            let native = resolve_native_type(payload_ty, structs, enums).map_err(|_| {
                format!(
                    "enum `{ctor}` variant `{name}` payload type `{}` is not a native scalar \
                     (heap payloads and nested aggregates are deferred)",
                    payload_ty.name
                )
            })?;
            // Only scalar payloads are supported: an `i64`/fixed-width/bool/char/
            // byte cell (`NativeType::I64`) or a float (`F64`/`F32`). A nested
            // struct/array/enum payload is out of scope and skips gracefully.
            match native {
                NativeType::I64 | NativeType::F64 | NativeType::F32 => payload.push(native),
                _ => {
                    return Err(format!(
                        "enum `{ctor}` variant `{name}` has a non-scalar payload; \
                         only scalar enum payloads are in the native subset"
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
) -> Result<NativeType, String> {
    if let BytecodeExprKind::Array(elements) = &expr.kind {
        let first = elements
            .first()
            .ok_or("empty array literals are not in the native stack subset")?;
        let elem = native_type_of_init(first, structs, enums)?;
        for other in &elements[1..] {
            let other_ty = native_type_of_init(other, structs, enums)?;
            if other_ty != elem {
                return Err("array literal elements have differing native layouts".to_string());
            }
        }
        return Ok(NativeType::Array {
            elem: Box::new(elem),
            len: elements.len(),
        });
    }
    resolve_native_type(&expr.ty, structs, enums)
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
}

impl<'a> NativeCtx<'a> {
    /// Plan the stack frame: assign contiguous 8-byte-word slots to every
    /// parameter and `let`/`for` local (aggregates reserve one word per
    /// flattened scalar), plus 32 bytes of Win64 shadow space when the function
    /// makes calls. All slots are `[rbp - displacement]`.
    fn plan(
        function: &'a BytecodeFunction,
        callable: &'a std::collections::HashSet<&'a str>,
        extern_sigs: &'a HashMap<&'a str, &'a crate::IrExternSignature>,
        structs: &'a [IrStructDef],
        enums: &'a [IrEnumDef],
        strings: &'a mut StringPool,
    ) -> Result<Self, String> {
        let mut locals: HashMap<String, NativeLocal> = HashMap::new();
        let mut next_slot: i32 = 0;

        // Parameters first (they will be spilled from registers in the prologue).
        // Aggregate parameters are deferred, so every param is a single i64 word.
        for param in &function.params {
            next_slot += 8;
            locals.insert(
                param.name.clone(),
                NativeLocal {
                    slot: next_slot,
                    ty: NativeType::I64,
                },
            );
        }

        // Then `let` and `for` induction locals discovered anywhere in the body.
        collect_native_locals(
            &function.instructions,
            structs,
            enums,
            &mut locals,
            &mut next_slot,
        )?;

        // Reserve scratch words for `match` scrutinees that are not plain locals
        // (a call result or freshly-constructed enum is spilled to scratch before
        // the tag dispatch). One shared region sized to the widest such enum
        // scrutinee across the function suffices, since a match fully consumes its
        // scratch before the next one runs. The scratch base is the first word
        // past the planned locals.
        let scratch_words = max_match_scratch_words(&function.instructions, structs, enums)?;
        let scratch_base = next_slot;
        next_slot += scratch_words as i32 * 8;

        let has_call = body_has_call(&function.instructions);
        // Reserve local slots plus (if calling) 32 bytes of shadow space.
        let shadow = if has_call { 32 } else { 0 };
        let raw = next_slot + shadow;
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
                        native_type_of_init(value, structs, enums)?
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
                collect_native_locals(body, structs, enums, locals, next_slot)?;
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    collect_native_locals(&branch.body, structs, enums, locals, next_slot)?;
                }
                collect_native_locals(else_body, structs, enums, locals, next_slot)?;
            }
            BytecodeInstruction::While { body, .. } | BytecodeInstruction::Loop { body, .. } => {
                collect_native_locals(body, structs, enums, locals, next_slot)?;
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
                                let words = payload_ty.words() as i32;
                                *next_slot += words * 8;
                                locals.insert(
                                    binding.clone(),
                                    NativeLocal {
                                        slot: *next_slot - (words - 1) * 8,
                                        ty: payload_ty.clone(),
                                    },
                                );
                            }
                        }
                    }
                    collect_native_locals(&arm.body, structs, enums, locals, next_slot)?;
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
        BytecodeExprKind::Binary { left, right, .. } => expr_has_call(left) || expr_has_call(right),
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

fn lower_native_function(
    function: &BytecodeFunction,
    callable: &std::collections::HashSet<&str>,
    extern_sigs: &HashMap<&str, &crate::IrExternSignature>,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    strings: &mut StringPool,
) -> Result<LoweredNativeFunction, String> {
    let mut ctx = NativeCtx::plan(function, callable, extern_sigs, structs, enums, strings)?;
    let mut code = Vec::new();

    // Prologue: push rbp; mov rbp, rsp; sub rsp, frame_size.
    code.extend_from_slice(&[0x55, 0x48, 0x89, 0xE5]);
    emit_sub_rsp(&mut code, ctx.frame_size);

    // Spill register parameters (rcx, rdx, r8, r9) into their slots.
    // mov [rbp - slot], reg
    const PARAM_STORE: [&[u8]; 4] = [
        &[0x48, 0x89, 0x8D], // mov [rbp+disp32], rcx
        &[0x48, 0x89, 0x95], // mov [rbp+disp32], rdx
        &[0x4C, 0x89, 0x85], // mov [rbp+disp32], r8
        &[0x4C, 0x89, 0x8D], // mov [rbp+disp32], r9
    ];
    for (index, param) in function.params.iter().enumerate() {
        let slot = ctx.local_slot(&param.name)?;
        code.extend_from_slice(PARAM_STORE[index]);
        code.extend_from_slice(&(-slot).to_le_bytes());
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
            lower_native_expr(&mut ctx, expr, &mut code)?;
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
                NativeType::I64 => {
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
            let place = resolve_scalar_place(ctx, name, path)?;
            match op {
                AssignOp::Replace => {
                    // Evaluate the RHS, then store into the resolved scalar slot.
                    match place {
                        ScalarPlace::Const { slot } => {
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
            lower_native_expr(ctx, expr, code)?;
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
/// `base_slot`. Three initializer shapes are supported, mirroring how the IR
/// lowerer represents construction:
///   * an array literal `[e0, e1, ...]` -> each element materialized in turn;
///   * a struct constructor `Call { name: StructName, args }` -> each field in
///     declared order (the IR already reorders named fields);
///   * an aggregate variable `x` -> a word-by-word copy of another local.
fn lower_aggregate_init(
    ctx: &mut NativeCtx,
    base_slot: i32,
    ty: &NativeType,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
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
            NativeType::I64 => {
                lower_native_expr(ctx, arg, code)?;
                store_local(code, word);
            }
            NativeType::F64 | NativeType::F32 => {
                let width = lower_native_float_expr(ctx, arg, code)?;
                store_float_local(code, word, width);
            }
            _ => return Err("enum payload must be a native scalar".to_string()),
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
        _ => {
            // Matching the result of a call that *returns* an enum (or any other
            // temporary enum expression) requires an aggregate return ABI that is
            // not yet implemented; such a function skips gracefully to the
            // interpreters rather than miscompiling.
            return Err(
                "match scrutinee must be an enum local or a freshly-constructed enum \
                 (enum-returning calls are deferred on the native backend)"
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
                        NativeType::I64 => {
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
                        _ => return Err("enum payload binding is not a native scalar".to_string()),
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

/// Lower an `if`/`elif`/`else` chain. Each branch: evaluate condition into rax,
/// `test rax,rax`; `jz next`; body; `jmp end`. The final else falls through.
fn lower_native_if(
    ctx: &mut NativeCtx,
    branches: &[BytecodeIfBranch],
    else_body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    let mut end_jumps: Vec<usize> = Vec::new();

    for branch in branches {
        lower_native_expr(ctx, &branch.condition, code)?;
        code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
        // jz next_branch (rel32, patched below).
        code.extend_from_slice(&[0x0F, 0x84]);
        let jz_site = code.len();
        code.extend_from_slice(&[0, 0, 0, 0]);

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
    lower_native_expr(ctx, condition, code)?;
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.extend_from_slice(&[0x0F, 0x84]); // jz end (patched)
    let exit_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

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
            if !ctx.callable.contains(name.as_str()) {
                return Err(format!(
                    "call to non-i64-scalar or unknown function `{name}`"
                ));
            }
            if args.len() > 4 {
                return Err(format!(
                    "native calls support at most four arguments; `{name}` got {}",
                    args.len()
                ));
            }
            // If the target is an `extern fn` (a C symbol), marshal it across the
            // Win64 C ABI: validate that every parameter and the return type is a
            // marshallable integer scalar (the integer-register subset of §5.1 in
            // ffi_design.md), then normalize a narrow C return in `rax`. Float
            // (`f32`/`f64`) externs need XMM argument routing and are deferred:
            // such a caller is demoted to the interpreters (which reject it with
            // `L0423`). A non-extern call keeps the internal i64 convention.
            let extern_return = if let Some(sig) = ctx.extern_sigs.get(name.as_str()) {
                for param_ty in &sig.params {
                    ffi_scalar_int_kind(&param_ty.name).ok_or_else(|| {
                        format!(
                            "extern `{name}` parameter type `{}` is not a native FFI \
                             integer scalar (floats/pointers/aggregates are deferred)",
                            param_ty.name
                        )
                    })?;
                }
                Some(ffi_scalar_int_kind(&sig.return_type.name).ok_or_else(|| {
                    format!(
                        "extern `{name}` return type `{}` is not a native FFI integer \
                         scalar (floats/pointers/aggregates are deferred)",
                        sig.return_type.name
                    )
                })?)
            } else {
                None
            };
            // Evaluate args left-to-right, pushing each result. Then pop into the
            // Win64 argument registers in order. An integer scalar already sits in
            // the low bits of `rax` normalized to its width (the interpreter's
            // cell model), which is exactly what Win64 passes in the low bits of
            // the argument register, so no per-argument marshalling is needed for
            // the integer subset.
            for arg in args {
                lower_native_expr(ctx, arg, code)?;
                code.push(0x50); // push rax
            }
            // Pop in reverse so the first argument lands in rcx.
            const ARG_POP: [&[u8]; 4] = [
                &[0x59],       // pop rcx
                &[0x5A],       // pop rdx
                &[0x41, 0x58], // pop r8
                &[0x41, 0x59], // pop r9
            ];
            for index in (0..args.len()).rev() {
                code.extend_from_slice(ARG_POP[index]);
            }
            // call rel32 -> relocation against the target symbol.
            code.push(0xE8);
            let site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            ctx.relocations.push(CodeRelocation {
                offset: site as u32,
                symbol: name.clone(),
            });
            // Normalize an extern's C return value into its canonical `i64` cell.
            // The Win64 ABI leaves the upper bits of a narrow integer return
            // undefined, so a returned `i8`/`i16`/`i32`/`u8`/`u16`/`u32` is
            // re-normalized (sign/zero extended) so downstream Lullaby code sees
            // the same cell the interpreters produce. `i64`/64-bit kinds are a
            // no-op.
            if let Some(Some(fixed)) = extern_return {
                emit_normalize_rax(code, fixed);
            }
            Ok(())
        }
        BytecodeExprKind::Field { .. } | BytecodeExprKind::Index { .. } => {
            // A struct-field or array-index read yielding an i64 scalar. Resolve
            // the access to a stack word and load it into rax.
            let place = resolve_read_place(ctx, expr)?;
            emit_load_place(ctx, &place, code)
        }
        BytecodeExprKind::Bool(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Array(_)
        | BytecodeExprKind::Await { .. }
        // Closures are not in the native scalar subset: a function that
        // constructs or calls one is skipped and runs on the interpreters.
        | BytecodeExprKind::Closure { .. } => {
            Err("expression is not in the native i64-scalar subset".to_string())
        }
    }
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
            Err(format!(
                "float call `{name}` is not in the native subset (float-returning functions and math builtins are deferred)"
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
    if strings.is_empty() {
        write_text_only_object(functions, emit_stub, debug)
    } else {
        write_object_with_data(functions, strings, emit_stub, debug)
    }
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
    for helper in [HEAP_ALLOC_SYMBOL, HEAP_STRLEN_SYMBOL] {
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

        // The compiled `fib` code must contain a `test rax, rax` (the `if`
        // condition test) and a `setl al` (the `<` comparison).
        let sec = COFF_HEADER_SIZE as usize;
        let text_offset = read_u32(&program.bytes, sec + 20) as usize;
        let text_size = read_u32(&program.bytes, sec + 16) as usize;
        let text = &program.bytes[text_offset..text_offset + text_size];
        assert!(
            text.windows(3).any(|w| w == [0x48, 0x85, 0xC0]),
            "expected a `test rax, rax`"
        );
        assert!(
            text.windows(3).any(|w| w == [0x0F, 0x9C, 0xC0]),
            "expected a `setl al` for `<`"
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
    fn skips_match_over_enum_scrutinee_gracefully() {
        // `match` over a *local* enum now compiles natively, but an enum passed as
        // a *parameter* is still deferred: aggregate/enum parameters are not in the
        // native register ABI, so `classify` (whose `o` is an `option<i64>` param)
        // is skipped at the signature stage and runs on the interpreters — never
        // miscompiled — while the i64-scalar `double`/`main` still compile.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn classify o option<i64> -> i64\n",
            "    match o\n",
            "        some(v) -> v\n",
            "        none -> 0\n\n",
            "fn double x i64 -> i64\n",
            "    x + x\n\n",
            "fn main -> i64\n",
            "    return double(21)\n",
        )))
        .expect("emit native program");
        // `double` and `main` are i64-scalar and compile; `classify` (option param)
        // is skipped for its non-i64 parameter.
        assert_eq!(
            program.compiled,
            vec!["double".to_string(), "main".to_string()]
        );
        assert!(
            program.skipped.iter().any(|s| s.name == "classify"),
            "match-over-enum function must be skipped: {:?}",
            program.skipped
        );
    }

    #[test]
    fn skips_non_i64_functions_but_compiles_the_rest() {
        // `greet` returns a string (skipped); `main` and `add` are i64 (compiled).
        let program = emit_alpha1_native_program(&module_for(
            "fn greet s string -> string\n    s\n\nfn add a i64 b i64 -> i64\n    a + b\n\nfn main -> i64\n    return add(1, 2)\n",
        ))
        .expect("emit native program");
        assert_eq!(
            program.compiled,
            vec!["add".to_string(), "main".to_string()]
        );
        assert_eq!(program.skipped.len(), 1);
        assert_eq!(program.skipped[0].name, "greet");
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
    fn skips_saturating_builtin_gracefully() {
        // `saturating_add` returns a plain integer but is deferred on the native
        // backend (it is not emitted inline); a `main` using it skips gracefully.
        let err = emit_alpha1_native_program(&module_for(concat!(
            "fn main -> i64\n",
            "    let s u8 = saturating_add(to_u8(200), to_u8(100))\n",
            "    to_i64(s)\n",
        )))
        .expect_err("saturating builtin is deferred");
        assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
        assert!(
            err.skipped
                .iter()
                .any(|s| s.name == "main" && s.reason.contains("saturating_add")),
            "skip reason should name the deferred builtin: {:?}",
            err.skipped
        );
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
    fn skips_float_extern_caller_gracefully() {
        // A float (`f64`/`f32`) extern needs XMM argument/return routing, which is
        // deferred. A caller of such an extern must skip gracefully (demoted to the
        // interpreters, which reject the extern call with `L0423`) rather than
        // miscompiling — leaving nothing eligible here.
        let err = emit_alpha1_native_program(&module_for(concat!(
            "extern fn cfloor x f64 -> f64\n\n",
            "fn main -> i64\n",
            "    let r f64 = cfloor(3.7)\n",
            "    let flag i64 = 0\n",
            "    if r > 3.0\n",
            "        flag = 1\n",
            "    flag\n",
        )))
        .expect_err("float extern is deferred on native");
        assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
        assert!(
            err.skipped
                .iter()
                .any(|s| s.name == "main" && s.reason.contains("cfloor")),
            "skip reason should name the deferred float extern: {:?}",
            err.skipped
        );
    }

    #[test]
    fn errors_when_no_i64_scalar_function_is_eligible() {
        // `main` itself returns a string, so nothing is eligible for native.
        let err = emit_alpha1_native_program(&module_for("fn main -> string\n    \"hi\"\n"))
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
    fn function_with_float_signature_still_skips_gracefully() {
        // The signature constraint is unchanged: a function with a float parameter
        // or float return type is still recorded in `skipped` and falls back to
        // the interpreters. `main` (all-i64) still compiles.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn scale x f64 -> f64\n",
            "    x * 2.0\n\n",
            "fn main -> i64\n",
            "    let n i64 = 7\n",
            "    n\n",
        )))
        .expect("emit native program");
        assert_eq!(program.compiled, vec!["main".to_string()]);
        assert_eq!(program.skipped.len(), 1, "{:?}", program.skipped);
        assert_eq!(program.skipped[0].name, "scale");
        assert!(
            program.skipped[0].reason.contains("f64"),
            "skip reason should name the float signature: {}",
            program.skipped[0].reason
        );
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
    fn defers_enum_returning_call_gracefully() {
        // A function that returns an enum and a caller that matches that call
        // result are both deferred (aggregate return ABI is not implemented), and
        // the deferral never crashes — the whole program falls to the interpreters
        // with a clear skip reason.
        let err = emit_alpha1_native_program(&module_for(concat!(
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
        .expect_err("no function is native-eligible");
        assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
        assert!(
            err.skipped.iter().any(|s| s.name == "use_lookup"
                && s.reason.contains("enum-returning calls are deferred")),
            "expected a clear deferral reason for the enum-returning match: {:?}",
            err.skipped
        );
    }

    #[test]
    fn defers_result_with_string_payload_gracefully() {
        // A `result<i64, string>` carries a heap payload in `err`; matching it is
        // out of the native scalar subset, so the function skips gracefully rather
        // than miscompiling the string.
        let program = emit_alpha1_native_program(&module_for(concat!(
            "fn classify n i64 -> i64\n",
            "    let r result<i64, string> = ok(n)\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> len(m)\n\n",
            "fn main -> i64\n",
            "    classify(3)\n",
        )));
        // Either the whole program is ineligible (error) or `classify` is skipped
        // while a trivial `main` might still compile; in this shape `main` calls
        // the skipped `classify`, so nothing is eligible.
        match program {
            Err(err) => {
                assert!(
                    err.skipped.iter().any(|s| s.name == "classify"),
                    "expected `classify` skipped for its string payload: {:?}",
                    err.skipped
                );
            }
            Ok(program) => {
                assert!(
                    program.skipped.iter().any(|s| s.name == "classify"),
                    "expected `classify` skipped for its string payload: {:?}",
                    program.skipped
                );
            }
        }
    }
}
