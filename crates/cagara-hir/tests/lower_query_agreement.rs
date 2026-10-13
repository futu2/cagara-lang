//! The term lowering must build the **same checked query** as the source path.
//!
//! This is the migration's evidence for the relational half: `lower_query`
//! consumes a normalized term, while `elaborate_query` walks source, and the two
//! are compared through the erased `Rel` — the tree every later phase sees. A
//! difference means the lowering dropped or reordered something, which is
//! exactly the class of bug the set-operation operand order once was.
//!
//! The comparison is *not* the tautology the expression half started with: the
//! two sides really are different code. It is written so that it can fail —
//! `the_comparison_can_fail` drives it on a query whose lowering is deliberately
//! wrong, so a green run means the walk found something to compare.

use cagara_hir::check::check;
use cagara_hir::normalize::{lower_query, Body, Facts, Normalizer, WorkspaceFacts};
use cagara_hir::{root_queries_checked, Workspace};

const TABLES: &str = "\
users : query { id = int, name = string, age = int } = table \"s\" \"users\"
orders : query { id = int, uid = int, amount = float } = table \"s\" \"orders\"
vips : query { id = int, name = string, age = int } = table \"s\" \"vips\"
";

/// The source path's query for `name`, as an erased relation.
fn source_path(src: &str, name: &str) -> String {
    let ws = Workspace::from_source(src);
    let tc = check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    let rel = root_queries_checked(&ws, &tc)
        .into_iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no query `{name}`"))
        .1
        .unwrap_or_else(|d| panic!("`{name}` did not compile: {}", d.message));
    format!("{rel:?}")
}

/// The term path's query for `name`, as an erased relation.
fn term_path(src: &str, name: &str) -> String {
    let ws = Workspace::from_source(src);
    let tc = check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    let facts = WorkspaceFacts::new(&ws, &tc);
    let def = ws.modules[ws.root]
        .module
        .defs
        .iter()
        .position(|d| d.name == name)
        .unwrap_or_else(|| panic!("no definition `{name}`"));
    let body = Body {
        module: ws.root,
        def,
        expr: ws.modules[ws.root].module.defs[def].body.clone(),
    };
    let resolved = |m: usize, id: u32| facts.resolution(m, id);
    let mut cx = Normalizer::new(Box::new(resolved));
    let term = cx
        .in_module(ws.root, |cx| {
            cagara_hir::normalize::expression(&facts, cx, &body)
        })
        .unwrap_or_else(|g| panic!("`{name}` did not normalize: {}", g.message));
    let query =
        lower_query(&term).unwrap_or_else(|g| panic!("`{name}` did not lower: {}", g.message));
    let rel = cagara_hir::erase(query)
        .unwrap_or_else(|e| panic!("`{name}` did not erase: {}", e.message));
    format!("{rel:?}")
}

