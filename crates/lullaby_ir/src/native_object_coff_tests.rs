use lullaby_diagnostics::Span;
use lullaby_parser::{BinaryOp, TypeRef};

use super::*;

#[test]
fn emits_minimal_coff_object_for_i64_literal_main() {
    let module = literal_return_module("i64", BytecodeExprKind::Integer(42));
    let object = emit_coff_object(&module).expect("emit object");

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
    let error = emit_coff_object(&module).expect_err("reject variable return");

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

    let object = emit_coff_object(&module).expect("emit object");
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

    let object = emit_coff_object(&module).expect("emit object");
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
