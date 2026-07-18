//! Multi-file module loader.
//!
//! Lullaby is file-as-module: each `.lby` file is a module whose top-level
//! `fn`/`struct`/`enum`/`alias` declarations are file-private unless marked
//! `pub`. A file imports another module by name (`import NAME`), which loads
//! `NAME.lby` from the entry file's directory and makes its `pub` items
//! available unqualified.
//!
//! This loader is a *frontend-only* stage that runs before semantic analysis.
//! It lexes and parses the entry file plus every transitively imported module,
//! enforces the flat-namespace no-shadowing rule (`L0391`), cross-module
//! visibility (`L0392`), import-cycle rejection (`L0393`), and missing-module
//! resolution (`L0397`), then merges every module's declarations into a single
//! flat [`Program`]. Because the merged program is an ordinary single `Program`,
//! the semantic analyzer and all three backends run unchanged.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lullaby_diagnostics::{DiagnosticPhase, DiagnosticReport, Span};
use lullaby_lexer::{lex, validate_source_path};
use lullaby_parser::{
    AliasDecl, EnumDecl, Expr, ExprKind, Function, MatchArm, Program, Stmt, StructDecl, TypeRef,
    parse,
};

/// An in-memory override of on-disk source: `overlay_key(path) -> source text`.
///
/// When a module's resolved path is present in the overlay, the loader uses the
/// overlay's text instead of reading the file from disk. The CLI never uses this
/// (it always passes an empty overlay, so behavior is byte-for-byte the on-disk
/// path); the language server uses it to analyze the editor's live, possibly
/// unsaved buffers. Keys must be produced with [`overlay_key`] so callers and the
/// loader normalize paths identically.
pub type SourceOverlay = HashMap<PathBuf, String>;

/// Normalize a path into the key form used by [`SourceOverlay`]. Canonicalizes
/// when the file exists on disk (so the same file reached by different spellings
/// maps to one key), and falls back to the path as-written for a not-yet-saved
/// buffer whose file does not exist.
pub fn overlay_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// One loaded module exposed to callers: its module name, resolved path, source
/// text, and parsed AST. This is the per-file view the merged [`Program`]
/// deliberately flattens away; tools that need file identity (the language
/// server, for cross-file go-to-definition and hover) read it from here.
#[derive(Debug, Clone)]
pub struct ModuleSource {
    pub name: String,
    pub path: PathBuf,
    pub source: String,
    pub program: Program,
}

/// A merged multi-file program plus the source text of the entry file (used for
/// verbose diagnostic rendering downstream, exactly like the single-file path).
///
/// `modules` additionally exposes each loaded module (name, path, source, AST)
/// in load order, so a consumer can recover the file identity the merged
/// `program` flattens away. The CLI ignores it; the language server uses it for
/// cross-file resolution.
pub struct LoadedProgram {
    pub program: Program,
    pub entry_source: String,
    pub modules: Vec<ModuleSource>,
}

/// Snapshot the internal modules as the public [`ModuleSource`] view.
fn module_sources(modules: &[Module]) -> Vec<ModuleSource> {
    modules
        .iter()
        .map(|module| ModuleSource {
            name: module.name.clone(),
            path: module.path.clone(),
            source: module.source.clone(),
            program: module.program.clone(),
        })
        .collect()
}

/// One parsed module: its resolved path, source text, parsed AST, and the set
/// of module names it imports.
struct Module {
    name: String,
    path: PathBuf,
    source: String,
    program: Program,
}

/// Load the entry file and every module it transitively imports, enforce the
/// module rules, and return a single merged [`Program`]. On any error the whole
/// set of diagnostics is returned; the caller renders and reports them.
///
/// `import NAME` resolves `NAME.lby`
/// by searching, in order, the importing file's own directory and then every
/// directory in `search_dirs` (the project's `src` directories followed by the
/// `src` directories of each transitively resolved dependency). All module rules
/// (`L0391`/`L0392`/`L0393`/`L0397`) apply across the whole merged set exactly as
/// in the single-file case. Passing an empty `search_dirs` is byte-for-byte
/// identical to the legacy single-file behavior.
pub fn load_program_in_project(
    entry: &Path,
    search_dirs: &[PathBuf],
) -> Result<LoadedProgram, Vec<DiagnosticReport>> {
    load_program_in_project_with_overlay(entry, search_dirs, &SourceOverlay::new())
}

