//! The language guide's operator claims, checked against the compiler.
//!
//! `docs/LEARN.md` teaches the stage shorthands with a table, and every
//! shorthand in it is a claim about two things: that the symbol is in the
//! compiler's fixed operator table, and that the prelude defines it as the
//! stage the table names. Both halves were wrong at once and nothing noticed.
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
//! So this test parses both sides — the fixed table and the prelude — and
//! requires them to agree exactly. It is deliberately a *set* comparison in
//! both directions:
//!
//! * a shorthand in the fixed table that the guide omits fails, because an
//!   incomplete table is how a wrong entry hides;
//! * a guide row whose meaning differs from the prelude's binding fails, which
//!   is the `&^` bug directly;
//! * a guide row for a symbol the fixed table does not contain fails.
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

/// The stage shorthands in the fixed table and their prelude meanings, as
/// `symbol -> stage`.
///
/// The symbol comes from `ops.rs`; the definition in the prelude is
/// `_&X_ : ... = body`. The stage is read from the **body's final call**, which
/// is the only place it appears:
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
/// the prelude also contains a commented-out example of exactly this form —
/// a plain `find` over the text matches the comment first, which is how the
/// `&^` row was read as `...` on the first attempt.
///
/// The shorthands the language has, and what each means.
///
/// The **symbols** come from the core's fixed table, which is the only place a
/// shorthand's spelling lives. The **meaning** comes from the prelude, as
/// the `_&^_` definition — spelling and meaning are two different facts and
/// live in two places.
fn shorthands() -> BTreeMap<String, String> {
    let text = ["prelude/core.cagara", "prelude/std.cagara"]
        .iter()
        .map(|p| std::fs::read_to_string(repo_root().join(p)).expect(p))
        .collect::<String>();
    let ops = std::fs::read_to_string(repo_root().join("crates/cagara-syntax/src/ops.rs"))
        .expect("ops.rs");
    let mut symbols: Vec<String> = Vec::new();
    for line in ops.lines() {
        // `("&|", Fixity::left(1)),` — a shorthand. Bare `&` is the pipe, and
        // `&&` is logic rather than a shorthand, so the pipe level is what
        // tells them apart: only shorthands sit at `&`'s own level.
        let t = line.trim();
        let Some(rest) = t.strip_prefix("(\"") else {
            continue;
        };
        let Some((sym, fixity)) = rest.split_once('"') else {
            continue;
        };
        if sym.len() > 1 && sym.starts_with('&') && fixity.contains("left(1)") {
            symbols.push(sym.to_string());
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
    out
}

/// The guide's operator table and the language's fixed table must agree.
#[test]
fn the_guide_teaches_every_operator_the_language_has() {
    // The guide's operator table is prose, and prose drifts. Compare it with
    // the shipped table in both directions: a shorthand the language has but
    // the guide never mentions is a missing page, and a shorthand the guide
    // teaches but the parser does not have is a lie. Both are quiet failures
    // without this test — the first leaves a useful spelling undocumented, the
    // second makes every copied example a syntax error.
    let guide = documented();
    let table = shorthands();
    for (op, named) in &table {
        match guide.get(op) {
            Some(taught) => assert_eq!(
                taught, named,
                "the guide pairs `{op}` with `{taught}`, the table with `{named}`"
            ),
            None => panic!("`{op}` is in the table but the guide never mentions it"),
        }
    }
    for op in guide.keys() {
        assert!(
            table.contains_key(op),
            "the guide teaches `{op}`, which the parser does not have"
        );
    }
}
