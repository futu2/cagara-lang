//! The pipeline's observable output, pinned as text.
//!
//! # Why this exists
//!
//! The consolidation that replaces the slice-ordered typed lowering with one
//! normalizer and one relational arena must not change what a program means,
//! where a failure is blamed, or who is at fault. The checkable part of that
//! claim is the compiler's *output*: for every program in the repository, the
//! erased relation of each query, the fault classification of each definition,
//! and every diagnostic with its location.
//!
//! This file records exactly that and compares it against a checked-in
//! snapshot. It deliberately names no internal type: it cannot be "updated to
//! the new model" by accident, because it does not know the model. A step of
//! the consolidation that changes a single byte fails here, and the diff says
//! which program and which definition moved.
//!
//! # What is deliberately not compared
//!
//! Nothing is stripped. The `Rel` rendering keeps its locations and origins,
//! because a consolidation that reassigned a stage's origin would still print
//! the same SQL while blaming the wrong line, and the compiler's diagnostics
//! are part of its behavior.

use cagara_hir::{check, compile_input, erase, CheckedQuery, CompilerInput, Workspace};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every source file the repository compiles: the shipped examples plus the
/// HIR corpus, which between them exercise every stage, join, set operation,
/// window, aggregate, helper and alias spelling the language has.
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

/// Programs written here rather than in a file: the shape coverage the unit
/// suites exercise, kept at the pipeline boundary so a consolidation cannot
/// quietly narrow it.
const INLINE_PROGRAMS: &[(&str, &str)] = &[
    (
        "scalar expressions",
        "q = users & select { a = .id, b = .name, c = upper .name, d = .id + 1 }\n",
    ),
    (
        "composition",
        "q = users & select { x = (trim >>> upper) .name }\n",
    ),
    (
        "inline lambda",
        "q = users & select { x = (f => upper f) .name }\n",
    ),
    (
        "aliases",
        "f = upper\ng = f\nq = users & select { x = g .name }\n",
    ),
    (
        "composed alias",
        "up = upper\np = trim >>> up\nq = users & select { x = p .name }\n",
    ),
    (
        "conditional",
        "q = users & select { x = ifThenElse .active 1 2 }\n",
    ),
    (
        "case",
        "q = users & select { x = case { .id > 1 => \"a\"; true => \"b\" } }\n",
    ),
    ("membership", "q = users & where (inList [\"a\"] .name)\n"),
    ("filter", "q = users & where (.age > 1)\n"),
    ("projection", "q = users & select { .name }\n"),
    ("join", "q = users & innerJoin orders (.<id == .>user_id)\n"),
    (
        "left join",
        "q = users & leftJoin orders (.<id == .>user_id)\n",
    ),
    ("piped join", "q = users ?< orders (.<id == .>user_id)\n"),
    ("set operation", "q = users & union admins\n"),
    ("infix set", "q = users &| admins\n"),
    ("except", "q = users &~ admins\n"),
    (
        "aggregate",
        "q = users & agg { a = group .age, n = count }\n",
    ),
    (
        "window",
        "q = users & select { r = rowNumber {order = [asc .id]} }\n",
    ),
    ("named stage", "s = select { .id }\nq = users & s\n"),
    (
        "composed stage",
        "s = where (.age > 1) >>> select { .id }\nq = users & s\n",
    ),
    (
        "inline lambda stage",
        "q = users & (x => where (.age > 1) x)\n",
    ),
    (
        "pipeline",
        "q = users & where (.age > 1) & select { .id } & order [asc .id] & limit 2\n",
    ),
    (
        "prefix chain",
        "q = users & prefix \"u_\" & suffix \"_v2\"\n",
    ),
    ("omit", "q = users & omit \"age\"\n"),
    ("mapValue", "q = users & mapValue \"nullable\"\n"),
    ("distinct", "q = users & distinct\n"),
    ("update", "q = users & update { older = .age + 1 }\n"),
    (
        "helper with two stages",
        "two = p => q => q & prefix p & prefix p\nq = users & two \"x_\"\n",
    ),
    (
        "helper with a wrapper parameter",
        "w = m => q => q & mapValue m\nq = users & w \"nullable\"\n",
    ),
    ("recursive query", "a = b\nb = a\nq = a\n"),
    (
        "recursive scalar",
        "f = f\nq = users & select { x = f 1 }\n",
    ),
    (
        "unknown table",
        "t : query { id = int } = table \"s\" \"t\"\nq = t & where (.id > 1)\n",
    ),
    ("unknown column", "q = users & where (.nope > 1)\n"),
];

const TABLES: &str = "users : query { id = int, name = string, age = int, active = bool } = \
                      table \"public\" \"users\"\n\
                      orders : query { id = int, user_id = int, amount = float } = \
                      table \"public\" \"orders\"\n\
                      admins : query { id = int, name = string, age = int, active = bool } = \
                      table \"public\" \"admins\"\n";