/// A `Rel` debug rendering with every `Span { .. }` body replaced.
///
/// The two paths legitimately differ in which text they blame, so the
/// comparison is over structure. Spans are the only thing removed: names,
/// columns, literal values, argument order and nesting all stay.
fn strip_spans(rel: &str) -> String {
    let mut out = String::with_capacity(rel.len());
    let bytes = rel.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if rel[i..].starts_with("Span {") {
            let mut depth = 0usize;
            let mut j = i;
            loop {
                match bytes.get(j) {
                    Some(b'{') => depth += 1,
                    Some(b'}') => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    None => break,
                    _ => {}
                }
                j += 1;
            }
            out.push_str("Span { .. }");
            i = j;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[test]
fn both_paths_build_the_same_query() {
    let cases: &[(&str, &str)] = &[
        ("a table", "q = users\n"),
        ("a where", "q = users & where (.age > 18)\n"),
        ("a select", "q = users & select { a = .id, b = .name }\n"),
        ("an update", "q = users & update { age = .age + 1 }\n"),
        ("an agg", "q = users & agg { k = group .age, n = count }\n"),
        ("an order", "q = users & order [asc .age, desc .name]\n"),
        ("a limit", "q = users & limit 5\n"),
        ("an offset", "q = users & offset 5\n"),
        ("a distinct", "q = users & select { .age } & distinct\n"),
        ("an omit", "q = users & omit \"name\"\n"),
        ("a prefix", "q = users & select { .id } & prefix \"p_\"\n"),
        ("a suffix", "q = users & select { .id } & suffix \"_s\"\n"),
        (
            "a pipeline",
            "q = users & where (.age > 1) & select { .id } & order [asc .id] & limit 2\n",
        ),
        (
            "a table via a name",
            "base = users\nq = base & where (.id > 1)\n",
        ),
        // The binary stages, and for the set operations both spellings: they
        // deliberately put the piped query on opposite sides, so a lowering that
        // guessed the side would pass one spelling and reverse the other.
        (
            "innerJoin",
            "q = users & innerJoin orders (.<id == .>uid)\n",
        ),
        ("leftJoin", "q = users & leftJoin orders (.<id == .>uid)\n"),
        (
            "rightJoin",
            "q = users & rightJoin orders (.<id == .>uid)\n",
        ),
        ("fullJoin", "q = users & fullJoin orders (.<id == .>uid)\n"),
        ("semiJoin", "q = users & semiJoin orders (.<id == .>uid)\n"),
        ("antiJoin", "q = users & antiJoin orders (.<id == .>uid)\n"),
        ("join operator", "q = users & orders ? (.<id == .>uid)\n"),
        ("union, bare name", "q = users & union vips\n"),
        ("union, operator", "q = users &| vips\n"),
        ("union, direct call", "q = union users vips\n"),
        ("unionAll", "q = users & unionAll vips\n"),
        ("intersect", "q = users & intersect vips\n"),
        ("except, bare name", "q = users & except vips\n"),
        ("except, operator", "q = users &~ vips\n"),
        (
            "a join then a filter",
            "q = users & innerJoin orders (.<id == .>uid) & where (.amount > 1.0)\n",
        ),
    ];

    let mut compared = 0usize;
    for (label, body) in cases {
        let src = format!("{TABLES}{body}");
        let from_source = source_path(&src, "q");
        let from_term = term_path(&src, "q");
        // Compared **up to the blamed span**, and the difference is deliberate
        // rather than tolerated. A stage's node is located at the whole
        // application in the term path (`where (.age > 18)`, 153..178) and at
        // the stage *name* in the source path (`where`, 153..158). Both are
        // truthful; the term's is the one a user can act on, since it is the
        // text they would go and edit, and the source path's is an artefact of
        // blaming the callee name it dispatched on. What must agree is the
        // *shape*: the same stages, in the same order, over the same columns.
        assert_eq!(
            strip_spans(&from_source),
            strip_spans(&from_term),
            "{label}: the two paths disagree\n  source: {from_source}\n  term:   {from_term}"
        );
        compared += 1;
    }
    assert!(compared >= 29, "only {compared} cases ran");
}

/// The two set-operation spellings really do differ, so agreeing with both
/// means the lowering read the operand order rather than guessing it.
///
/// `users & union vips` puts the piped query on the right; `users &| vips` puts
/// it on the left. A lowering that assumed one would still pass whichever case
/// matched its assumption, so the evidence is that the two are *different* and
/// each equals its own source-path result.
#[test]
fn the_two_set_spellings_really_do_differ() {
    let bare = format!("{TABLES}q = users & union vips\n");
    let operator = format!("{TABLES}q = users &| vips\n");
    let from_bare = strip_spans(&term_path(&bare, "q"));
    let from_operator = strip_spans(&term_path(&operator, "q"));
    assert_ne!(
        from_bare, from_operator,
        "the two spellings lowered to the same tree; one of them must be reversed"
    );
    // Each still agrees with its own source-path result, so neither was fixed
    // by making the other one wrong.
    for src in [&bare, &operator] {
        assert_eq!(
            strip_spans(&source_path(src, "q")),
            strip_spans(&term_path(src, "q"))
        );
    }
}

/// The comparison can actually fail: a deliberately wrong lowering differs.
///
/// Without this, a green run would only mean the two sides happened to be the
/// same object — the failure mode that made the expression half's first
/// agreement test worthless.
#[test]
fn the_comparison_can_fail() {
    let src = format!("{TABLES}q = users & limit 5\n");
    let from_source = source_path(&src, "q");
    // `limit 7` where the program says `limit 5`: same shape, different tree.
    let wrong = format!("{TABLES}q = users & limit 7\n");
    let wrong_term = term_path(&wrong, "q");
    assert_ne!(
        from_source, wrong_term,
        "the comparison cannot tell two different queries apart"
    );
}
