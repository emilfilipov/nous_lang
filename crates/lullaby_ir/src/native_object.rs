use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use lullaby_parser::{AssignOp, BinaryOp, TypeRef};

use crate::native_contract::{NativeObjectFormat, NativeTarget, alpha1_native_backend_contract};
use crate::{
    BytecodeExpr, BytecodeExprKind, BytecodeFunction, BytecodeIfBranch, BytecodeInstruction,
    BytecodeModule, BytecodePlace, IrStructDef,
};

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
}

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

/// Emit a linkable COFF object for the i64-scalar-subset functions of `module`.
///
/// Eligible functions (all params + return are `i64`, at most four params, and a
/// body built from the supported subset) are lowered to x86-64. An entry stub
/// calls `main` and forwards its result to `ExitProcess`. Ineligible functions
/// are recorded in `skipped`. If no function is eligible, returns an error with
/// code `L0339`.
pub fn emit_alpha1_native_program(
    module: &BytecodeModule,
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
        let callable: std::collections::HashSet<&str> =
            eligible_names.iter().map(String::as_str).collect();
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
            match lower_native_function(function, &callable, &module.structs, &mut strings) {
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

        if lowered.is_empty() || !lowered.iter().any(|f| f.name == "main") {
            // The entry stub requires `main`. If `main` is not eligible there is
            // nothing runnable to emit.
            let reason = if lowered.is_empty() {
                "no functions were eligible for the native i64-scalar subset".to_string()
            } else {
                "`main` is not eligible for the native i64-scalar subset".to_string()
            };
            return Err(NativeProgramError {
                code: NATIVE_NO_ELIGIBLE_CODE,
                message: reason,
                skipped,
            });
        }

        let compiled: Vec<String> = lowered.iter().map(|f| f.name.clone()).collect();
        let bytes = write_native_program_object(&lowered, &strings);
        return Ok(NativeProgram {
            target,
            bytes,
            entry_symbol: NATIVE_ENTRY_SYMBOL.to_string(),
            compiled,
            skipped,
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
    /// A named struct whose fields are all supported native types, in order.
    Struct {
        name: String,
        fields: Vec<(String, NativeType)>,
    },
    /// A fixed-length array of a supported element type.
    Array { elem: Box<NativeType>, len: usize },
}

impl NativeType {
    /// The number of 8-byte words this value occupies on the stack.
    fn words(&self) -> usize {
        match self {
            NativeType::I64 => 1,
            NativeType::Struct { fields, .. } => fields.iter().map(|(_, t)| t.words()).sum(),
            NativeType::Array { elem, len } => elem.words() * len,
        }
    }
}

/// Resolve a declared `TypeRef` into a `NativeType`. Arrays are not resolvable
/// from the type alone (their length is not encoded in `array<T>`); array
/// locals derive their layout from their initializer instead, so a bare
/// `array<...>` type reaching here is an error the caller turns into a skip.
fn resolve_native_type(ty: &TypeRef, structs: &[IrStructDef]) -> Result<NativeType, String> {
    match ty.name.as_str() {
        "i64" => Ok(NativeType::I64),
        name if name.starts_with("array<") => Err(format!(
            "array length for `{name}` is unknown from its type"
        )),
        name => {
            let def = structs
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| format!("type `{name}` is not in the native stack subset"))?;
            let mut fields = Vec::with_capacity(def.fields.len());
            for (field_name, field_ty) in &def.fields {
                let native = resolve_native_type(field_ty, structs).map_err(|_| {
                    format!("struct `{name}` field `{field_name}` is not an all-i64 native type")
                })?;
                fields.push((field_name.clone(), native));
            }
            Ok(NativeType::Struct {
                name: name.to_string(),
                fields,
            })
        }
    }
}

/// Infer the `NativeType` of an initializer expression, using its static type
/// plus (for array literals) the literal element count. This is how array
/// lengths enter the layout, since `array<T>` carries no length.
fn native_type_of_init(expr: &BytecodeExpr, structs: &[IrStructDef]) -> Result<NativeType, String> {
    if let BytecodeExprKind::Array(elements) = &expr.kind {
        let first = elements
            .first()
            .ok_or("empty array literals are not in the native stack subset")?;
        let elem = native_type_of_init(first, structs)?;
        for other in &elements[1..] {
            let other_ty = native_type_of_init(other, structs)?;
            if other_ty != elem {
                return Err("array literal elements have differing native layouts".to_string());
            }
        }
        return Ok(NativeType::Array {
            elem: Box::new(elem),
            len: elements.len(),
        });
    }
    resolve_native_type(&expr.ty, structs)
}

/// A function lowered to x86-64: its symbol name, machine-code bytes, and the
/// relocations (at byte offsets within the code) that reference other symbols.
struct LoweredNativeFunction {
    name: String,
    code: Vec<u8>,
    relocations: Vec<CodeRelocation>,
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
    /// Relocations accumulated while emitting this function.
    relocations: Vec<CodeRelocation>,
    /// Program-wide interned string constants (`.rdata`), shared across all
    /// functions being lowered.
    strings: &'a mut StringPool,
}

impl<'a> NativeCtx<'a> {
    /// Plan the stack frame: assign contiguous 8-byte-word slots to every
    /// parameter and `let`/`for` local (aggregates reserve one word per
    /// flattened scalar), plus 32 bytes of Win64 shadow space when the function
    /// makes calls. All slots are `[rbp - displacement]`.
    fn plan(
        function: &'a BytecodeFunction,
        callable: &'a std::collections::HashSet<&'a str>,
        structs: &'a [IrStructDef],
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
        collect_native_locals(&function.instructions, structs, &mut locals, &mut next_slot)?;

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
            relocations: Vec::new(),
            strings,
        })
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
                        native_type_of_init(value, structs)?
                    } else {
                        resolve_native_type(ty, structs)?
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
                collect_native_locals(body, structs, locals, next_slot)?;
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    collect_native_locals(&branch.body, structs, locals, next_slot)?;
                }
                collect_native_locals(else_body, structs, locals, next_slot)?;
            }
            BytecodeInstruction::While { body, .. } | BytecodeInstruction::Loop { body, .. } => {
                collect_native_locals(body, structs, locals, next_slot)?;
            }
            _ => {}
        }
    }
    Ok(())
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
        BytecodeInstruction::Throw { .. }
        | BytecodeInstruction::Try { .. }
        | BytecodeInstruction::Match { .. } => false,
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
    structs: &[IrStructDef],
    strings: &mut StringPool,
) -> Result<LoweredNativeFunction, String> {
    let mut ctx = NativeCtx::plan(function, callable, structs, strings)?;
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
    if tail_is_value_expr {
        let (head, tail) = instructions.split_at(instructions.len() - 1);
        lower_native_stmts(&mut ctx, head, &mut code, &mut loops)?;
        if let BytecodeInstruction::Expr(expr) = &tail[0] {
            lower_native_expr(&mut ctx, expr, &mut code)?;
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
            // A scalar `let` uses the register path; an aggregate `let`
            // materializes each flattened scalar word directly into its slots.
            if matches!(ctx.local(name)?.ty, NativeType::I64) {
                lower_native_expr(ctx, value, code)?;
                let slot = ctx.local_slot(name)?;
                store_local(code, slot); // mov [rbp - slot], rax
            } else {
                let base = ctx.local(name)?.slot;
                let ty = ctx.local(name)?.ty.clone();
                lower_aggregate_init(ctx, base, &ty, value, code)?;
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
        BytecodeInstruction::Throw { .. } | BytecodeInstruction::Try { .. } => {
            Err("throw/try is not in the native subset".to_string())
        }
        BytecodeInstruction::Match { .. } => Err("match is not in the native subset".to_string()),
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
        },
        BytecodeExprKind::Binary { left, op, right } => {
            lower_native_binary(ctx, left, *op, right, code)
        }
        BytecodeExprKind::Call { name, args } => {
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
            // Evaluate args left-to-right, pushing each result. Then pop into the
            // Win64 argument registers in order.
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
        | BytecodeExprKind::Await { .. } => {
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
            lower_native_expr(ctx, left, code)?;
            code.push(0x50); // push rax (left)
            lower_native_expr(ctx, right, code)?; // right in rax
            emit_i64_binop_from_stack(code, op)
        }
    }
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
            code.extend_from_slice(&[0x48, 0x99]); // cqo (sign-extend into rdx)
            code.extend_from_slice(&[0x49, 0xF7, 0xF8]); // idiv r8
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
    }
    Ok(())
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
) -> Vec<u8> {
    if strings.is_empty() {
        write_text_only_object(functions)
    } else {
        write_object_with_data(functions, strings)
    }
}

/// Assemble the whole `.text` blob (entry stub + functions) and the section
/// relocations, then write the COFF headers, section data, symbol table, and
/// string table.
fn write_text_only_object(functions: &[LoweredNativeFunction]) -> Vec<u8> {
    // Lay out `.text`: entry stub first, then each function. Record each
    // function's start offset so relocations resolve.
    let mut text: Vec<u8> = Vec::new();
    let mut relocations: Vec<TextRelocation> = Vec::new();

    // Entry stub: sub rsp, 40 (align + shadow); call main; mov ecx, eax;
    // call ExitProcess; (int3 padding). The `sub rsp,40` keeps rsp 16-aligned at
    // each `call` (return address makes 8; 40 = 0x28 restores alignment).
    let stub_start = text.len();
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
    let _ = stub_start;

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
    symbols.push(SymbolDef {
        name: NATIVE_ENTRY_SYMBOL.to_string(),
        section_number: 1,
        value: 0,
    });
    for function in functions {
        symbols.push(SymbolDef {
            name: function.name.clone(),
            section_number: 1,
            value: *func_offsets.get(&function.name).expect("function offset"),
        });
    }
    symbols.push(SymbolDef {
        name: EXIT_PROCESS_SYMBOL.to_string(),
        section_number: 0,
        value: 0,
    });

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

    // -- Compute layout offsets ---------------------------------------------
    let num_relocs = relocations.len() as u32;
    let raw_text_offset = COFF_HEADER_SIZE + SECTION_HEADER_SIZE;
    let reloc_table_offset = raw_text_offset + text.len() as u32;
    let symbol_table_offset = reloc_table_offset + num_relocs * COFF_RELOC_SIZE;
    let num_symbols = symbols.len() as u32;

    // -- Emit ----------------------------------------------------------------
    let mut bytes = Vec::new();

    // COFF header.
    push_u16(&mut bytes, AMD64_MACHINE);
    push_u16(&mut bytes, 1); // one section
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

    // Section raw data.
    bytes.extend_from_slice(&text);

    // Relocation records: VirtualAddress (u32), SymbolTableIndex (u32), Type (u16).
    for reloc in &relocations {
        push_u32(&mut bytes, reloc.offset);
        push_u32(&mut bytes, reloc.symbol_index);
        push_u16(&mut bytes, IMAGE_REL_AMD64_REL32);
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
fn write_object_with_data(functions: &[LoweredNativeFunction], strings: &StringPool) -> Vec<u8> {
    // -- Build .text: entry stub, user functions, heap helpers ---------------
    let mut text: Vec<u8> = Vec::new();
    let mut relocations: Vec<TextRelocation> = Vec::new();

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
    symbols.push(SymbolDef {
        name: NATIVE_ENTRY_SYMBOL.to_string(),
        section_number: 1,
        value: 0,
        is_function: true,
    });
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
    symbols.push(SymbolDef {
        name: EXIT_PROCESS_SYMBOL.to_string(),
        section_number: 0,
        value: 0,
        is_function: true,
    });
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

    let symbol_index_of = |name: &str| -> u32 {
        symbols
            .iter()
            .position(|s| s.name == name)
            .expect("symbol exists") as u32
    };
    for reloc in &mut relocations {
        reloc.symbol_index = symbol_index_of(&reloc.symbol_name);
    }

    // -- Section layout ------------------------------------------------------
    const NUM_SECTIONS: u32 = 3;
    let bss_size = 8 + HEAP_REGION_SIZE;
    let num_relocs = relocations.len() as u32;

    let headers_end = COFF_HEADER_SIZE + NUM_SECTIONS * SECTION_HEADER_SIZE;
    let text_raw = headers_end;
    let rdata_raw = text_raw + text.len() as u32;
    // `.bss` has no raw data. Relocations follow the raw section data.
    let reloc_table_offset = rdata_raw + rdata.len() as u32;
    let symbol_table_offset = reloc_table_offset + num_relocs * COFF_RELOC_SIZE;
    let num_symbols = symbols.len() as u32;

    // -- Emit ----------------------------------------------------------------
    let mut bytes = Vec::new();

    // COFF header.
    push_u16(&mut bytes, AMD64_MACHINE);
    push_u16(&mut bytes, NUM_SECTIONS as u16);
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

    // Section raw data: .text then .rdata (.bss has none).
    bytes.extend_from_slice(&text);
    bytes.extend_from_slice(&rdata);

    // Relocations (all belong to .text).
    for reloc in &relocations {
        push_u32(&mut bytes, reloc.offset);
        push_u32(&mut bytes, reloc.symbol_index);
        push_u16(&mut bytes, IMAGE_REL_AMD64_REL32);
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
        let line = resolve_native_type(&TypeRef::new("Line"), &structs).expect("resolve Line");
        assert_eq!(line.words(), 4, "Line flattens to four i64 words");

        let array = NativeType::Array {
            elem: Box::new(NativeType::I64),
            len: 5,
        };
        assert_eq!(array.words(), 5);
    }
}
