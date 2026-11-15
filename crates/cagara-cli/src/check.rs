//! `cagara check`: report every file under a path that does not check.
//!
//! One answer for a tree of files: which of them do not check, and why. The
//! single-file path answers "what SQL does this program mean"; this answers "is
//! this directory sound", which is what a pre-commit hook and an editor's project
//! mode need. It compiles nothing to SQL: a file is checked as a *library*, so a
//! directory of table declarations is reportable without one of them being `main`.

use crate::{display, done, out, usage, USAGE};
use cagara_hir::{check, Workspace};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

struct Found {
    /// `error` or `warning`: only the former fails the run.
    severity: &'static str,
    path: String,
    line: usize,
    col: usize,
    message: String,
    /// The rewrite the diagnostic knows, when it knows one. `check` is also a
    /// library entry point for scripts, so the fix travels with the diagnostic
    /// rather than being re-derived from the text by each consumer.
    suggestions: Vec<(String, String)>,
}

impl Found {
    fn text(&self) -> String {
        let mut line = format!(
            "{}:{}:{}: {}: {}",
            self.path, self.line, self.col, self.severity, self.message
        );
        for (title, edit) in &self.suggestions {
            line.push_str(&format!("\n  fix: {title}"));
            if !edit.is_empty() {
                line.push_str(&format!(" — {edit}"));
            }
        }
        line
    }

    /// Whether this diagnostic fails the run. A warning is reported and the
    /// run succeeds: the program compiles and means what its types say.
    fn fails(&self) -> bool {
        self.severity == "error"
    }

    /// One JSON object per line, so a consumer can read the stream without a
    /// parser for the whole document, and a truncated run still yields the
    /// diagnostics it got to.
    fn json(&self) -> String {
        fn esc(s: &str) -> String {
            let mut o = String::with_capacity(s.len() + 2);
            for c in s.chars() {
                match c {
                    '"' => o.push_str("\\\""),
                    '\\' => o.push_str("\\\\"),
                    '\n' => o.push_str("\\n"),
                    '\r' => o.push_str("\\r"),
                    '\t' => o.push_str("\\t"),
                    c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
                    c => o.push(c),
                }
            }
            o
        }
        let fixes: Vec<String> = self
            .suggestions
            .iter()
            .map(|(title, edit)| {
                format!(
                    "{{\"title\":\"{}\",\"edit\":\"{}\"}}",
                    esc(title),
                    esc(edit)
                )
            })
            .collect();
        format!(
            "{{\"severity\":\"{}\",\"path\":\"{}\",\"line\":{},\"column\":{},\"message\":\"{}\",\"fixes\":[{}]}}",
            self.severity,
            esc(&self.path),
            self.line,
            self.col,
            esc(&self.message),
            fixes.join(",")
        )
    }
}

/// The fixes a diagnostic carries, as (title, what the text would become).
///
/// The edits are spans and insertions; applying them needs the source, which is
/// here. A consumer that wants to *offer* the fix rather than read it (the
/// language server) applies the edits itself, from `Diag::suggestions`.
fn fixes(ws: &Workspace, d: &cagara_hir::workspace::Diag) -> Vec<(String, String)> {
    use cagara_hir::workspace::SuggestionEdit;
    let text = &ws.modules[ws.root].text;
    d.suggestions
        .iter()
        .map(|s| {
            let rendered: Vec<String> = s
                .edits
                .iter()
                .map(|e| {
                    let slice = |sp: &cagara_syntax::ast::Span| {
                        text.get(sp.start as usize..sp.end as usize)
                            .unwrap_or_default()
                    };
                    match e {
                        SuggestionEdit::Replace { span, text: new } => {
                            format!("{} → {new}", slice(span))
                        }
                        SuggestionEdit::Surround {
                            span,
                            before,
                            after,
                        } => format!("{} → {before}{}{after}", slice(span), slice(span)),
                        SuggestionEdit::Retoken { from, to, .. } => format!("{from} → {to}"),
                    }
                })
                .collect();
            (s.title.clone(), rendered.join(", ").replace(", ", " "))
        })
        .collect()
}

