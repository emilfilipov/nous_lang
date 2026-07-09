use lullaby_parser::TypeRef;
use serde::{Deserialize, Serialize};

pub const ALPHA1_NATIVE_CONTRACT_NAME: &str = "alpha1-native-backend-contract";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeBackendContract {
    pub name: String,
    pub first_target: NativeTarget,
    pub supported_targets: Vec<NativeTarget>,
    pub calling_convention: NativeCallingConvention,
    pub stack_frame: NativeStackFrameContract,
    pub pointer_layout: NativePointerLayout,
    pub array_layout: NativeArrayLayout,
    pub resource_cleanup: NativeResourceCleanupContract,
    pub diagnostics: NativeDiagnosticsContract,
    pub object_emission: NativeObjectEmissionPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeTarget {
    pub triple: String,
    pub architecture: NativeArchitecture,
    pub object_format: NativeObjectFormat,
    pub pointer_width_bits: u16,
    pub endian: NativeEndian,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeArchitecture {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeObjectFormat {
    Coff,
    Elf,
    MachO,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeEndian {
    Little,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCallingConvention {
    pub name: String,
    pub entry_function: String,
    pub parameter_order: NativeParameterOrder,
    pub return_strategy: NativeReturnStrategy,
    pub call_boundary_alignment_bytes: u16,
    pub variadic_calls_allowed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeParameterOrder {
    SourceOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeReturnStrategy {
    DirectForScalarAndHandleValues,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeStackFrameContract {
    pub call_boundary_alignment_bytes: u16,
    pub slot_classes: Vec<NativeStackSlotClass>,
    pub cleanup_order: NativeCleanupOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeStackSlotClass {
    Parameter,
    Local,
    Temporary,
    Spill,
    CleanupRecord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeCleanupOrder {
    MemoryOperationSequence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativePointerLayout {
    pub pointer_width_bits: u16,
    pub null_is_valid_safe_value: bool,
    pub type_tag_source: String,
    pub safety_requirements: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeArrayLayout {
    pub representation: NativeArrayRepresentation,
    pub descriptor_fields: Vec<String>,
    pub element_layout_source: String,
    pub safety_requirements: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeArrayRepresentation {
    RuntimeDescriptorHandle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeResourceCleanupContract {
    pub sequence_source: String,
    pub explicit_release_role: String,
    pub compiler_cleanup_role: String,
    pub requirements: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeDiagnosticsContract {
    pub phase: String,
    pub code_family: String,
    pub requirements: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeObjectEmissionPlan {
    pub first_target_triple: String,
    pub first_object_format: NativeObjectFormat,
    pub linker_workflow_in_scope: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeValueLayout {
    pub type_pattern: String,
    pub class: NativeValueClass,
    pub size_bytes: u16,
    pub align_bytes: u16,
    pub pass_mode: NativePassMode,
    pub return_mode: NativeReturnMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeValueClass {
    Void,
    Integer,
    Boolean,
    RuntimeHandle,
    HeapPointer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativePassMode {
    NoPayload,
    DirectValue,
    DirectPointerSizedHandle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeReturnMode {
    NoPayload,
    DirectValue,
    DirectPointerSizedHandle,
}

pub fn alpha1_native_backend_contract() -> NativeBackendContract {
    let first_target = x86_64_windows_target();

    NativeBackendContract {
        name: ALPHA1_NATIVE_CONTRACT_NAME.to_string(),
        first_target: first_target.clone(),
        supported_targets: vec![
            first_target,
            x86_64_linux_target(),
            x86_64_macos_target(),
            aarch64_macos_target(),
        ],
        calling_convention: NativeCallingConvention {
            name: "lullaby-alpha1-internal-abi".to_string(),
            entry_function: "main".to_string(),
            parameter_order: NativeParameterOrder::SourceOrder,
            return_strategy: NativeReturnStrategy::DirectForScalarAndHandleValues,
            call_boundary_alignment_bytes: 16,
            variadic_calls_allowed: false,
        },
        stack_frame: NativeStackFrameContract {
            call_boundary_alignment_bytes: 16,
            slot_classes: vec![
                NativeStackSlotClass::Parameter,
                NativeStackSlotClass::Local,
                NativeStackSlotClass::Temporary,
                NativeStackSlotClass::Spill,
                NativeStackSlotClass::CleanupRecord,
            ],
            cleanup_order: NativeCleanupOrder::MemoryOperationSequence,
        },
        pointer_layout: NativePointerLayout {
            pointer_width_bits: 64,
            null_is_valid_safe_value: false,
            type_tag_source: "TypeRef plus semantic expression metadata".to_string(),
            safety_requirements: vec![
                "load/store/dealloc require live-resource memory metadata".to_string(),
                "safe source operations must not lower a null pointer value".to_string(),
            ],
        },
        array_layout: NativeArrayLayout {
            representation: NativeArrayRepresentation::RuntimeDescriptorHandle,
            descriptor_fields: vec![
                "length: i64".to_string(),
                "data: pointer-sized contiguous element storage handle".to_string(),
            ],
            element_layout_source: "alpha1_value_layout(element TypeRef)".to_string(),
            safety_requirements: vec![
                "index expressions are checked against descriptor length before element access"
                    .to_string(),
                "descriptor handles are pointer-sized values owned by runtime cleanup metadata"
                    .to_string(),
            ],
        },
        resource_cleanup: NativeResourceCleanupContract {
            sequence_source: "IrMemoryOperation.sequence".to_string(),
            explicit_release_role: "ExplicitRelease".to_string(),
            compiler_cleanup_role: "CompilerCleanup".to_string(),
            requirements: vec![
                "native lowering must preserve deterministic memory-operation sequence order"
                    .to_string(),
                "cleanup lowering must share the same live-resource metadata as bytecode artifacts"
                    .to_string(),
            ],
        },
        diagnostics: NativeDiagnosticsContract {
            phase: "native".to_string(),
            code_family: "L####".to_string(),
            requirements: vec![
                "backend failures must report shared diagnostic codes".to_string(),
                "target-specific failures must include the target triple".to_string(),
            ],
        },
        object_emission: NativeObjectEmissionPlan {
            first_target_triple: "x86_64-pc-windows-msvc".to_string(),
            first_object_format: NativeObjectFormat::Coff,
            linker_workflow_in_scope: false,
            notes: vec![
                "prototype object emission before linker orchestration".to_string(),
                "do not bypass AST, IR, or bytecode validation paths".to_string(),
            ],
        },
    }
}

pub fn alpha1_value_layout(ty: &TypeRef) -> Option<NativeValueLayout> {
    match ty.name.as_str() {
        "void" => Some(NativeValueLayout {
            type_pattern: "void".to_string(),
            class: NativeValueClass::Void,
            size_bytes: 0,
            align_bytes: 1,
            pass_mode: NativePassMode::NoPayload,
            return_mode: NativeReturnMode::NoPayload,
        }),
        "i64" => Some(NativeValueLayout {
            type_pattern: "i64".to_string(),
            class: NativeValueClass::Integer,
            size_bytes: 8,
            align_bytes: 8,
            pass_mode: NativePassMode::DirectValue,
            return_mode: NativeReturnMode::DirectValue,
        }),
        "bool" => Some(NativeValueLayout {
            type_pattern: "bool".to_string(),
            class: NativeValueClass::Boolean,
            size_bytes: 1,
            align_bytes: 1,
            pass_mode: NativePassMode::DirectValue,
            return_mode: NativeReturnMode::DirectValue,
        }),
        "string" => Some(pointer_sized_handle_layout(
            "string",
            NativeValueClass::RuntimeHandle,
        )),
        name if ty.array_element().is_some() => Some(pointer_sized_handle_layout(
            name,
            NativeValueClass::RuntimeHandle,
        )),
        name if name.starts_with("ptr_") => Some(pointer_sized_handle_layout(
            name,
            NativeValueClass::HeapPointer,
        )),
        _ => None,
    }
}

fn pointer_sized_handle_layout(
    type_pattern: impl Into<String>,
    class: NativeValueClass,
) -> NativeValueLayout {
    NativeValueLayout {
        type_pattern: type_pattern.into(),
        class,
        size_bytes: 8,
        align_bytes: 8,
        pass_mode: NativePassMode::DirectPointerSizedHandle,
        return_mode: NativeReturnMode::DirectPointerSizedHandle,
    }
}

/// The default native target: `x86_64-pc-windows-msvc` (COFF). The native
/// backend emits this format by default, so existing behavior and the
/// byte-for-byte COFF snapshots are unchanged.
pub fn x86_64_windows_target() -> NativeTarget {
    NativeTarget {
        triple: "x86_64-pc-windows-msvc".to_string(),
        architecture: NativeArchitecture::X86_64,
        object_format: NativeObjectFormat::Coff,
        pointer_width_bits: 64,
        endian: NativeEndian::Little,
    }
}

/// The `x86_64-unknown-linux-gnu` target (ELF64, System V AMD64). Selected with
/// `lullaby native --target x86_64-unknown-linux-gnu`.
pub fn x86_64_linux_target() -> NativeTarget {
    NativeTarget {
        triple: "x86_64-unknown-linux-gnu".to_string(),
        architecture: NativeArchitecture::X86_64,
        object_format: NativeObjectFormat::Elf,
        pointer_width_bits: 64,
        endian: NativeEndian::Little,
    }
}

/// The `x86_64-apple-darwin` target (Mach-O x86-64). Selected with
/// `lullaby native --target x86_64-apple-darwin`.
pub fn x86_64_macos_target() -> NativeTarget {
    NativeTarget {
        triple: "x86_64-apple-darwin".to_string(),
        architecture: NativeArchitecture::X86_64,
        object_format: NativeObjectFormat::MachO,
        pointer_width_bits: 64,
        endian: NativeEndian::Little,
    }
}

fn aarch64_macos_target() -> NativeTarget {
    NativeTarget {
        triple: "aarch64-apple-darwin".to_string(),
        architecture: NativeArchitecture::Aarch64,
        object_format: NativeObjectFormat::MachO,
        pointer_width_bits: 64,
        endian: NativeEndian::Little,
    }
}

/// The `aarch64-unknown-linux-gnu` target (ELF64, AAPCS64). Selected with
/// `lullaby native --target aarch64-unknown-linux-gnu`; lowered by the dedicated
/// AArch64 code generator (`crate::aarch64`) to a freestanding aarch64 ELF
/// object. This is the second implemented instruction-set backend.
pub fn aarch64_linux_target() -> NativeTarget {
    NativeTarget {
        triple: "aarch64-unknown-linux-gnu".to_string(),
        architecture: NativeArchitecture::Aarch64,
        object_format: NativeObjectFormat::Elf,
        pointer_width_bits: 64,
        endian: NativeEndian::Little,
    }
}

/// Resolve a `--target` triple to the native target the backend can emit today:
/// the three x86-64 triples — `x86_64-pc-windows-msvc` (COFF),
/// `x86_64-unknown-linux-gnu` (ELF), `x86_64-apple-darwin` (Mach-O) — plus
/// `aarch64-unknown-linux-gnu` (aarch64 ELF), the second instruction-set
/// backend. The `aarch64-apple-darwin` triple is a declared future target with
/// no code generator yet, so it is still rejected here.
pub fn native_target_for_triple(triple: &str) -> Option<NativeTarget> {
    match triple {
        "x86_64-pc-windows-msvc" => Some(x86_64_windows_target()),
        "x86_64-unknown-linux-gnu" => Some(x86_64_linux_target()),
        "x86_64-apple-darwin" => Some(x86_64_macos_target()),
        "aarch64-unknown-linux-gnu" => Some(aarch64_linux_target()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha1_contract_names_first_target_and_supported_targets() {
        let contract = alpha1_native_backend_contract();

        assert_eq!(contract.name, ALPHA1_NATIVE_CONTRACT_NAME);
        assert_eq!(contract.first_target.triple, "x86_64-pc-windows-msvc");
        assert_eq!(
            contract.object_emission.first_target_triple,
            contract.first_target.triple
        );
        assert!(contract.supported_targets.iter().any(|target| {
            target.triple == "aarch64-apple-darwin"
                && target.architecture == NativeArchitecture::Aarch64
        }));
    }

    #[test]
    fn alpha1_value_layout_covers_current_type_surface() {
        let cases = [
            ("void", NativeValueClass::Void, 0, NativePassMode::NoPayload),
            (
                "i64",
                NativeValueClass::Integer,
                8,
                NativePassMode::DirectValue,
            ),
            (
                "bool",
                NativeValueClass::Boolean,
                1,
                NativePassMode::DirectValue,
            ),
            (
                "string",
                NativeValueClass::RuntimeHandle,
                8,
                NativePassMode::DirectPointerSizedHandle,
            ),
            (
                "array<i64>",
                NativeValueClass::RuntimeHandle,
                8,
                NativePassMode::DirectPointerSizedHandle,
            ),
            (
                "ptr_i64",
                NativeValueClass::HeapPointer,
                8,
                NativePassMode::DirectPointerSizedHandle,
            ),
        ];

        for (name, class, size_bytes, pass_mode) in cases {
            let layout = alpha1_value_layout(&TypeRef::new(name)).expect("layout exists");
            assert_eq!(layout.class, class, "{name}");
            assert_eq!(layout.size_bytes, size_bytes, "{name}");
            assert_eq!(layout.pass_mode, pass_mode, "{name}");
        }
    }

    #[test]
    fn cleanup_contract_uses_memory_operation_sequence() {
        let contract = alpha1_native_backend_contract();

        assert_eq!(
            contract.stack_frame.cleanup_order,
            NativeCleanupOrder::MemoryOperationSequence
        );
        assert_eq!(
            contract.resource_cleanup.sequence_source,
            "IrMemoryOperation.sequence"
        );
    }

    #[test]
    fn native_contract_round_trips_through_json() {
        let contract = alpha1_native_backend_contract();
        let encoded = serde_json::to_string_pretty(&contract).expect("encode contract");
        let decoded: NativeBackendContract =
            serde_json::from_str(&encoded).expect("decode contract");

        assert_eq!(decoded, contract);
    }
}
