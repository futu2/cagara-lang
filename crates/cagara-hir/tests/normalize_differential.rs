//! The two query routes, side by side, on the same programs.
//!
//! # What this compares
//!
//! `normalize::definition` — the normalizer that is replacing the slice-ordered
//! lowering — and `typed_lower::lower` — the lowering it replaces — both reduce
//! a definition's body and both end at an erased [`Rel`]. For every query
//! definition this test runs both and compares the relations.
//!
//! While the consolidation is in progress the normalizer is incomplete, so a
//! mismatch is *expected* for shapes it cannot yet build; the point of the run
//! is to say exactly which ones, and to refuse to let the list grow. The list
//! lives in `snapshots/normalize_mismatches.txt`, one ordinary line per program and
//! definition, and a run that produces a longer list than the recorded one
//! fails.
//!
//! # Why the corpus is what it is
//!
//! Every `.cagara` file in `examples/` and in the HIR corpus, plus the inline
//! shapes from `compile::tests::all_query_forms_compile_without_a_gap`. The
//! corpus is the programs users write; the inline shapes are the constructs
//! the suite promises are all supported. Between them every stage, join, set
//! operation, window, aggregate, helper and alias spelling is exercised.
//!
//! # What is not compared
//!
//! Only relations are compared, not the checked trees behind them. The two
//! routes build different intermediate structures by construction — that is the
//! whole point of the consolidation — and what has to agree is the product.

use cagara_hir::check::SchemeView;
use cagara_hir::{check, CompilerInput, Workspace};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn snapshot_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/normalize_mismatches.txt")
}

/// Every source file the repository compiles.
fn sources() -> Vec<PathBuf> {
    let root = repo_root();
    let mut files = Vec::new();
    for dir in [
        root.join("examples"),
        root.join("crates/cagara-hir/tests/corpus"),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut in_dir: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "cagara"))
            .collect();
        in_dir.sort();
        files.extend(in_dir);
    }
    files
}

const TABLES: &str = "users : query { id = int, name = string, age = int } = \
                      table \"public\" \"users\"\n\
                      orders : query { id = int, user_id = int, amount = float } = \
                      table \"public\" \"orders\"\n\
                      admins : query { id = int, name = string, age = int } = \
                      table \"public\" \"admins\"\n";

/// The inline shapes, copied from `compile::tests::all_query_forms_compile_without_a_gap`.
///
/// Copied rather than shared: that test is a *unit* test inside the crate and
/// this is an integration test outside it, and the point of the copy is that a
/// shape added to one list is visibly added to the other rather than silently
/// inherited.
const PROGRAMS: &[(&str, &str)] = &[
    ("a filter", "q = users & where (.age > 1)\n"),
    ("a projection", "q = users & select { .name }\n"),
    (
        "a join",
        "q = users & innerJoin orders (.<id == .>user_id)\n",
    ),
    ("a set operation", "q = users & union admins\n"),
    (
        "an aggregate",
        "q = users & agg { a = group .age, n = count }\n",
    ),
    (
        "a window",
        "q = users & select { r = rowNumber {order = [asc .id]} }\n",
    ),
    ("a named stage", "s = select { .id }\nq = users & s\n"),
    (
        "a composed stage",
        "s = where (.age > 1) >>> select { .id }\nq = users & s\n",
    ),
    (
        "an inline lambda stage",
        "q = users & (x => where (.age > 1) x)\n",
    ),
    (
        "a case",
        "q = users & select { x = case { .age > 1 => \"a\"; true => \"b\" } }\n",
    ),
    (
        "a pipeline",
        "q = users & where (.age > 1) & select { .id } & order [asc .id] & limit 2\n",
    ),
];

/// What one definition's two routes produced.
struct Verdict {
    label: String,
    program: String,
    name: String,
    legacy: Result<cagara_hir::ir::Rel, String>,
    normalizer: Result<cagara_hir::ir::Rel, String>,
}

impl Verdict {
    fn agrees(&self) -> bool {
        match (&self.legacy, &self.normalizer) {
            (Ok(legacy), Ok(normalizer)) => legacy == normalizer,
            _ => false,
        }
    }

    /// One line, for the recorded list.
    fn line(&self) -> String {
        format!("{} :: `{}`", self.program, self.name)
    }

    /// The detail behind a mismatch: what each route produced, truncated so the
    /// report stays readable and beginning at the first place they differ.
    fn detail(&self) -> String {
        format!(
            "  legacy: {}\n  normalizer:   {}",
            describe(&self.legacy),
            describe(&self.normalizer)
        )
    }
}

