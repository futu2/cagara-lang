//! Formatting must not change what a program means, and must not refuse a good one.
//!
//! `cagara-fmt` has its own guarantees and they are about *text*: the output parses
//! to the same skeleton, every comment is still there, formatting twice changes
//! nothing. Those are the primary checks and they live beside the formatter. This
//! file asks the question a user asks — is the SQL the same — which needs the whole
//! pipeline and so can only be asked here.
//!
//! # What this is worth, measured
//!
//! `verify_shape` compares every node kind *and every token's text* between the
//! input and a re-parse of the output, so a working guard implies the program is
//! unchanged and the meaning check below is a second line of defence rather than the
//! first. That was checked rather than assumed: removing the parentheses a nested
//! expression prints with — which re-associates `(a - b) - c` into `a - b - c` — made
//! the formatter *refuse* the file (`the output would change the program`) instead of
//! emitting a wrong one.
//!
//! So the two tests here cover different failures. If the guard breaks, the meaning
//! test is what notices. If the guard becomes too strict, or the printer starts
//! refusing shapes it used to print, the meaning test still passes — every program
//! would be refused, and a refusal is not a wrong answer. That is
//! `formatting_succeeds_for_every_shipped_program`, and the mutation made it fail
//! first, which is how the gap in the first version of this file was found: it had
//! only the meaning check, and a mass refusal tripped a threshold inside it instead
//! of being reported as "the formatter refuses good files".

use cagara_hir::{check, root_queries_checked, root_verdicts, Fault, TypeCheck, Workspace};
use cagara_sql::{compile, Dialect, Options};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every shipped program: the examples the README links to, and the corpus cases.
///
/// Read from their real location rather than copied here, for the reason the corpus
/// test gives: a copy drifts from the file a user actually reads.
fn every_program() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in [
        repo_root().join("examples"),
        repo_root().join("crates/cagara-hir/tests/corpus"),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "cagara"))
            .collect();
        found.sort();
        files.extend(found);
    }
    files
}

/// What a definition means: whether it was accepted, its type, and its SQL.
///
/// The type is part of the meaning, not decoration — `select { x = 1 }` and
/// `select { x = 1.0 }` differ only in an inferred type. Every dialect is included
/// so a change that only shows up in one target's spelling is still caught.
fn meaning(text: &str) -> Vec<String> {
    let ws = Workspace::from_source(text);
    let tc: TypeCheck = check(&ws);
    let mut out = Vec::new();
    for (name, _rel, fault) in root_verdicts(&ws, &tc) {
        let index = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == name);
        let kind = match fault {
            Fault::Program => "rejected",
            Fault::Compiler => "COMPILER-GAP",
            Fault::NotAQuery => "not-a-query",
        };
        out.push(format!("{name}: {kind}"));
        if let Some(i) = index {
            if let Some(e) = tc.error_for(ws.root, i) {
                out.push(format!("{name}: type-error: {}", e.message));
            } else if let Some(t) = tc.type_of(ws.root, i) {
                out.push(format!("{name}: {t}"));
            }
        }
    }
    for (name, rel) in root_queries_checked(&ws, &tc) {
        match rel {
            Ok(rel) => {
                for d in [
                    Dialect::DuckDb,
                    Dialect::Postgres,
                    Dialect::Sqlite,
                    Dialect::Mysql,
                    Dialect::Trino,
                ] {
                    let sql = compile(
                        &rel,
                        Options {
                            dialect: d,
                            pretty: false,
                            optimize: false,
                        },
                    )
                    .map(|s| s.trim_end_matches(';').to_string())
                    .unwrap_or_else(|e| format!("<error: {e}>"));
                    out.push(format!("{name} [{d:?}]: {sql}"));
                }
            }
            Err(d) => out.push(format!("{name}: <error: {}>", d.message)),
        }
    }
    out
}

/// Every shipped program formats, and none of them is refused.
///
/// A formatter that refuses a valid file is broken in the way users notice first:
/// format-on-save does nothing, `cagara fmt --check` fails on a file nobody can fix,
/// and there is no message that suggests what to do about it. The corpus is the set
/// of files this repository promises to format, so it is the right set to assert on.
#[test]
fn formatting_succeeds_for_every_shipped_program() {
    let files = every_program();
    assert!(
        files.len() >= 10,
        "the corpus went missing: {} files",
        files.len()
    );
    let mut refused = Vec::new();
    for path in &files {
        let src = std::fs::read_to_string(path).expect("a corpus file");
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if let Err(e) = cagara_fmt::format(&src) {
            refused.push(format!("{name}: {e}"));
        }
    }
    assert!(
        refused.is_empty(),
        "refused {} file(s): {refused:#?}",
        refused.len()
    );
}

