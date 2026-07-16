//! `lullaby fmt` — canonical formatting.

use std::{fs, path::PathBuf};

use lullaby_diagnostics::DiagnosticPhase;
use lullaby_lexer::{lex_with_comments, validate_source_path};
use lullaby_parser::{format_program_with_comments, parse};

use crate::args::{FmtMode, OutputMode};
use crate::diagnostics::{format_reports, frontend_report};

/// Format a `.lby` source file: print the canonical rendering to stdout
/// (default), rewrite it in place (`--write`), or verify it is already
/// canonical and exit non-zero otherwise (`--check`).
pub(crate) fn fmt_file(path: PathBuf, fmt_mode: FmtMode) -> Result<(), String> {
    if let Err(diagnostic) = validate_source_path(&path) {
        return Err(format_reports(
            &[frontend_report(diagnostic, DiagnosticPhase::Source, &path)],
            OutputMode::Concise,
            None,
        ));
    }
    let source = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read `{}`: {error}", path.display()))?;
    // Capture comment trivia alongside the tokens so the formatter can re-emit
    // comments; `lullaby fmt` must never destroy them.
    let (tokens, comments) = lex_with_comments(&source).map_err(|diagnostics| {
        format_reports(
            &diagnostics
                .into_iter()
                .map(|diagnostic| frontend_report(diagnostic, DiagnosticPhase::Lexer, &path))
                .collect::<Vec<_>>(),
            OutputMode::Concise,
            Some(&source),
        )
    })?;
    let program = parse(&tokens).map_err(|diagnostics| {
        format_reports(
            &diagnostics
                .into_iter()
                .map(|diagnostic| frontend_report(diagnostic, DiagnosticPhase::Parser, &path))
                .collect::<Vec<_>>(),
            OutputMode::Concise,
            Some(&source),
        )
    })?;
    let formatted = format_program_with_comments(&program, &comments);
    match fmt_mode {
        FmtMode::Print => print!("{formatted}"),
        FmtMode::Write => {
            if formatted != source {
                fs::write(&path, &formatted)
                    .map_err(|error| format!("failed to write `{}`: {error}", path.display()))?;
                println!("formatted {}", path.display());
            }
        }
        FmtMode::Check => {
            if formatted != source {
                return Err(format!(
                    "{} is not canonically formatted; run `lullaby fmt --write {}`",
                    path.display(),
                    path.display()
                ));
            }
        }
    }
    Ok(())
}
