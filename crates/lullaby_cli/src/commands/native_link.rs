//! The best-effort link step behind `lullaby native`: discover `rust-lld` and the
//! MSVC library search paths, then invoke the linker. Every failure mode degrades
//! gracefully — the relocatable object is always kept.

use std::{
    env,
    path::{Path, PathBuf},
};

/// The result of the best-effort link step.
pub(crate) enum LinkOutcome {
    /// A `.exe` was produced.
    Linked,
    /// The toolchain (rust-lld or kernel32.lib) could not be located.
    Unavailable(String),
    /// The linker ran but returned an error.
    Failed(String),
}

/// Discover `rust-lld` and the library search paths, then invoke it in lld-link
/// mode to produce a console `.exe`. `kernel32.lib` (for `ExitProcess`) is always
/// required; the entry stub bypasses the CRT via `/entry:`. `import_libs` adds
/// any C import libraries an `extern fn` call needs (e.g. `ucrt.lib`), discovered
/// the same way. When rust-lld or any required import library cannot be located,
/// linking degrades gracefully (the object is kept, the reason is reported).
pub(crate) fn link_native_object(
    obj: &Path,
    exe: &Path,
    entry_symbol: &str,
    import_libs: &[String],
) -> LinkOutcome {
    let Some(lld) = find_rust_lld() else {
        return LinkOutcome::Unavailable("rust-lld not found in the rustc sysroot".to_string());
    };
    let lib_paths = discover_lib_paths();
    // Every library the object needs must be discoverable on the search path
    // before we attempt the link; otherwise degrade gracefully.
    let mut required_libs: Vec<String> = vec!["kernel32.lib".to_string()];
    for lib in import_libs {
        if !required_libs.iter().any(|existing| existing == lib) {
            required_libs.push(lib.clone());
        }
    }
    for lib in &required_libs {
        if !lib_paths.iter().any(|dir| dir.join(lib).is_file()) {
            return LinkOutcome::Unavailable(format!(
                "{lib} not found (set the MSVC `LIB` environment variable, e.g. run from a Developer Command Prompt)"
            ));
        }
    }

    let mut command = std::process::Command::new(&lld);
    command.args(["-flavor", "link", "/nologo", "/subsystem:console"]);
    command.arg(format!("/entry:{entry_symbol}"));
    command.arg(format!("/out:{}", exe.display()));
    for dir in &lib_paths {
        command.arg(format!("/libpath:{}", dir.display()));
    }
    command.arg(obj);
    for lib in &required_libs {
        command.arg(lib);
    }

    match command.output() {
        Ok(out) if out.status.success() => LinkOutcome::Linked,
        Ok(out) => {
            let mut detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if detail.is_empty() {
                detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
            LinkOutcome::Failed(detail)
        }
        Err(error) => LinkOutcome::Failed(format!("could not run rust-lld: {error}")),
    }
}

/// Locate `rust-lld.exe` under `<rustc --print sysroot>/lib/rustlib/...`.
fn find_rust_lld() -> Option<PathBuf> {
    let output = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let candidate = PathBuf::from(sysroot)
        .join("lib")
        .join("rustlib")
        .join("x86_64-pc-windows-msvc")
        .join("bin")
        .join("rust-lld.exe");
    candidate.is_file().then_some(candidate)
}

/// Collect library search directories: the MSVC `LIB` environment variable (set
/// in a Developer Command Prompt) split on `;`, plus any it names that exist.
fn discover_lib_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(lib) = env::var("LIB") {
        for entry in lib.split(';') {
            let entry = entry.trim();
            if !entry.is_empty() {
                let dir = PathBuf::from(entry);
                if dir.is_dir() {
                    paths.push(dir);
                }
            }
        }
    }
    paths
}