fn describe(result: &Result<cagara_hir::ir::Rel, String>) -> String {
    match result {
        Ok(rel) => {
            let text = format!("{rel:?}");
            if text.len() <= 400 {
                text
            } else {
                let mut cut = 400;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}…", &text[..cut])
            }
        }
        Err(message) => {
            let mut text = message.replace('\n', " ");
            if text.len() > 400 {
                let mut cut = 400;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                text.push('…');
            }
            format!("ERR {text}")
        }
    }
}

/// Compare both routes over one workspace's definitions.
fn compare_workspace(label: &str, ws: &Workspace, verdicts: &mut Vec<Verdict>) {
    let tc = check(ws);
    let root = ws.root;
    let input = CompilerInput::new(ws, &tc);
    for (def, definition) in ws.modules[root].module.defs.iter().enumerate() {
        // Only query definitions: the other roots have no relation to compare,
        // and a definition that failed checking has no trustworthy body.
        if !matches!(tc.scheme_view(root, def), Some(SchemeView::Query { .. })) {
            continue;
        }
        if tc.error_for(root, def).is_some() {
            continue;
        }
        let legacy = cagara_hir::legacy_debug_for_tests(input, root, def, &definition.body);
        let normalizer = cagara_hir::normalize_debug_for_tests(input, root, def, &definition.body);
        verdicts.push(Verdict {
            label: label.to_string(),
            program: format!("{label} def {def}"),
            name: definition.name.clone(),
            legacy,
            normalizer,
        });
    }
}

#[test]
fn both_routes_build_the_same_relation() {
    let mut verdicts = Vec::new();
    let mut files = 0usize;
    for path in sources() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let label = path
            .strip_prefix(repo_root())
            .unwrap_or(&path)
            .display()
            .to_string();
        let ws = Workspace::from_source(&text);
        compare_workspace(&label, &ws, &mut verdicts);
        files += 1;
    }
    assert!(files >= 10, "only {files} source files were found");
    for (label, body) in PROGRAMS {
        let ws = Workspace::from_source(&format!("{TABLES}{body}"));
        compare_workspace(&format!("inline: {label}"), &ws, &mut verdicts);
    }

    let compared = verdicts.len();
    let mut mismatches: BTreeMap<String, String> = BTreeMap::new();
    for verdict in verdicts.iter().filter(|verdict| !verdict.agrees()) {
        mismatches
            .entry(verdict.line())
            .or_insert_with(|| verdict.detail());
    }
    let agreements = compared - mismatches.len();

    eprintln!(
        "normalizer differential: {compared} query definitions over {} programs, \
         {agreements} agree, {} do not",
        files + PROGRAMS.len(),
        mismatches.len()
    );
    for (line, detail) in &mismatches {
        eprintln!("MISMATCH {line}\n{detail}");
    }

    let recorded = mismatches
        .keys()
        .map(|line| format!("{line}\n{}\n", mismatches[line]))
        .collect::<Vec<_>>()
        .join("");

    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        let path = snapshot_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("snapshot directory");
        }
        std::fs::write(&path, &recorded).expect("write the mismatch list");
        return;
    }

    let previous = std::fs::read_to_string(snapshot_path()).unwrap_or_default();
    let allowed: Vec<&str> = previous
        .lines()
        .filter(|line| !line.starts_with("  ") && !line.trim().is_empty())
        .collect();
    let now: Vec<&str> = mismatches.keys().map(String::as_str).collect();
    let grew: Vec<&str> = now
        .iter()
        .copied()
        .filter(|line| !allowed.contains(line))
        .collect();
    let fixed: Vec<&str> = allowed
        .iter()
        .copied()
        .filter(|line| !now.contains(line))
        .collect();

    assert!(
        grew.is_empty(),
        "the normalizer lost ground on {} shape(s) (recorded: {}, now: {}):\n  {}\n\
         Re-record with UPDATE_SNAPSHOTS=1 only if the change is understood.",
        grew.len(),
        allowed.len(),
        now.len(),
        grew.join("\n  ")
    );
    if !fixed.is_empty() {
        eprintln!(
            "{} shape(s) now agree and can be removed from the recorded list \
             (re-record with UPDATE_SNAPSHOTS=1):\n  {}",
            fixed.len(),
            fixed.join("\n  ")
        );
    }
}
