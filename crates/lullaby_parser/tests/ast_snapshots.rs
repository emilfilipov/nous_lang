use std::fs;
use std::path::{Path, PathBuf};

use lullaby_lexer::lex;
use lullaby_parser::{Program, parse};

const UPDATE_ENV: &str = "LULLABY_UPDATE_PARSER_SNAPSHOTS";

struct SnapshotCase {
    source: &'static str,
    snapshot: &'static str,
}

const CASES: &[SnapshotCase] = &[
    SnapshotCase {
        source: "tests/fixtures/valid/run_arithmetic.lby",
        snapshot: "tests/snapshots/run_arithmetic.ast.json",
    },
    SnapshotCase {
        source: "tests/fixtures/valid/run_array.lby",
        snapshot: "tests/snapshots/run_array.ast.json",
    },
    SnapshotCase {
        source: "tests/fixtures/valid/run_for_step.lby",
        snapshot: "tests/snapshots/run_for_step.ast.json",
    },
    SnapshotCase {
        source: "tests/fixtures/valid/docs_cleanup_helper.lby",
        snapshot: "tests/snapshots/docs_cleanup_helper.ast.json",
    },
];

#[test]
fn parser_ast_snapshots_match_checked_in_golden_files() {
    let repo = workspace_root();
    let parser_crate = parser_crate_root();
    let update = std::env::var_os(UPDATE_ENV).is_some();

    for case in CASES {
        let source_path = repo.join(case.source);
        let snapshot_path = parser_crate.join(case.snapshot);
        let actual = snapshot_for(&source_path);

        if update {
            if let Some(parent) = snapshot_path.parent() {
                fs::create_dir_all(parent).expect("create snapshot directory");
            }
            fs::write(&snapshot_path, &actual).expect("write parser snapshot");
            continue;
        }

        let expected = fs::read_to_string(&snapshot_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", snapshot_path.display()));
        assert_eq!(
            expected, actual,
            "parser AST snapshot changed for {}.\nReview the parser change, then refresh the checked-in golden files with PowerShell: `$env:LULLABY_UPDATE_PARSER_SNAPSHOTS='1'; cargo test -p lullaby_parser --test ast_snapshots; Remove-Item Env:LULLABY_UPDATE_PARSER_SNAPSHOTS`.",
            case.source
        );
    }
}

fn snapshot_for(source_path: &Path) -> String {
    let source = fs::read_to_string(source_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", source_path.display()));
    let tokens = lex(&source).expect("lex snapshot source");
    let program = parse(&tokens).expect("parse snapshot source");
    serialize_program(&program)
}

fn serialize_program(program: &Program) -> String {
    let mut json = serde_json::to_string_pretty(program).expect("serialize AST");
    json.push('\n');
    json
}

fn workspace_root() -> PathBuf {
    parser_crate_root()
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn parser_crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
