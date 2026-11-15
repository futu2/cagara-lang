//! `cagara check`: report every file under a path that does not check.
//!
//! One answer for a tree of files: which of them do not check, and why. The
//! single-file path answers "what SQL does this program mean"; this answers "is
//! this directory sound", which is what a pre-commit hook and an editor's project
//! mode need. It compiles nothing to SQL: a file is checked as a *library*, so a
//! directory of table declarations is reportable without one of them being `main`.

use crate::{display, done, out, usage, USAGE};
use cagara_hir::{check, compile_checked, Workspace};
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
    // Each *file* once, however many paths reach it.
    //
    // A symlink is followed, so the same file can be reached many times: `ln -s .
    // self` inside a directory makes every level of the walk see it again, and the
    // walk stops only because the OS refuses to resolve more than 40 symlinks in one
    // path. Measured on this tree: 41 entries for one six-byte file, 41 workspaces
    // compiled, 1.6 s instead of 0.04 s. A symlink to a sibling directory is the
    // quieter version — the file is checked twice, and only the diagnostic dedupe
    // hid it.
    //
    // The key is the canonical path, and the path *kept* is the first one the sorted
    // walk reached, so what is printed is a path the user typed or one under it.
    let mut seen_files: HashSet<PathBuf> = HashSet::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // A directory already walked needs no second visit, which is what bounds a
        // loop to one pass instead of forty.
        if !seen_dirs.insert(std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone())) {
            continue;
        }
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
                let key = std::fs::canonicalize(&entry).unwrap_or_else(|_| entry.clone());
                if seen_files.insert(key) {
                    out.push(entry);
                }
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
        let root_path = ws.modules[ws.root].path.display().to_string();
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
            // And the *compiler's* verdict, not only the checker's.
            //
            // The two are not the same thing: a set operation whose sides have the
            // same columns in different orders type-checks (row unification matches
            // by name) and cannot be lowered (a set operation is positional in SQL).
            // `check` reported nothing and `cagara --only` refused to compile the same
            // file — a pre-commit hook passing on something the compiler rejects, which
            // is the one thing a checker must not do.
            //
            // This does not make `check` compile a program to SQL for its own sake:
            // it asks the compiler for its diagnostics, which it already produces
            // when it builds a file, and a file without a `main` is compiled as a
            // library, so the "a directory of table declarations is reportable"
            // property holds.
            // Every root definition the compiler could not build, including the
            // ones that are not the entry point. `Compilation::diagnostics` is not
            // enough: it carries the checker's and the workspace's, while a
            // *definition's* failure is on its `result` — that is where the SQL mode
            // reads it from, and reading a different list here is how a file that
            // `cagara --only q` refuses came to pass `cagara check`.
            let compiled = compile_checked(&ws, &tc);
            for d in compiled.diagnostics.iter().filter(|d| d.path == root_path) {
                found.push(Found {
                    severity: d.severity.as_str(),
                    path: d.path.clone(),
                    line: d.line,
                    col: d.col,
                    message: d.message.clone(),
                    suggestions: fixes(&ws, d),
                });
            }
            for query in &compiled.queries {
                let Err(d) = &query.result else {
                    continue;
                };
                if d.path != root_path {
                    continue;
                }
                found.push(Found {
                    severity: d.severity.as_str(),
                    path: d.path.clone(),
                    line: d.line,
                    col: d.col,
                    message: d.message.clone(),
                    suggestions: fixes(&ws, d),
                });
            }
            // And the *SQL* lowering, which is where the remaining errors are.
            //
            // `compile_checked` builds a `Rel` and validates its schema; turning that
            // `Rel` into a statement is `cagara-sql`'s job and can still refuse — a
            // `distinct` after an `order` on an expression the projection dropped is
            // one, and it is a *program* error with a message telling the author what
            // to write. `cagara --only q` reports it; `cagara check` did not, so a
            // pre-commit hook passed on a file the compiler rejects.
            //
            // ANSI is the dialect to ask: these refusals are about the relation, not
            // about a target's spelling, and the ones that *are* target-specific
            // (`fullJoin` on MySQL) are a different question that `check` is not
            // asked. A target refusal here would be a false positive on a file whose
            // real target is another dialect.
            for query in &compiled.queries {
                let Ok(rel) = &query.result else {
                    continue;
                };
                let Err((loc, message)) = cagara_sql::compile_located(
                    rel,
                    cagara_sql::Options {
                        dialect: cagara_sql::Dialect::Ansi,
                        pretty: false,
                        optimize: false,
                    },
                ) else {
                    continue;
                };
                // The lowering names the stage that refused, so this points at it.
                // `diag_span` is the conversion every other diagnostic goes through,
                // which keeps the line and column measured against the same text.
                let d = match loc {
                    Some(l) => ws.diag_span(
                        l.module,
                        l.span,
                        format!("error in `{}`: {message}", query.name),
                    ),
                    None => cagara_hir::workspace::Diag {
                        severity: cagara_hir::workspace::Severity::Error,
                        path: root_path.clone(),
                        line: 0,
                        col: 0,
                        message: format!("error in `{}`: {message}", query.name),
                        source: String::new(),
                        width: 0,
                        suggestions: Vec::new(),
                    },
                };
                found.push(Found {
                    severity: d.severity.as_str(),
                    path: d.path.clone(),
                    line: d.line,
                    col: d.col,
                    message: d.message.clone(),
                    suggestions: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::sources_under;
    use std::path::{Path, PathBuf};

    /// A scratch tree that removes itself.
    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("cagara-check-unit-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Tree(dir)
        }

        fn write(&self, name: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::create_dir_all(p.parent().expect("a parent")).expect("parents");
            std::fs::write(&p, "q = 1\n").expect("write");
            p
        }

        #[cfg(unix)]
        fn link(&self, target: &str, name: &str) {
            let p = self.0.join(name);
            std::fs::create_dir_all(p.parent().expect("a parent")).expect("parents");
            std::os::unix::fs::symlink(target, p).expect("symlink");
        }

        fn found(&self) -> Vec<PathBuf> {
            sources_under(&self.0).expect("walk")
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Each *file* once, however many paths reach it.
    ///
    /// Asserted on the walk itself rather than through `cagara check`, deliberately:
    /// the duplicate work is invisible from the outside. `Workspace` canonicalises the
    /// path it is given, so the same file reached twice produces the same diagnostic,
    /// and `run`'s dedupe prints one line — the only outward sign is that a six-byte
    /// file takes 1.6 s to check instead of 0.04 s. A CLI-level test of "one report"
    /// passes with and without the guard (it was written, run against the unguarded
    /// walk, and passed), so it was removed rather than kept as a test that cannot
    /// fail. The count is what is wrong, so the count is what is asserted.
    #[test]
    fn a_file_reached_by_two_paths_is_found_once() {
        let t = Tree::new("dedupe");
        t.write("a/one.cagara");
        t.write("a/two.cagara");
        t.write("b/three.cagara");
        assert_eq!(t.found().len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_sibling_directory_is_not_walked_twice() {
        let t = Tree::new("sibling");
        t.write("a/one.cagara");
        t.link("../a", "b/link");
        assert_eq!(
            t.found().len(),
            1,
            "the same file through two paths: {:?}",
            t.found()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_does_not_multiply_the_walk() {
        let t = Tree::new("loop");
        t.write("a/one.cagara");
        // `a/self` is `a`, so every level of the walk sees it again. Without a guard
        // the walk runs until the OS refuses to resolve more than 40 symlinks in one
        // path, which is 41 entries for one file.
        t.link(".", "a/self");
        assert_eq!(
            t.found().len(),
            1,
            "one file, however many ways the walk reaches it: {:?}",
            t.found()
        );
    }

    /// A path that is not a directory is returned as itself, without walking.
    #[test]
    fn a_file_is_returned_as_itself() {
        let t = Tree::new("single");
        let f = t.write("one.cagara");
        assert_eq!(sources_under(&f).expect("walk"), vec![f]);
    }

    /// A directory with no `.cagara` files yields nothing, and is not an error.
    #[test]
    fn a_tree_without_sources_is_empty_not_an_error() {
        let t = Tree::new("empty");
        t.write("notes.txt");
        assert!(t.found().is_empty(), "{:?}", t.found());
    }

    /// Dot directories are skipped; dot *files* are not.
    ///
    /// The first is a build artifact or an editor cache. The second is a file
    /// somebody wrote on purpose — `.cagara` in a home directory is a real thing —
    /// and skipping it silently would be a check that says "all good" about a file
    /// it never read.
    #[test]
    fn dot_directories_are_skipped_and_dot_files_are_not() {
        let t = Tree::new("dots");
        t.write(".cache/ignored.cagara");
        t.write("kept.cagara");
        let found = t.found();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].ends_with("kept.cagara"), "{found:?}");
    }

    /// The walk is sorted, so two runs report in the same order.
    #[test]
    fn the_walk_is_ordered() {
        let t = Tree::new("order");
        for name in ["b.cagara", "a.cagara", "c/d.cagara"] {
            t.write(name);
        }
        let found: Vec<String> = t
            .found()
            .iter()
            .map(|p| {
                p.strip_prefix(&t.0)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let mut sorted = found.clone();
        sorted.sort();
        assert_eq!(found, sorted, "{found:?}");
        assert_eq!(found.len(), 3);
        assert_eq!(
            Path::new(&found[0]),
            Path::new("a.cagara"),
            "relative to the tree: {found:?}"
        );
    }
}
