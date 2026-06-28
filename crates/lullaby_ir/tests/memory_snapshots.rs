use std::fs;
use std::path::{Path, PathBuf};

use lullaby_ir::{IrMemoryOperation, analyze_bytecode_memory_operations, lower, lower_to_bytecode};
use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_semantics::validate_executable;

const UPDATE_ENV: &str = "LULLABY_UPDATE_IR_MEMORY_SNAPSHOTS";

struct SnapshotCase {
    source: &'static str,
    snapshot: &'static str,
}

const CASES: &[SnapshotCase] = &[
    SnapshotCase {
        source: "tests/fixtures/valid/run_store.lullaby",
        snapshot: "tests/snapshots/run_store.memory.json",
    },
    SnapshotCase {
        source: "tests/fixtures/valid/run_array.lullaby",
        snapshot: "tests/snapshots/run_array.memory.json",
    },
];

#[test]
fn bytecode_memory_snapshots_match_checked_in_golden_files() {
    let repo = workspace_root();
    let ir_crate = ir_crate_root();
    let update = std::env::var_os(UPDATE_ENV).is_some();

    for case in CASES {
        let source_path = repo.join(case.source);
        let snapshot_path = ir_crate.join(case.snapshot);
        let actual = snapshot_for(&source_path);

        if update {
            if let Some(parent) = snapshot_path.parent() {
                fs::create_dir_all(parent).expect("create snapshot directory");
            }
            fs::write(&snapshot_path, &actual).expect("write memory snapshot");
            continue;
        }

        let expected = fs::read_to_string(&snapshot_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", snapshot_path.display()));
        assert_eq!(
            expected, actual,
            "IR memory metadata snapshot changed for {}.\nReview the backend metadata change, then refresh the checked-in golden files with PowerShell: `$env:LULLABY_UPDATE_IR_MEMORY_SNAPSHOTS='1'; cargo test -p lullaby_ir --test memory_snapshots; Remove-Item Env:LULLABY_UPDATE_IR_MEMORY_SNAPSHOTS`.",
            case.source
        );
    }
}

fn snapshot_for(source_path: &Path) -> String {
    let source = fs::read_to_string(source_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", source_path.display()));
    let tokens = lex(&source).expect("lex snapshot source");
    let program = parse(&tokens).expect("parse snapshot source");
    let checked = validate_executable(&program).expect("validate snapshot source");
    let ir = lower(&checked).expect("lower snapshot source");
    let bytecode = lower_to_bytecode(&ir);
    serialize_operations(&analyze_bytecode_memory_operations(&bytecode))
}

fn serialize_operations(operations: &[IrMemoryOperation]) -> String {
    let mut json = serde_json::to_string_pretty(operations).expect("serialize memory metadata");
    json.push('\n');
    json
}

fn workspace_root() -> PathBuf {
    ir_crate_root()
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn ir_crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