/// Formatting a program does not change what it means.
///
/// Reported as a diff of the two meanings rather than a bare `assert_eq`, because a
/// failure here is a formatter bug and the reviewer needs to see which definition
/// changed and how.
#[test]
fn formatting_preserves_every_program_in_the_corpus() {
    for path in &every_program() {
        let src = std::fs::read_to_string(path).expect("a corpus file");
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let Ok(formatted) = cagara_fmt::format(&src) else {
            // `examples/errors.cagara` does not parse; `formatting_succeeds_for_every_shipped_program`
            // is where a refusal of a *valid* file is caught.
            continue;
        };
        let before = meaning(&src);
        let after = meaning(&formatted.text);
        if before != after {
            let mut report = format!("{name}: formatting changed the meaning\n");
            for (a, b) in before.iter().zip(&after) {
                if a != b {
                    report.push_str(&format!("  before: {a}\n  after : {b}\n"));
                }
            }
            report.push_str(&format!("\n--- formatted ---\n{}", formatted.text));
            panic!("{report}");
        }
        // And the text is a fixed point, which the formatter also promises; asserted
        // here so one test covers "formatting is safe" rather than two half of it.
        let again = cagara_fmt::format(&formatted.text)
            .unwrap_or_else(|e| panic!("{name}: its own output does not format: {e}"))
            .text;
        assert_eq!(again, formatted.text, "{name}: not idempotent");
    }
}

/// The same, for text a formatter is most likely to get wrong.
///
/// These are the shapes that break real formatters: a comment longer than the line
/// width (it cannot be broken, so it must be left alone), a string that cannot be
/// broken (same), nesting deeper than the width allows (every level wants to break,
/// and the indentation has to stop growing or each level makes the line worse), and
/// an operator chain that must not be re-associated. None of them is in the corpus,
/// which is written the way its author wanted to read it.
#[test]
fn formatting_preserves_the_awkward_shapes() {
    const TABLE: &str = "t : query { a = int, b = int, s = string } = table \"m\" \"t\"\n";
    let cases: &[(&str, &str)] = &[
        // A comment that cannot be broken, on a definition that must break.
        (
            "long comment",
            "q = t & where (.a > 1) & select { a = .a, b = .b } # this comment is far longer than the configured width of one hundred columns and cannot be broken\n",
        ),
        // A string literal that cannot be broken, on the same shape.
        (
            "long string",
            "q = t & where (.a > 1) & select { x = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" }\n",
        ),
        // Nesting deeper than the width allows: indentation must not grow forever.
        (
            "deep nesting",
            "q = t & where ((((((((.a > 1) && (.b > 2)) || (.a > 3)) && (.b > 4)) || (.a > 5)) && (.b > 6)) || (.a > 7)) && (.b > 8)) & select { .a }\n",
        ),
        // A subtraction chain: re-associating it changes the answer.
        (
            "subtraction",
            "q = t & select { x = .a - .b - 1 - 2 - 3 - 4 - 5 - 6 - 7 - 8 - 9 - 10 - 11 - 12 - 13 - 14 }\n",
        ),
        // A pipeline whose stages are all long enough to break.
        (
            "long stages",
            "q = t & where (.a > 1 && .b > 2 && .s != \"x\") & select { a = .a, b = .b, s = .s } & order [asc .a, desc .b] & limit 10\n",
        ),
    ];
    for (what, body) in cases {
        let src = format!("{TABLE}{body}");
        let formatted = cagara_fmt::format(&src)
            .unwrap_or_else(|e| panic!("{what}: does not format: {e}"))
            .text;
        let before = meaning(&src);
        let after = meaning(&formatted);
        assert_eq!(before, after, "{what}: meaning changed\n{formatted}");
        // Every line inside the width, except the two things that cannot be broken
        // down: a token longer than the width, and a trailing comment. A comment is
        // one lexeme from `#` to the end of the line, so a long one *must* overflow —
        // the alternative is rewriting what the author wrote.
        for line in formatted.lines() {
            let over = line.chars().count() > cagara_fmt::WIDTH;
            let unbreakable = line.split_whitespace().count() <= 1 || line.contains('#');
            assert!(
                !over || unbreakable,
                "{what}: a breakable line is over the width ({} chars): {line:?}",
                line.chars().count()
            );
        }
        // And it is a fixed point.
        let again = cagara_fmt::format(&formatted).expect("idempotent").text;
        assert_eq!(again, formatted, "{what}: not idempotent");
    }
}
