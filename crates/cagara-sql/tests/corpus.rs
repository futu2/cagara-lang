//! The compiler corpus: every shipped program, run through the whole pipeline.
//!
//! The property tests in `cagara-hir`'s `compile/tests.rs` assert what the
//! compiler *must* do (no capability gaps, no checker errors on legal
//! programs). This file asserts what the shipped programs *mean*, so a change
//! to checking, elaboration, or lowering cannot quietly alter a documented
//! example.
//!
//! # Why a snapshot and not just "it compiles"
//!
//! "Every example compiles" is covered elsewhere. The gap this closes is
//! *drift*: a refactor that keeps examples compiling while changing their
//! inferred types or emitted SQL would pass every existing test. The snapshot
//! records the three things a user can observe about a definition — whether it
//! was accepted, what type it has, and what SQL it produces — so a diff is
//! reviewable instead of invisible.
//!
//! # Adding a case
//!
//! Add a `.cagara` file to `tests/corpus/` and run with `CAGARA_BLESS=1` to
//! write its snapshot. Without that variable the snapshot is compared, so CI
//! fails on any unreviewed change.

use cagara_hir::{check, root_queries_checked, root_verdicts, Fault, TypeCheck, Workspace};
use cagara_sql::{compile, Dialect, Options};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where the additional corpus cases live, and where snapshots are written.
///
/// The shipped `examples/` are covered too, and read from their real location
/// rather than copied here: a copy would drift from the file the README links
/// to, and the point of the corpus is to pin what users actually see.
fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../cagara-hir/tests/corpus")
}

/// The repository root, two levels above this crate.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every program in the corpus: the shipped examples first, then the extra
/// cases. Sorted within each group for a stable order.
fn corpus_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let examples = repo_root().join("examples");
    if let Ok(entries) = std::fs::read_dir(&examples) {
        let mut found: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "cagara"))
            .collect();
        found.sort();
        files.extend(found);
    }
    if let Ok(entries) = std::fs::read_dir(corpus_dir()) {
        let mut found: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "cagara"))
            .collect();
        found.sort();
        files.extend(found);
    }
    files
}

/// Where a program's snapshot is written: beside it under `snapshots/`,
/// mirroring the source file's directory so examples and corpus cases with the
/// same name cannot collide.
fn snapshot_path(source: &Path) -> PathBuf {
    let root = repo_root();
    let corpus = corpus_dir();
    // An example strips against the repository root, so it keeps its `examples/`
    // component. A corpus case comes from `corpus_dir()` itself, which is not
    // below the root in path-component terms — the root reaches it through a
    // `..`, and `strip_prefix` is lexical — so it is stripped against the corpus
    // directory instead and lands at `snapshots/<name>.snap`. Without that
    // second attempt the strip fails, `join` sees an absolute path and discards
    // its base, and a case's snapshot is written beside the case rather than
    // under `snapshots/`, where this function's documentation says it goes.
    let relative = source
        .strip_prefix(&root)
        .or_else(|_| source.strip_prefix(&corpus))
        .unwrap_or(source);
    let mut snap = corpus.join("snapshots").join(relative);
    snap.set_extension("snap");
    snap
}

fn def_index(ws: &Workspace, name: &str) -> usize {
    ws.modules[ws.root]
        .module
        .defs
        .iter()
        .position(|d| d.name == name)
        .unwrap_or(usize::MAX)
}