/// Like [`load_program_in_project`], but any module whose resolved path is in
/// `overlay` is read from the overlay's in-memory text instead of from disk.
/// Passing an empty overlay is identical to [`load_program_in_project`].
pub fn load_program_in_project_with_overlay(
    entry: &Path,
    search_dirs: &[PathBuf],
    overlay: &SourceOverlay,
) -> Result<LoadedProgram, Vec<DiagnosticReport>> {
    if let Err(diagnostic) = validate_source_path(entry) {
        return Err(vec![
            DiagnosticReport::new(diagnostic.code, DiagnosticPhase::Source, diagnostic.message)
                .with_source_path(entry.display().to_string())
                .with_span(diagnostic.span),
        ]);
    }

    let dir = entry.parent().map(Path::to_path_buf).unwrap_or_default();
    // The entry module's name is its file stem; imports resolve as siblings.
    let entry_name = entry
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("main")
        .to_string();

    let mut modules: Vec<Module> = Vec::new();
    let mut loaded: HashMap<String, usize> = HashMap::new();
    // `visiting` is the active DFS stack, used to detect import cycles.
    let mut visiting: HashSet<String> = HashSet::new();
    let mut diagnostics: Vec<DiagnosticReport> = Vec::new();

    load_module(
        &entry_name,
        entry,
        &dir,
        search_dirs,
        overlay,
        &mut modules,
        &mut loaded,
        &mut visiting,
        &mut diagnostics,
    );

    // Structural errors (parse failures, cycles, missing files) make later
    // visibility/shadowing checks meaningless, so stop here if any occurred.
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    check_duplicate_names(&modules, &mut diagnostics);
    check_visibility(&modules, &mut diagnostics);

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let merged = merge(&modules);
    let entry_source = modules
        .iter()
        .find(|module| module.name == entry_name)
        .map(|module| module.source.clone())
        .unwrap_or_default();

    Ok(LoadedProgram {
        program: merged,
        entry_source,
        modules: module_sources(&modules),
    })
}

/// Load *every* `.lby` module found across a project's source directories
/// (`search_dirs`), merge them, and enforce all module rules. This is the
/// entry-less path used by `check`/`test` on a library project: there is no
/// executable entry, so every module in the project (and its dependencies) is
/// loaded and validated as one merged program. Modules that only some other
/// module imports are still included, so unused-but-broken modules are caught.
pub fn load_library_project(
    search_dirs: &[PathBuf],
) -> Result<LoadedProgram, Vec<DiagnosticReport>> {
    load_library_project_with_overlay(search_dirs, &SourceOverlay::new())
}

/// Like [`load_library_project`], but any module whose resolved path is in
/// `overlay` is read from the overlay's in-memory text instead of from disk.
/// Passing an empty overlay is identical to [`load_library_project`].
pub fn load_library_project_with_overlay(
    search_dirs: &[PathBuf],
    overlay: &SourceOverlay,
) -> Result<LoadedProgram, Vec<DiagnosticReport>> {
    let mut modules: Vec<Module> = Vec::new();
    let mut loaded: HashMap<String, usize> = HashMap::new();
    let mut visiting: HashSet<String> = HashSet::new();
    let mut diagnostics: Vec<DiagnosticReport> = Vec::new();

    // Collect every `.lby` file in every search dir, in a deterministic order.
    let mut roots: Vec<PathBuf> = Vec::new();
    for dir in search_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut in_dir: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("lby"))
            .collect();
        in_dir.sort();
        roots.extend(in_dir);
    }

    for root in &roots {
        let name = root
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("module")
            .to_string();
        let dir = root.parent().map(Path::to_path_buf).unwrap_or_default();
        load_module(
            &name,
            root,
            &dir,
            search_dirs,
            overlay,
            &mut modules,
            &mut loaded,
            &mut visiting,
            &mut diagnostics,
        );
    }

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    check_duplicate_names(&modules, &mut diagnostics);
    check_visibility(&modules, &mut diagnostics);

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let merged = merge(&modules);
    let entry_source = modules
        .first()
        .map(|module| module.source.clone())
        .unwrap_or_default();

    Ok(LoadedProgram {
        program: merged,
        entry_source,
        modules: module_sources(&modules),
    })
}