/// Every `*.cagara` file under `path`, in a stable order. A file path is itself.
fn sources_under(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !path.is_dir() {
        out.push(path.to_path_buf());
        return Ok(out);
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                // A dot directory is a build artifact or an editor's cache,
                // not part of the tree being checked.
                if entry
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
                {
                    continue;
                }
                stack.push(entry);
            } else if entry.extension().is_some_and(|e| e == "cagara") {
                out.push(entry);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// `cagara check [--json] <path>...`: report every file that does not check.
///
/// Exit codes: `0` all files check, `1` at least one diagnostic, `2` a usage
/// error. The distinction is what lets a hook say "this is broken" and "you
/// called me wrong" differently.
pub(crate) fn run(args: impl Iterator<Item = OsString>) -> std::io::Result<ExitCode> {
    let mut json = false;
    let mut paths: Vec<PathBuf> = Vec::new();
    for a in args {
        match a.to_str() {
            Some("--json") => json = true,
            Some("-h") | Some("--help") => {
                out(USAGE)?;
                return Ok(ExitCode::SUCCESS);
            }
            _ if a.to_string_lossy().starts_with("--") => {
                return Ok(usage(&format!(
                    "unexpected argument `{}` for `cagara check`",
                    display(&a)
                )))
            }
            _ => paths.push(PathBuf::from(a)),
        }
    }
    if paths.is_empty() {
        return Ok(usage("`cagara check` needs a file or a directory"));
    }
    let mut files = Vec::new();
    for p in &paths {
        match sources_under(p) {
            Ok(found) => files.extend(found),
            Err(e) => {
                eprintln!("{}: {e}", p.display());
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    // One diagnostic, once — however many workspaces reach it.
    //
    // `check .` opens a workspace per file, and a file that imports another sees
    // that file's diagnostics too: an error in `lib.cagara` arrives once from
    // `main.cagara`'s workspace and again from `lib.cagara`'s own. Both carry the
    // same path, position and message, so the report listed it twice and a build
    // system counting errors counted two. The key is the whole rendered
    // diagnostic, because two *different* problems can share a position (a
    // `select` field with two bad columns reports one error per column, and the
    // record spans both).
    let mut seen: HashSet<String> = HashSet::new();
    let mut failed = false;
    for file in &files {
        let ws = Workspace::open(file);
        let mut found: Vec<Found> = Vec::new();
        for d in &ws.diags {
            found.push(Found {
                severity: d.severity.as_str(),
                path: d.path.clone(),
                line: d.line,
                col: d.col,
                message: d.message.clone(),
                suggestions: fixes(&ws, d),
            });
        }
        if ws.diags.is_empty() {
            let tc = check(&ws);
            for e in tc.errors() {
                found.push(Found {
                    severity: e.diag.severity.as_str(),
                    path: e.diag.path.clone(),
                    line: e.diag.line,
                    col: e.diag.col,
                    message: e.diag.message.clone(),
                    suggestions: fixes(&ws, &e.diag),
                });
            }
            for w in tc.warnings() {
                found.push(Found {
                    severity: w.diag.severity.as_str(),
                    path: w.diag.path.clone(),
                    line: w.diag.line,
                    col: w.diag.col,
                    message: w.diag.message.clone(),
                    suggestions: fixes(&ws, &w.diag),
                });
            }
        }
        for f in found {
            // A suggestion list is part of the diagnostic: the same message at the
            // same position with a different fix offered is a different report.
            let key = if json { f.json() } else { f.text() };
            if !seen.insert(key) {
                continue;
            }
            if json {
                out(&f.json())?;
            } else {
                eprintln!("{}", f.text());
            }
            failed |= f.fails();
        }
    }
    Ok(done(failed))
}