/// One definition's observable behavior, as a single snapshot line.
///
/// Kept to one line per definition so a snapshot diff names the definition
/// that changed rather than reprinting a block.
fn describe(ws: &Workspace, tc: &TypeCheck, name: &str, dialect: Dialect) -> String {
    let verdict_list = root_verdicts(ws, tc);
    let verdicts: BTreeMap<&str, &Fault> = verdict_list
        .iter()
        .map(|(n, _, f)| (n.as_str(), f))
        .collect();
    if let Some(fault) = verdicts.get(name) {
        // A definition the compiler could not build. Distinguish the two
        // kinds: `Program` is the user's to fix, `Compiler` is this project's.
        let kind = match fault {
            Fault::Program => "rejected",
            Fault::Compiler => "COMPILER-GAP",
            Fault::NotAQuery => "not-a-query",
        };
        return format!("{name}: {kind}");
    }
    let index = def_index(ws, name);
    if let Some(error) = tc.error_for(ws.root, index) {
        return format!("{name}: type-error: {}", error.message);
    }
    let Some(ty) = tc.type_of(ws.root, index) else {
        return format!("{name}: <no type recorded>");
    };
    let sql = root_queries_checked(ws, tc)
        .into_iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, r)| r.ok())
        .and_then(|rel| {
            compile(
                &rel,
                Options {
                    dialect,
                    pretty: false,
                    optimize: false,
                },
            )
            .ok()
        });
    match sql {
        // Collapse whitespace: the snapshot is about meaning, and the
        // formatter's line breaks are already pinned by `cagara-fmt`'s tests.
        Some(sql) => format!(
            "{name}: {ty} => {}",
            sql.split_whitespace().collect::<Vec<_>>().join(" ")
        ),
        None => format!("{name}: {ty}"),
    }
}

/// Render a whole file's snapshot.
fn snapshot(path: &Path) -> String {
    let ws = Workspace::open(path);
    let tc = check(&ws);
    let mut out = String::new();
    out.push_str(&format!(
        "# {}\n",
        path.file_name().unwrap().to_string_lossy()
    ));
    // Loader diagnostics first: a file that does not parse has no definitions
    // to describe, and saying so is more useful than an empty snapshot.
    for diag in &ws.diags {
        out.push_str(&format!("! loader: {}\n", diag.message));
    }
    let names: Vec<String> = ws.modules[ws.root]
        .module
        .defs
        .iter()
        .map(|d| d.name.clone())
        .collect();
    if names.is_empty() {
        out.push_str("(no definitions)\n");
    }
    for name in names {
        out.push_str(&describe(&ws, &tc, &name, Dialect::Ansi));
        out.push('\n');
    }
    out
}

/// The corpus round-trips: every shipped example definition still checks,
/// elaborates, and lowers to the SQL its snapshot records.
#[test]
fn corpus_matches_its_snapshot() {
    let bless = std::env::var_os("CAGARA_BLESS").is_some();
    let files = corpus_files();
    assert!(
        !files.is_empty(),
        "no corpus files found in {}",
        corpus_dir().display()
    );
    let mut failures = Vec::new();
    for path in &files {
        let rendered = snapshot(path);
        let snap = snapshot_path(path);
        if let Some(parent) = snap.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::read_to_string(&snap) {
            Ok(existing) if existing == rendered => {}
            Ok(_) if bless => {
                std::fs::write(&snap, &rendered).expect("write snapshot");
                eprintln!("blessed {}", snap.display());
            }
            Ok(existing) => failures.push(format!(
                "{}\n--- recorded ---\n{existing}\n--- actual ---\n{rendered}",
                snap.display()
            )),
            Err(_) if bless => {
                std::fs::write(&snap, &rendered).expect("write snapshot");
                eprintln!("created {}", snap.display());
            }
            Err(_) => failures.push(format!(
                "{} has no snapshot; run with CAGARA_BLESS=1 to create it:\n{rendered}",
                snap.display()
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} corpus file(s) changed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// No corpus program may hit a compiler capability gap.
///
/// This is the corpus half of the totality gate: the property tests in
/// `cagara-hir` use small synthetic programs, while this one runs the programs
/// the project actually ships. A gap here means a documented, user-visible
/// construct regressed.
#[test]
fn no_corpus_program_hits_a_compiler_gap() {
    for path in corpus_files() {
        let ws = Workspace::open(&path);
        let tc = check(&ws);
        for (name, _, fault) in root_verdicts(&ws, &tc) {
            assert_ne!(
                fault,
                Fault::Compiler,
                "{} / `{name}` hit a compiler capability gap",
                path.display()
            );
        }
    }
}
