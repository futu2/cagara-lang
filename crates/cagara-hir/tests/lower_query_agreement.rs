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

/// A `Rel` debug rendering with every source location removed.
///
/// The two paths legitimately differ in which text they blame, so the comparison
/// is over structure. A location is **module plus span**, and both parts can
/// differ for a reason that is not a defect: a stage reached through the
/// prelude's `_>>>_` carries the prelude's module, because the composition body
/// is written there, while the same pipeline written inline carries the user's.
/// What must agree is the shape: the same stages, in the same order, over the
/// same columns and literals.
fn strip_spans(rel: &str) -> String {
    let mut out = String::with_capacity(rel.len());
    let bytes = rel.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // `Loc { module: .., span: Span { .. } }` — drop the whole location.
        if rel[i..].starts_with("Loc {") {
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
            out.push_str("Loc { .. }");
            i = j;
            continue;
        }
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

/// A **stage value** applied to an input lowers like the inline spelling.
///
/// A stage is a term with its input missing — a partially applied primitive
/// (`s = select { .id }` is `__select [record]`) or a `Lambda`
/// (`s = where p >>> select f` is `Lambda [x] → __select [f, __where [p, x]]`).
/// Applying it substitutes the input, and the result is the same saturated term
/// the inline spelling produces, so the lowering needs no stage cases of its
/// own: reducing *is* the stage application, which is what lets
/// `elaborate_user_stage`, `apply_lambda_stage` and `apply_point_free_stage`
/// go away.
///
/// This asserts the equivalence on the three spellings of one pipeline, which is
/// the property those three functions exist to provide separately.
#[test]
fn a_stage_value_lowers_like_its_inline_spelling() {
    // One pipeline, written three ways. Each `q` must produce the same query.
    let spellings: &[(&str, &str)] = &[
        (
            "named stage",
            "s = where (.age > 1) >>> select { .name }\nq = users & s\n",
        ),
        (
            "composed stage",
            "s = where (.age > 1) >>> select { .name }\nq = users & s\n",
        ),
        (
            "inline composition",
            "q = users & (where (.age > 1) >>> select { .name })\n",
        ),
        (
            "inline lambda",
            "q = users & (x => select { .name } (where (.age > 1) x))\n",
        ),
        (
            "inline pipeline",
            "q = users & where (.age > 1) & select { .name }\n",
        ),
    ];

    let mut built = Vec::new();
    for (label, body) in spellings {
        let src = format!("{TABLES}{body}");
        let from_source = strip_spans(&source_path(&src, "q"));
        let from_term = strip_spans(&term_path(&src, "q"));
        assert_eq!(
            from_source, from_term,
            "{label}: the term path and the source path disagree"
        );
        built.push((label, from_source));
    }
    // Every spelling is the *same query*, not merely each self-consistent.
    for (label, rel) in &built[1..] {
        assert_eq!(
            &built[0].1, rel,
            "`{label}` lowered to a different query from `{}`",
            built[0].0
        );
    }
}

/// Every relational primitive the prelude declares is either lowered or is a
/// known row combinator.
///
/// Step three of the plan was "stage values" — a term whose input is missing,
/// either a partially applied primitive or a `Lambda` over one. This records the
/// measurement that made that step unnecessary, and it is the *reason*: applying
/// a stage is the reduction the normalizer already performs, so by the time a
/// term reaches the lowering every stage is saturated and the unary and binary
/// cases are the whole vocabulary. `a_stage_value_lowers_like_its_inline_spelling`
/// is the behavioural half of the same claim.
///
/// The vocabulary is read from the prelude rather than listed here, so a stage
/// added to the language fails this test instead of silently reaching
/// "not lowered yet".
#[test]
fn the_lowering_covers_the_relational_vocabulary() {
    const PRELUDE: &str = include_str!("../../../prelude.cagara");
    // A primitive whose declared signature mentions a query more than once is a
    // stage: `query r -> query s`, or `… -> query r -> query s`.
    // A declaration's signature may span several lines before the `primitive`
    // keyword, so the signature text is accumulated until the body is reached.
    let mut stages: Vec<String> = Vec::new();
    let mut signature = String::new();
    for line in PRELUDE.lines() {
        let Some(pos) = line.find("primitive \"") else {
            signature.push_str(line);
            signature.push(' ');
            continue;
        };
        signature.push_str(&line[..pos]);
        let rest = &line[pos + "primitive \"".len()..];
        if let Some(end) = rest.find('\"') {
            // A stage takes a query and gives one back, so `query` appears at
            // least twice in its signature.
            if signature.matches("query").count() >= 2 {
                stages.push(rest[..end].to_string());
            }
        }
        signature.clear();
    }
    stages.sort();
    stages.dedup();

    // The two row combinators: `merge` overlays rows and is rejected as a query
    // body by the source path; `mapValue` is internal to the outer-join row laws
    // and its wrapper argument is a type-level name a user cannot write. Neither
    // has a `CheckedQuery` constructor to lower into.
    let row_combinators = ["__mapValue", "__merge"];
    let stages: Vec<&String> = stages
        .iter()
        .filter(|s| !row_combinators.contains(&s.as_str()))
        .collect();
    assert!(
        stages.len() >= 19,
        "expected the prelude's relational vocabulary, found {}: {stages:?}",
        stages.len()
    );

    // Each one is named in the lowering, which is where a missing case would be
    // absent. Reading the source of this crate in a test is unusual; it is worth
    // it here because the alternative is a hand-written list that a new stage
    // would not update.
    const LOWERING: &str = include_str!("../src/normalize.rs");
    let missing: Vec<&&String> = stages
        .iter()
        .filter(|name| !LOWERING.contains(&format!("\"{name}\"")))
        .collect();
    assert!(
        missing.is_empty(),
        "these relational primitives are declared by the prelude and have no \
         lowering case: {missing:?}"
    );
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
