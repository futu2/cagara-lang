//! The language guide's operator claims, checked against the prelude.
//!
//! `docs/LEARN.md` teaches the stage shorthands with a table, and every
//! shorthand in it is a claim about `prelude.cagara`: that the symbol is
//! declared, and that it means the stage the table names. Both halves were
//! wrong at once and nothing noticed.
//!
//! `&^` was documented as `offset` for several releases. It had been accurate
//! when written — `offset` then took a record of fields — and was left behind
//! when the symbol was rebound to `intersect`. A reader copying the guide's own
//! worked example got a type error on a symbol the guide had just taught them.
//!
//! The table was also **incomplete**, listing the six stage shorthands and
//! omitting the four set operations, which is what let the wrong entry survive
//! review: someone reading the table had no way to notice a symbol was missing,
//! and therefore no reason to doubt the symbol that was there.
//!
//! So this test parses both sides — the table and the prelude — and requires
//! them to agree exactly. It is deliberately a *set* comparison in both
//! directions:
//!
//! * a shorthand the prelude declares and the table omits fails, because an
//!   incomplete table is how a wrong entry hides;
//! * a table row whose meaning differs from the prelude's binding fails, which
//!   is the `&^` bug directly;
//! * a table row for a symbol the prelude does not declare fails.
//!
//! A hand-written expectation would not have caught any of these, because
//! writing the table out again is exactly how the original error was introduced.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The stage shorthands `docs/LEARN.md` documents, as `symbol -> stage`.
///
/// Reads the markdown table between the "Shorthand" header and the paragraph
/// that ends it. `\|` is unescaped because that is how a pipe is written inside
/// a markdown cell.
fn documented() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(repo_root().join("docs/LEARN.md")).expect("docs/LEARN.md");
    let mut out = BTreeMap::new();
    let mut in_table = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("| Shorthand") {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        if !t.starts_with('|') {
            break;
        }
        // Split on **unescaped** pipes only, then unescape. A pipe inside a
        // markdown cell is written `\|`, so removing the escapes before
        // splitting cuts the row in the wrong place: `| `&\|` | `union` |`
        // became three cells and the `&|` row was silently dropped, which is
        // the same class of quiet incompleteness this test exists to catch.
        let row = t.trim_matches('|');
        let mut cells: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut chars = row.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&'|') => {
                    cur.push('|');
                    chars.next();
                }
                '|' => {
                    cells.push(cur.trim().to_string());
                    cur.clear();
                }
                _ => cur.push(c),
            }
        }
        cells.push(cur.trim().to_string());
        // Skip the `|---|---|` separator.
        if cells.len() != 2 || cells[0].starts_with("---") {
            continue;
        }
        let symbol = cells[0].trim_matches('`').to_string();
        let stage = cells[1].trim_matches('`').to_string();
        if symbol.starts_with('&') {
            out.insert(symbol, stage);
        }
    }
    assert!(
        out.len() >= 10,
        "the shorthand table parsed as {} row(s), which means the parser is not \
         looking at the table any more: {out:?}",
        out.len()
    );
    out
}

/// The stage shorthands `prelude.cagara` declares, as `symbol -> stage`.
///
/// A declaration is `infixl 1 &X`; the definition is `_&X_ : ... = body`. The
/// stage is read from the **body's final call**, which is the only place it
/// appears:
///
/// ```text
/// _&-_ : query r -> expr r int -> query r = q => n => limit n q
///                                                      ^^^^^
/// _&?_ : query r -> expr r bool -> query r = q => pred => where pred q
///                                                        ^^^^^
/// ```
///
/// Two earlier attempts at this read the wrong thing and both were caught by the
/// floor below rather than by review. Reading the body's *last* identifier picked
/// up the parameter (`n` for `&-`); reading the *type annotation* picked up the
/// first type (`bool` for `&?`). Neither the annotation nor the parameter list
/// names the stage.
///
/// The search is for a line that *starts* with the definition name, because
/// `prelude.cagara` also contains a commented-out example of exactly this form —
/// a plain `find` over the text matches the comment first, which is how the
/// `&^` row was read as `...` on the first attempt.
fn declared() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(repo_root().join("prelude.cagara")).expect("prelude.cagara");
    let mut symbols: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        // `infixl 1 &|` — a declaration. Bare `&` is the pipe, not a shorthand.
        if let Some(rest) = t.strip_prefix("infixl 1 &") {
            let sym = rest.trim();
            if !sym.is_empty() && !sym.contains(char::is_whitespace) {
                symbols.push(format!("&{sym}"));
            }
        }
    }

    let mut out = BTreeMap::new();
    for sym in &symbols {
        let needle = format!("_&{}_", &sym[1..]);
        let Some(line) = text.lines().find(|l| l.starts_with(&needle)) else {
            continue;
        };
        let Some((_, body)) = line.split_once('=') else {
            continue;
        };
        // The callee of the final application, after the last `=>`.
        let after = body.rsplit("=>").next().unwrap_or(body).trim();
        let stage = after.split_whitespace().next().unwrap_or("");
        if !stage.is_empty() {
            out.insert(sym.clone(), stage.to_string());
        }
    }
    assert!(
        out.len() >= 10,
        "the prelude parsed as {} shorthand(s), which means the parser is not \
         looking at the declarations any more: {out:?}",
        out.len()
    );
    out
}

/// Every shorthand the guide lists means what the prelude says it means.
#[test]
fn the_guide_shorthand_table_matches_the_prelude() {
    let doc = documented();
    let prelude = declared();

    for (sym, stage) in &doc {
        let Some(actual) = prelude.get(sym) else {
            panic!(
                "docs/LEARN.md documents `{sym}` as `{stage}`, but `prelude.cagara` \
                 does not declare `{sym}`. Either declare it or remove the row — a \
                 row for an undeclared symbol is what `&~` was when the guide used \
                 it as its example of an unknown operator."
            );
        };
        assert_eq!(
            actual, stage,
            "docs/LEARN.md documents `{sym}` as `{stage}`, but the prelude defines \
             it as `{actual}`. This is the `&^` bug: the symbol was documented as \
             `offset` while the prelude bound it to `intersect`, so a reader \
             copying the guide got a type error."
        );
    }
}

/// The guide's table is not missing any shorthand the prelude declares.
///
/// The completeness half, and the one that would have caught the `&^` bug
/// sooner: an incomplete table gives a reader no reason to doubt the rows it
/// does have.
#[test]
fn the_guide_lists_every_declared_shorthand() {
    let doc = documented();
    let prelude = declared();

    let missing: Vec<&String> = prelude.keys().filter(|s| !doc.contains_key(*s)).collect();
    assert!(
        missing.is_empty(),
        "prelude.cagara declares {missing:?} but docs/LEARN.md does not list them. \
         An incomplete table is how a wrong entry hides — the guide omitted the \
         four set operations while documenting `&^` as the wrong stage."
    );
}
