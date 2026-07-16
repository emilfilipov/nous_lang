//! The project- and environment-level commands that do not compile anything:
//! `lullaby new`, `lullaby examples`, `lullaby docs`, and `lullaby lsp`.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use lullaby_loader::manifest;

/// The canonical hosted documentation website. Lullaby's documentation lives
/// online only (maintained separately); there is no bundled offline HTML doc
/// artifact.
const DOCS_URL: &str = "https://lullaby-lang.org";

pub(crate) fn docs() -> Result<(), String> {
    println!("docs: {DOCS_URL}");
    Ok(())
}

/// Run the Language Server Protocol server over stdio. This blocks, servicing
/// JSON-RPC requests from an editor client until the client sends `exit` (or
/// closes stdin). All request handling lives in the `lullaby_lsp` crate.
pub(crate) fn lsp() -> Result<(), String> {
    lullaby_lsp::run_stdio().map_err(|error| format!("lsp server error: {error}"))
}

pub(crate) fn examples() -> Result<(), String> {
    let path = locate_examples().ok_or_else(|| {
        "examples not found; expected examples/valid near the lullaby binary or tests/fixtures/valid in the repository".to_string()
    })?;
    println!("examples: {}", path.display());
    Ok(())
}

fn locate_examples() -> Option<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(exe) = env::current_exe()
        && let Some(bin_dir) = exe.parent()
    {
        candidates.push(bin_dir.join("examples/valid"));
        candidates.push(bin_dir.join("../examples/valid"));
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(manifest_dir.join("../../examples/valid"));
    candidates.push(manifest_dir.join("../../tests/fixtures/valid"));
    candidates.push(PathBuf::from("examples/valid"));
    candidates.push(PathBuf::from("tests/fixtures/valid"));

    candidates.into_iter().find(|path| path.is_dir())
}

/// Scaffold a new Lullaby project in a fresh directory named after the project.
///
/// Creates `<name>/lullaby.json`, a runnable `<name>/src/main.lby`, and a
/// `.gitignore`, then prints the next step. Fails with a clear message if the
/// name is not a valid identifier or the target directory already exists — the
/// scaffold is never written over an existing directory.
pub(crate) fn new_project(name: PathBuf) -> Result<(), String> {
    let name = name.to_string_lossy().into_owned();
    if !is_valid_project_name(&name) {
        return Err(format!(
            "invalid project name `{name}`: start with a letter or underscore, then use \
             letters, digits, or underscores (e.g. `bedtime`, `my_app`)"
        ));
    }

    let root = PathBuf::from(&name);
    if root.exists() {
        return Err(format!(
            "`{name}` already exists here; choose another name or remove it first"
        ));
    }
    let src = root.join("src");
    fs::create_dir_all(&src)
        .map_err(|error| format!("could not create `{}`: {error}", src.display()))?;

    let manifest_name = manifest::MANIFEST_FILE_NAME;
    let manifest_json = format!(
        "{{\n  \"name\": \"{name}\",\n  \"version\": \"0.1.0\",\n  \"entry\": \"src/main.lby\",\n  \"src\": [\"src\"]\n}}\n"
    );
    let main_lby = format!(
        "# {name} — a new Lullaby project.\n\
         # Run it with:  lullaby run {name}\n\n\
         fn main -> void\n    println(\"hello from {name}\")\n"
    );
    let gitignore = "/target\n*.lbc\n";

    write_new_file(&root.join(manifest_name), &manifest_json)?;
    write_new_file(&src.join("main.lby"), &main_lby)?;
    write_new_file(&root.join(".gitignore"), gitignore)?;

    println!("created {name}/");
    println!("  {name}/{manifest_name}");
    println!("  {name}/src/main.lby");
    println!("\nnext:  lullaby run {name}");
    Ok(())
}

/// A project name that is also a valid Lullaby identifier, so the project can be
/// imported by name as a dependency: an ASCII letter or `_`, then ASCII
/// alphanumerics or `_`.
fn is_valid_project_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Write a freshly scaffolded file, mapping any I/O error to a CLI message.
fn write_new_file(path: &Path, contents: &str) -> Result<(), String> {
    fs::write(path, contents)
        .map_err(|error| format!("could not write `{}`: {error}", path.display()))
}
