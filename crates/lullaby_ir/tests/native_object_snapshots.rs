use std::fs;
use std::path::PathBuf;

use lullaby_ir::native_object::{emit_coff_object, snapshot_native_object};
use lullaby_ir::{lower, lower_to_bytecode};
use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_semantics::validate_executable;

const UPDATE_ENV: &str = "LULLABY_UPDATE_NATIVE_OBJECT_SNAPSHOTS";

struct SnapshotCase {
    source: &'static str,
    snapshot: &'static str,
}

const CASES: &[SnapshotCase] = &[
    SnapshotCase {
        source: "fn main -> i64\n    return 42\n",
        snapshot: "tests/snapshots/return_42.coff.json",
    },
    SnapshotCase {
        source: "fn main -> i64\n    let left i64 = 40\n    let right i64 = 2\n    return left + right\n",
        snapshot: "tests/snapshots/locals_add.coff.json",
    },
    SnapshotCase {
        source: "fn main -> i64\n    let value i64 = 40\n    value += 2\n    value *= 2\n    value -= 42\n    return value\n",
        snapshot: "tests/snapshots/assignments.coff.json",
    },
];

#[test]
fn coff_objects_match_checked_in_snapshots() {
    let ir_crate = ir_crate_root();
    let update = std::env::var_os(UPDATE_ENV).is_some();

    for case in CASES {
        let snapshot_path = ir_crate.join(case.snapshot);
        let actual = snapshot_for(case.source);

        if update {
            if let Some(parent) = snapshot_path.parent() {
                fs::create_dir_all(parent).expect("create native object snapshot directory");
            }
            fs::write(&snapshot_path, &actual).expect("write native object snapshot");
            continue;
        }

        let expected = fs::read_to_string(&snapshot_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", snapshot_path.display()));
        assert_eq!(
            expected, actual,
            "native object snapshot changed for {}.\nReview the object-emission change, then refresh the checked-in golden file with PowerShell: `$env:LULLABY_UPDATE_NATIVE_OBJECT_SNAPSHOTS='1'; cargo test -p lullaby_ir --test native_object_snapshots; Remove-Item Env:LULLABY_UPDATE_NATIVE_OBJECT_SNAPSHOTS`.",
            case.snapshot
        );
    }
}

/// The COFF machine-code bytes for a source program (whole object, which embeds
/// the `.text` section) — used to check for specific emitted instructions.
fn object_bytes(source: &str) -> Vec<u8> {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate_executable(&program).expect("validate");
    let ir = lower(&checked).expect("lower");
    let bytecode = lower_to_bytecode(&ir);
    // The full native emitter (arrays, floats, strings) — same path the CLI uses,
    // unlike the i64-only prototype `emit_coff_object`.
    lullaby_ir::native_object::emit_native_program(&bytecode)
        .expect("emit")
        .bytes
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn sum_reduction_over_i64_array_is_auto_vectorized() {
    // `for i: s += a[i]` over an `array<i64>` emits SSE2 packed instructions.
    let vectorized = object_bytes(
        "fn main -> i64\n    let a array<i64> = [1, 2, 3, 4, 5, 6, 7, 8]\n    let s i64 = 0\n    for i from 0 to 7\n        s += a[i]\n    s\n",
    );
    assert!(
        contains(&vectorized, &[0x66, 0x0F, 0xD4, 0xC1]), // paddq xmm0, xmm1
        "expected a paddq in the vectorized sum reduction"
    );
    assert!(
        contains(&vectorized, &[0xF3, 0x0F, 0x6F, 0x09]), // movdqu xmm1, [rcx]
        "expected a movdqu in the vectorized sum reduction"
    );

    // A body that is not a bare `s += a[i]` (here `s += a[i] * 2`) must NOT be
    // vectorized — the detector is specific and falls back to the scalar loop.
    let scalar = object_bytes(
        "fn main -> i64\n    let a array<i64> = [1, 2, 3, 4, 5, 6, 7, 8]\n    let s i64 = 0\n    for i from 0 to 7\n        s += a[i] * 2\n    s\n",
    );
    assert!(
        !contains(&scalar, &[0x66, 0x0F, 0xD4, 0xC1]),
        "a non-matching loop body must not be vectorized"
    );
}

fn snapshot_for(source: &str) -> String {
    let tokens = lex(source).expect("lex native object snapshot source");
    let program = parse(&tokens).expect("parse native object snapshot source");
    let checked = validate_executable(&program).expect("validate native object snapshot source");
    let ir = lower(&checked).expect("lower native object snapshot source");
    let bytecode = lower_to_bytecode(&ir);
    let object = emit_coff_object(&bytecode).expect("emit native object snapshot");
    let mut json =
        serde_json::to_string_pretty(&snapshot_native_object(&object)).expect("serialize snapshot");
    json.push('\n');
    json
}

fn ir_crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
