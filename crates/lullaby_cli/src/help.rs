//! The `--version` string and the `--help` text.

/// The user-facing version in the project's `MAJOR.PATCH-STATUS` scheme (see
/// documents/versioning.md), reconstructed from the semver `CARGO_PKG_VERSION`:
/// the scheme's PATCH is semver's minor (semver's patch is a `0` filler), and a
/// build with no prerelease suffix is `stable`. So `1.0.0-preview` renders as
/// `1.0-preview` and `1.0.0` as `1.0-stable`.
pub(crate) fn display_version() -> String {
    let full = env!("CARGO_PKG_VERSION");
    let (nums, status) = full.split_once('-').unwrap_or((full, "stable"));
    let mut parts = nums.split('.');
    let major = parts.next().unwrap_or("0");
    let minor = parts.next().unwrap_or("0");
    format!("{major}.{minor}-{status}")
}

pub(crate) fn print_help() {
    println!(
        "lullaby {}\n\nusage:\n  lullaby check [--verbose|--format json] <file.lby | project-dir | lullaby.json>\n  lullaby compile [--optimize none|constant-fold|dead-code|full] [-o output.lbc] [--verbose|--format json] <file.lby | project-dir | lullaby.json>\n  lullaby build [--optimize none|constant-fold|dead-code|full] [-o output.lbc] [--verbose|--format json] <file.lby | project-dir | lullaby.json>\n  lullaby inspect [--verbose|--format json] <file.lbc>\n  lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|full] [--verbose|--format json] <file.lby | project-dir | lullaby.json> [args...]\n  lullaby run [--verbose|--format json] <file.lbc>\n  lullaby test [--verbose] [--filter <substring>] [--timeout <seconds>] <file.lby | project-dir | lullaby.json>\n  lullaby wasm [--verbose] [-o out.wasm] <file.lby | project-dir | lullaby.json>\n  lullaby native [--verbose] [--freestanding|--no-std] [--debug|-g] [--fast-math] [--target <triple>] [-o out] <file.lby | project-dir | lullaby.json>\n  lullaby fmt [--write|--check] <file.lby>\n  lullaby new <name>\n  lullaby lsp\n  lullaby docs\n  lullaby examples\n  lullaby --version\n\nA <project-dir> is a directory containing a lullaby.json manifest; you may also\npass the lullaby.json path directly. A project may span multiple src directories\nand depend on other local Lullaby projects.",
        display_version()
    );
}
