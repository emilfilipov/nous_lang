//! COFF object emission for the native backend: the single-function prototype
//! emitter (`emit_coff_object`/`snapshot_native_object`), the shared COFF object
//! model (`NativeObjectFile`/`NativeObjectSection`/`NativeObjectSymbol`/
//! `NativeObjectSnapshot`, `NativeObjectError`), and the low-level byte writers.
//! Split out of native_object.rs; sees the parent's items via `use super::*`.

use super::*;

pub(crate) const AMD64_MACHINE: u16 = 0x8664;
pub(crate) const COFF_HEADER_SIZE: u32 = 20;
pub(crate) const SECTION_HEADER_SIZE: u32 = 40;
const SYMBOL_RECORD_SIZE: u32 = 18;
pub(crate) const TEXT_CHARACTERISTICS: u32 = 0x6050_0020;

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
                | BytecodeInstruction::RegionBlock { .. }
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
                | BytecodeInstruction::RegionBlock { .. }
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

pub(crate) fn push_fixed_name(bytes: &mut Vec<u8>, name: &str, width: usize) {
    let mut buffer = vec![0; width];
    buffer[..name.len()].copy_from_slice(name.as_bytes());
    bytes.extend_from_slice(&buffer);
}

pub(crate) fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn push_u32(bytes: &mut Vec<u8>, value: u32) {
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