/// One program's whole pipeline output, rendered.
fn render_program(label: &str, ws: &Workspace) -> String {
    let tc = check(ws);
    let input = CompilerInput::new(ws, &tc);
    let compiled = compile_input(input);
    let mut out = String::new();
    let root = ws.root;
    let _ = writeln!(
        out,
        "== {label} ({} modules, root {root})",
        ws.modules.len()
    );
    let mut diags: Vec<String> = compiled
        .diagnostics
        .iter()
        .map(|diag| format!("{}:{}:{} {}", diag.path, diag.line, diag.col, diag.message))
        .collect();
    diags.sort();
    for diag in diags {
        let _ = writeln!(out, "diagnostic {diag}");
    }
    // The root module's definition order is a fact about the program, so the
    // queries are rendered in it rather than sorted by name.
    for query in &compiled.queries {
        let _ = write!(
            out,
            "query {} def {} fault {:?}: ",
            query.name, query.id.def, query.fault
        );
        match &query.result {
            Ok(rel) => {
                let _ = writeln!(out, "{rel:?}");
            }
            Err(diag) => {
                let _ = writeln!(
                    out,
                    "ERR {}:{}:{} {}",
                    diag.path, diag.line, diag.col, diag.message
                );
            }
        }
    }
    // The checked layer's view of every root definition, for the definitions
    // `compile` reports no verdict for (a scalar helper has none) and for the
    // origin/kind of each node along the way.
    for (def, definition) in ws.modules[root].module.defs.iter().enumerate() {
        if !matches!(
            tc.scheme_view(root, def),
            Some(cagara_hir::check::SchemeView::Query { .. })
        ) {
            continue;
        }
        let _ = write!(out, "checked {} def {def}: ", definition.name);
        match cagara_hir::elaborate_definition_for_tests(input, root, def, definition) {
            Ok(checked) => {
                let _ = writeln!(out, "{}", describe(&checked));
            }
            Err(error) => {
                let _ = writeln!(out, "ERR {error:?}");
            }
        }
    }
    out
}

/// A checked query's shape with its origins, without the type information the
/// erasure already carries.
fn describe(query: &CheckedQuery) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "{:?} origin {:?} row [{}]",
        query.kind(),
        query.origin(),
        query
            .row()
            .columns()
            .iter()
            .map(|(name, ty)| format!("{name}: {ty}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let erased: Result<_, _> = erase(query.clone());
    let _ = write!(out, " erase {erased:?}");
    out
}

fn render_inline(label: &str, body: &str) -> String {
    let ws = Workspace::from_source(&format!("{TABLES}{body}"));
    render_program(label, &ws)
}

fn render_sources() -> String {
    let mut out = String::new();
    let mut seen = 0usize;
    for path in sources() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let ws = Workspace::from_source(&text);
        let label = path
            .strip_prefix(repo_root())
            .unwrap_or(&path)
            .display()
            .to_string();
        out.push_str(&render_program(&label, &ws));
        seen += 1;
    }
    assert!(seen >= 10, "only {seen} source files were found");
    for (label, body) in INLINE_PROGRAMS {
        out.push_str(&render_inline(label, body));
    }
    out
}

fn snapshot_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/pipeline_golden.txt")
}

/// The pipeline's output matches the checked-in snapshot.
///
/// `UPDATE_SNAPSHOTS=1` rewrites it. That is the only way to accept a change,
/// and a change accepted that way must be explained in the commit that does it:
/// the whole point is that a refactor which claims to preserve behaviour gets
/// this file byte-for-byte.
#[test]
fn the_pipeline_output_matches_its_snapshot() {
    let rendered = render_sources();
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        let path = snapshot_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("snapshot directory");
        }
        std::fs::write(&path, &rendered).expect("write snapshot");
        return;
    }
    let expected = std::fs::read_to_string(snapshot_path()).unwrap_or_else(|error| {
        panic!(
            "no pipeline snapshot at {}: {error}; run with UPDATE_SNAPSHOTS=1 once to record it",
            snapshot_path().display()
        )
    });
    if expected == rendered {
        return;
    }
    // Show the first differing program rather than 4000 lines of dump.
    let mut line = 1usize;
    let mut expected_lines = expected.lines();
    let mut rendered_lines = rendered.lines();
    loop {
        match (expected_lines.next(), rendered_lines.next()) {
            (Some(left), Some(right)) if left == right => line += 1,
            (left, right) => {
                panic!(
                    "the pipeline output changed at line {line} of {}.\n\
                     recorded: {left:?}\n\
                     produced: {right:?}\n\
                     If the change is intended, re-record with UPDATE_SNAPSHOTS=1 and \
                     explain it where it happens.",
                    snapshot_path().display()
                );
            }
        }
    }
}
