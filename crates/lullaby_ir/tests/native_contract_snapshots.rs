use std::fs;
use std::path::PathBuf;

use lullaby_ir::native_contract::native_backend_contract;

const UPDATE_ENV: &str = "LULLABY_UPDATE_NATIVE_CONTRACT_SNAPSHOTS";
const SNAPSHOT: &str = "tests/snapshots/native_backend_contract.json";

#[test]
fn native_backend_contract_matches_checked_in_snapshot() {
    let snapshot_path = ir_crate_root().join(SNAPSHOT);
    let actual = serialize_contract();

    if std::env::var_os(UPDATE_ENV).is_some() {
        if let Some(parent) = snapshot_path.parent() {
            fs::create_dir_all(parent).expect("create native contract snapshot directory");
        }
        fs::write(&snapshot_path, &actual).expect("write native contract snapshot");
        return;
    }

    let expected = fs::read_to_string(&snapshot_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", snapshot_path.display()));
    assert_eq!(
        expected, actual,
        "native backend contract snapshot changed.\nReview the ABI/layout contract change, then refresh the checked-in golden file with PowerShell: `$env:LULLABY_UPDATE_NATIVE_CONTRACT_SNAPSHOTS='1'; cargo test -p lullaby_ir --test native_contract_snapshots; Remove-Item Env:LULLABY_UPDATE_NATIVE_CONTRACT_SNAPSHOTS`."
    );
}

fn serialize_contract() -> String {
    let mut json = serde_json::to_string_pretty(&native_backend_contract())
        .expect("serialize native backend contract");
    json.push('\n');
    json
}

fn ir_crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