/// Lex+parse one module and recurse over its imports. Cycle detection uses the
/// active DFS stack (`visiting`); already-fully-loaded modules are skipped.
#[allow(clippy::too_many_arguments)]
fn load_module(
    name: &str,
    path: &Path,
    dir: &Path,
    search_dirs: &[PathBuf],
    overlay: &SourceOverlay,
    modules: &mut Vec<Module>,
    loaded: &mut HashMap<String, usize>,
    visiting: &mut HashSet<String>,
    diagnostics: &mut Vec<DiagnosticReport>,
) {
    if loaded.contains_key(name) {
        return;
    }

    // An overlay entry (the editor's live buffer) takes precedence over disk;
    // fall back to reading the file when there is none.
    let source = match overlay.get(&overlay_key(path)) {
        Some(source) => source.clone(),
        None => match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                diagnostics.push(
                    DiagnosticReport::new(
                        "L0397",
                        DiagnosticPhase::Loader,
                        format!(
                            "failed to read module `{name}` at `{}`: {error}",
                            path.display()
                        ),
                    )
                    .with_source_path(path.display().to_string()),
                );
                return;
            }
        },
    };

    let tokens = match lex(&source) {
        Ok(tokens) => tokens,
        Err(lex_diagnostics) => {
            for diagnostic in lex_diagnostics {
                diagnostics.push(
                    DiagnosticReport::new(
                        diagnostic.code,
                        DiagnosticPhase::Lexer,
                        diagnostic.message,
                    )
                    .with_source_path(path.display().to_string())
                    .with_span(diagnostic.span),
                );
            }
            return;
        }
    };

    let program = match parse(&tokens) {
        Ok(program) => program,
        Err(parse_diagnostics) => {
            for diagnostic in parse_diagnostics {
                diagnostics.push(
                    DiagnosticReport::new(
                        diagnostic.code,
                        DiagnosticPhase::Parser,
                        diagnostic.message,
                    )
                    .with_source_path(path.display().to_string())
                    .with_span(diagnostic.span),
                );
            }
            return;
        }
    };

    visiting.insert(name.to_string());
    let imports = program.imports.clone();
    modules.push(Module {
        name: name.to_string(),
        path: path.to_path_buf(),
        source,
        program,
    });
    loaded.insert(name.to_string(), modules.len() - 1);

    for import in &imports {
        if visiting.contains(import) {
            diagnostics.push(
                DiagnosticReport::new(
                    "L0393",
                    DiagnosticPhase::Loader,
                    format!("import cycle detected: module `{name}` re-imports `{import}`"),
                )
                .with_source_path(path.display().to_string()),
            );
            continue;
        }
        // Resolve `import NAME` to `NAME.lby`, searching the importing file's own
        // directory first, then every project/dependency `src` directory. The
        // resolved module's *own* imports then resolve relative to its own
        // directory (plus the same search dirs), so a dependency module can
        // import its siblings without seeing the importer's directory.
        let file_name = format!("{import}.lby");
        let import_path = resolve_import_path(&file_name, dir, search_dirs)
            .unwrap_or_else(|| dir.join(&file_name));
        let import_dir = import_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dir.to_path_buf());
        load_module(
            import,
            &import_path,
            &import_dir,
            search_dirs,
            overlay,
            modules,
            loaded,
            visiting,
            diagnostics,
        );
    }
    visiting.remove(name);
}

/// Resolve `file_name` (e.g. `math.lby`) against the importing file's directory
/// first, then each search directory in order. Returns the first existing path,
/// or `None` if none exists (the caller then falls back to the importer's
/// directory so the missing-module `L0397` diagnostic points at a sensible path).
fn resolve_import_path(file_name: &str, dir: &Path, search_dirs: &[PathBuf]) -> Option<PathBuf> {
    let local = dir.join(file_name);
    if local.is_file() {
        return Some(local);
    }
    for search_dir in search_dirs {
        let candidate = search_dir.join(file_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The set of top-level declaration names (`fn`/`struct`/`enum`/`alias`) a
/// module declares itself.
fn declared_names(program: &Program) -> HashSet<String> {
    let mut names = HashSet::new();
    for function in &program.functions {
        names.insert(function.name.clone());
    }
    for decl in &program.structs {
        names.insert(decl.name.clone());
    }
    for decl in &program.enums {
        names.insert(decl.name.clone());
    }
    for alias in &program.aliases {
        names.insert(alias.name.clone());
    }
    for decl in &program.consts {
        names.insert(decl.name.clone());
    }
    for decl in &program.actors {
        names.insert(decl.name.clone());
    }
    names
}

/// The `pub`-exported top-level declaration names of a module.
fn public_names(program: &Program) -> HashSet<String> {
    let mut names = HashSet::new();
    for function in &program.functions {
        if function.is_public {
            names.insert(function.name.clone());
        }
    }
    for decl in &program.structs {
        if decl.is_public {
            names.insert(decl.name.clone());
        }
    }
    for decl in &program.enums {
        if decl.is_public {
            names.insert(decl.name.clone());
        }
    }
    for alias in &program.aliases {
        if alias.is_public {
            names.insert(alias.name.clone());
        }
    }
    for decl in &program.consts {
        if decl.is_public {
            names.insert(decl.name.clone());
        }
    }
    for decl in &program.actors {
        if decl.is_public {
            names.insert(decl.name.clone());
        }
    }
    names
}

/// `L0391`: a top-level name declared in more than one loaded module. The flat
/// merged namespace forbids shadowing, whether the colliding items are public or
/// private.
fn check_duplicate_names(modules: &[Module], diagnostics: &mut Vec<DiagnosticReport>) {
    // name -> (owning module name, declaration span)
    let mut seen: HashMap<String, (String, Span)> = HashMap::new();
    for module in modules {
        for (name, span) in module_declarations(&module.program) {
            if let Some((other_module, _)) = seen.get(&name) {
                diagnostics.push(
                    DiagnosticReport::new(
                        "L0391",
                        DiagnosticPhase::Loader,
                        format!(
                            "name `{name}` is declared in both module `{other_module}` and module `{}`",
                            module.name
                        ),
                    )
                    .with_source_path(module.path.display().to_string())
                    .with_span(span),
                );
            } else {
                seen.insert(name, (module.name.clone(), span));
            }
        }
    }
}

/// Every top-level declaration name of a module paired with its span.
fn module_declarations(program: &Program) -> Vec<(String, Span)> {
    let mut items = Vec::new();
    for function in &program.functions {
        items.push((function.name.clone(), function.span));
    }
    for decl in &program.structs {
        items.push((decl.name.clone(), decl.span));
    }
    for decl in &program.enums {
        items.push((decl.name.clone(), decl.span));
    }
    for alias in &program.aliases {
        items.push((alias.name.clone(), alias.span));
    }
    for decl in &program.consts {
        items.push((decl.name.clone(), decl.span));
    }
    for decl in &program.actors {
        items.push((decl.name.clone(), decl.span));
    }
    items
}

/// `L0392`: a module references a user-declared name that is not visible to it —
/// either a private name owned by another module, or a `pub` name from a module
/// it did not `import`.
///
/// Only *user-declared* names (those declared as `fn`/`struct`/`enum`/`alias`
/// somewhere in the merged program) are subject to this check. Builtins,
/// primitive types, enum variants, and local variables are never user
/// declarations, so they are never flagged.
fn check_visibility(modules: &[Module], diagnostics: &mut Vec<DiagnosticReport>) {
    // Global map: declaration name -> owning module name.
    let mut owner: HashMap<String, String> = HashMap::new();
    for module in modules {
        for name in declared_names(&module.program) {
            owner.insert(name, module.name.clone());
        }
    }
    let public_by_module: HashMap<String, HashSet<String>> = modules
        .iter()
        .map(|module| (module.name.clone(), public_names(&module.program)))
        .collect();

    for module in modules {
        let own = declared_names(&module.program);
        // Visible names = this module's own declarations plus the pub names of
        // every module it imports.
        let mut visible: HashSet<String> = own.clone();
        for import in &module.program.imports {
            if let Some(names) = public_by_module.get(import) {
                visible.extend(names.iter().cloned());
            }
        }

        let mut referenced: Vec<(String, Span)> = Vec::new();
        for function in &module.program.functions {
            collect_function_references(function, &mut referenced);
        }
        for decl in &module.program.structs {
            collect_struct_references(decl, &mut referenced);
        }
        for decl in &module.program.enums {
            collect_enum_references(decl, &mut referenced);
        }
        for alias in &module.program.aliases {
            collect_alias_references(alias, &mut referenced);
        }

        for (name, span) in referenced {
            // Only user-declared names are checked; skip anything that is not a
            // declaration somewhere in the merged program.
            let Some(owning_module) = owner.get(&name) else {
                continue;
            };
            if visible.contains(&name) {
                continue;
            }
            // Not visible: report why (private vs. not imported).
            let is_public = public_by_module
                .get(owning_module)
                .is_some_and(|names| names.contains(&name));
            let reason = if is_public {
                format!(
                    "`{name}` is `pub` in module `{owning_module}` but module `{}` does not `import {owning_module}`",
                    module.name
                )
            } else {
                format!(
                    "`{name}` is private to module `{owning_module}` and cannot be used from module `{}`",
                    module.name
                )
            };
            diagnostics.push(
                DiagnosticReport::new("L0392", DiagnosticPhase::Loader, reason)
                    .with_source_path(module.path.display().to_string())
                    .with_span(span),
            );
        }
    }
}

fn collect_function_references(function: &Function, out: &mut Vec<(String, Span)>) {
    for param in &function.params {
        collect_type_references(&param.ty, function.span, out);
    }
    collect_type_references(&function.return_type, function.span, out);
    collect_block_references(&function.body, out);
}

fn collect_struct_references(decl: &StructDecl, out: &mut Vec<(String, Span)>) {
    for field in &decl.fields {
        collect_type_references(&field.ty, decl.span, out);
    }
}

fn collect_enum_references(decl: &EnumDecl, out: &mut Vec<(String, Span)>) {
    for variant in &decl.variants {
        for ty in &variant.payload {
            collect_type_references(ty, decl.span, out);
        }
    }
}

fn collect_alias_references(alias: &AliasDecl, out: &mut Vec<(String, Span)>) {
    collect_type_references(&alias.target, alias.span, out);
}

fn collect_block_references(body: &[Stmt], out: &mut Vec<(String, Span)>) {
    for stmt in body {
        collect_stmt_references(stmt, out);
    }
}

fn collect_stmt_references(stmt: &Stmt, out: &mut Vec<(String, Span)>) {
    match stmt {
        Stmt::Let { ty, value, .. } => {
            if let Some(ty) = ty {
                collect_type_references(ty, value.span, out);
            }
            collect_expr_references(value, out);
        }
        Stmt::Assign { value, .. } => collect_expr_references(value, out),
        Stmt::Return(Some(expr)) => collect_expr_references(expr, out),
        Stmt::Return(None) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Asm { .. } => {}
        Stmt::Expr(expr) => collect_expr_references(expr, out),
        Stmt::If {
            branches,
            else_body,
            ..
        } => {
            for branch in branches {
                collect_expr_references(&branch.condition, out);
                collect_block_references(&branch.body, out);
            }
            collect_block_references(else_body, out);
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_expr_references(condition, out);
            collect_block_references(body, out);
        }
        Stmt::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            collect_expr_references(start, out);
            collect_expr_references(end, out);
            if let Some(step) = step {
                collect_expr_references(step, out);
            }
            collect_block_references(body, out);
        }
        Stmt::ForEach { iterable, body, .. } => {
            collect_expr_references(iterable, out);
            collect_block_references(body, out);
        }
        Stmt::Loop { body, .. } | Stmt::Unsafe { body, .. } | Stmt::RegionBlock { body, .. } => {
            collect_block_references(body, out)
        }
        Stmt::Region(_) => {}
        Stmt::Throw { value, .. } => collect_expr_references(value, out),
        Stmt::Try {
            body, catch_body, ..
        } => {
            collect_block_references(body, out);
            collect_block_references(catch_body, out);
        }
    }
}

fn collect_expr_references(expr: &Expr, out: &mut Vec<(String, Span)>) {
    match &expr.kind {
        ExprKind::Integer(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::String(_)
        | ExprKind::Char(_)
        | ExprKind::Variable(_) => {}
        ExprKind::Array(values) => {
            for value in values {
                collect_expr_references(value, out);
            }
        }
        ExprKind::ArrayFill { value, count } => {
            collect_expr_references(value, out);
            collect_expr_references(count, out);
        }
        ExprKind::Index { target, index } => {
            collect_expr_references(target, out);
            collect_expr_references(index, out);
        }
        ExprKind::Unary { expr, .. } => collect_expr_references(expr, out),
        ExprKind::Binary { left, right, .. } => {
            collect_expr_references(left, out);
            collect_expr_references(right, out);
        }
        ExprKind::Call { name, args } => {
            // A call names a function (or a struct constructor / enum variant).
            // Function and struct names are user declarations; enum variants and
            // builtins are not, and are filtered out by the caller.
            out.push((name.clone(), expr.span));
            for arg in args {
                collect_expr_references(arg, out);
            }
        }
        ExprKind::StructLiteral { name, fields } => {
            out.push((name.clone(), expr.span));
            for (_, value) in fields {
                collect_expr_references(value, out);
            }
        }
        ExprKind::Field { target, .. } => collect_expr_references(target, out),
        ExprKind::Await { expr } => collect_expr_references(expr, out),
        ExprKind::Try(inner) => collect_expr_references(inner, out),
        // `spawn NAME(args)` names an actor declaration (a top-level name). A
        // trailing `supervise POLICY` clause names no declaration — the policy is
        // a fixed contextual word, not a reference — so it is skipped here.
        ExprKind::Spawn { actor, args, .. } => {
            out.push((actor.clone(), expr.span));
            for arg in args {
                collect_expr_references(arg, out);
            }
        }
        // `tell TARGET.HANDLER(args)`: the handler is not a top-level name, but the
        // target and arguments may reference top-level declarations.
        ExprKind::Tell { target, args, .. } => {
            collect_expr_references(target, out);
            for arg in args {
                collect_expr_references(arg, out);
            }
        }
        // A closure body may reference top-level names (call a function), so
        // recurse into it for module dependency analysis.
        ExprKind::Closure { body, .. } => collect_expr_references(body, out),
        ExprKind::Match { scrutinee, arms } => {
            collect_expr_references(scrutinee, out);
            for MatchArm { body, .. } in arms {
                collect_block_references(body, out);
            }
        }
        ExprKind::Conditional {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_expr_references(cond, out);
            collect_expr_references(then_branch, out);
            collect_expr_references(else_branch, out);
        }
        ExprKind::In { value, collection } => {
            collect_expr_references(value, out);
            collect_expr_references(collection, out);
        }
        ExprKind::Slice { target, start, end } => {
            collect_expr_references(target, out);
            if let Some(start) = start {
                collect_expr_references(start, out);
            }
            if let Some(end) = end {
                collect_expr_references(end, out);
            }
        }
        // `join_all`/`select` name no top-level declaration; the operand collection
        // may reference declarations, so recurse into it.
        ExprKind::Combinator { operand, .. } => collect_expr_references(operand, out),
    }
}

/// Collect every user-defined type name referenced by a type spelling. Generic
/// spellings such as `list<Point>` or `map<string, Shape>` are split so nested
/// type names are checked too; primitive/builtin type names are simply absent
/// from the declaration map and thus ignored by the caller.
fn collect_type_references(ty: &TypeRef, span: Span, out: &mut Vec<(String, Span)>) {
    for name in type_names(&ty.name) {
        out.push((name, span));
    }
}

/// Break a type spelling into its component identifier names, descending into
/// generic and function-type arguments.
fn type_names(spelling: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = String::new();
    for ch in spelling.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            names.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        names.push(current);
    }
    names
}

/// Concatenate every loaded module's declarations into one flat [`Program`].
/// Visibility and shadowing were already enforced, so the merged program is an
/// ordinary single-file program from semantics' point of view.
fn merge(modules: &[Module]) -> Program {
    let mut functions = Vec::new();
    let mut aliases = Vec::new();
    let mut structs = Vec::new();
    let mut enums = Vec::new();
    let mut traits = Vec::new();
    let mut impls = Vec::new();
    let mut consts = Vec::new();
    let mut actors = Vec::new();
    // The merged compilation unit is freestanding if any module declares the
    // `no-runtime` directive. This is the conservative (default-deny) choice for
    // the tier gate: a `no-runtime` module in the build enforces the freestanding
    // rules over the merged program. Per-module tier granularity in mixed-tier
    // multi-file projects is a later freestanding-tier stage.
    let mut is_no_runtime = false;
    for module in modules {
        functions.extend(module.program.functions.iter().cloned());
        aliases.extend(module.program.aliases.iter().cloned());
        structs.extend(module.program.structs.iter().cloned());
        enums.extend(module.program.enums.iter().cloned());
        traits.extend(module.program.traits.iter().cloned());
        impls.extend(module.program.impls.iter().cloned());
        consts.extend(module.program.consts.iter().cloned());
        actors.extend(module.program.actors.iter().cloned());
        is_no_runtime = is_no_runtime || module.program.is_no_runtime;
    }
    Program {
        functions,
        aliases,
        structs,
        enums,
        imports: Vec::new(),
        traits,
        impls,
        consts,
        actors,
        is_no_runtime,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A unique on-disk directory, cleaned up on drop.
    struct TempDir {
        dir: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("lullaby_loader_{}_{unique}", std::process::id()));
            fs::create_dir_all(&dir).expect("create temp dir");
            Self { dir }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.dir.join(name);
            fs::write(&path, contents).expect("write file");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn exposes_each_loaded_module_with_file_identity() {
        let temp = TempDir::new();
        let entry = temp.write(
            "main.lby",
            "import math\n\nfn main -> i64\n    return square(3)\n",
        );
        temp.write("math.lby", "pub fn square x i64 -> i64\n    return x * x\n");

        let loaded = load_program_in_project(&entry, &[]).expect("load ok");
        // The merged program flattens both files' functions together...
        assert!(loaded.program.functions.iter().any(|f| f.name == "square"));
        assert!(loaded.program.functions.iter().any(|f| f.name == "main"));
        // ...while `modules` preserves per-file identity for tooling.
        let math = loaded
            .modules
            .iter()
            .find(|module| module.name == "math")
            .expect("math module exposed");
        assert!(math.program.functions.iter().any(|f| f.name == "square"));
        assert!(math.path.ends_with("math.lby"));
    }

    #[test]
    fn overlay_takes_precedence_over_disk() {
        let temp = TempDir::new();
        // On-disk entry references an undefined name; the overlay replaces it with
        // a valid buffer, so the load succeeds.
        let entry = temp.write("main.lby", "fn main -> i64\n    return nope(0)\n");
        let mut overlay = SourceOverlay::new();
        overlay.insert(
            overlay_key(&entry),
            "fn main -> i64\n    return 0\n".to_string(),
        );

        let loaded = load_program_in_project_with_overlay(&entry, &[], &overlay)
            .expect("overlay buffer loads");
        assert_eq!(loaded.entry_source, "fn main -> i64\n    return 0\n");

        // Without the overlay the on-disk (broken) source is what loads.
        let disk = load_program_in_project(&entry, &[]).expect("disk load parses");
        assert!(disk.entry_source.contains("nope"));
    }

    #[test]
    fn empty_overlay_matches_the_on_disk_path() {
        let temp = TempDir::new();
        let entry = temp.write("main.lby", "fn main -> i64\n    return 7\n");
        let with_overlay =
            load_program_in_project_with_overlay(&entry, &[], &SourceOverlay::new()).expect("ok");
        let plain = load_program_in_project(&entry, &[]).expect("ok");
        assert_eq!(with_overlay.entry_source, plain.entry_source);
        assert_eq!(
            with_overlay.program.functions.len(),
            plain.program.functions.len()
        );
    }
}
