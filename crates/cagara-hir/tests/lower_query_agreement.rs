//! Compare the production normalizer with the independent term oracle across
//! every relational shape. The test covers stages, joins, set operations,
//! windows, and stage-value spellings; the SQL corpus tests cover emitted
//! behavior. `the_comparison_can_fail` ensures the comparison is substantive.

use cagara_hir::check::check;
use cagara_hir::term_oracle::{lower_query, Body, Facts, Normalizer, WorkspaceFacts};
use cagara_hir::{root_queries_checked, Workspace};

const TABLES: &str = "\
users : query { id = int, name = string, age = int } = table \"s\" \"users\"
orders : query { id = int, uid = int, amount = float } = table \"s\" \"orders\"
vips : query { id = int, name = string, age = int } = table \"s\" \"vips\"
";

/// The compiler's query for `name`, as an erased relation.
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

/// The same query built through the independent term oracle and lowerer.
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
        expr: &ws.modules[ws.root].module.defs[def].body,
    };
    let resolved = |m: usize, id: u32| facts.resolution(m, id);
    let mut cx = Normalizer::new(Box::new(resolved));
    let term = cx
        .in_module(ws.root, |cx| {
            cagara_hir::term_oracle::expression(&facts, cx, &body)
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
        // Windows. These are the one construct that reaches the lowering from
        // *inside* a stage's field rather than as a stage of its own, so they
        // exercise the spec hook: the term carries the spec's structure and
        // `lower_winspec` decides what a partition, an order key and a frame
        // bound mean.
        (
            "a window with an order",
            "q = users & select { r = rowNumber {order = [asc .age]} }\n",
        ),
        (
            "a window with a partition",
            "q = orders & select { s = sumOver {partition = [.uid]} .amount }\n",
        ),
        (
            "a window with a full spec",
            "q = orders & select { s = sumOver {partition = [.uid], order = [asc .id], frame = runningFrame} .amount }\n",
        ),
        (
            "a window over an aggregate's output",
            "q = orders & agg { k = group .uid, n = count } & select { .k, r = rowNumber {order = [desc .n]} }\n",
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
    assert!(compared >= 33, "only {compared} cases ran");
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

/// Every relational stage the prelude declares has a lowering case.
///
/// Read from the prelude rather than listed here, so a stage added to the
/// language fails this test instead of silently reaching "not lowered yet".
///
/// The check is **behavioural**: each stage is written as a real program, that
/// program is normalized and lowered, and the failure message is inspected for
/// "not lowered yet". Two earlier versions of this were wrong in ways worth
/// recording, because both looked convincing:
///
/// * searching this crate's source for the primitive's name passes on a
///   *comment* — it would have gone green on a name mentioned only in prose;
/// * writing the primitive's own name as a program body normalizes nothing,
///   because `__agg` is not a name the checker knows — every case hit the
///   early `continue`, so the scan inspected zero stages and "nothing is
///   unlowered" was true because nothing was looked at. The floor below is what
///   caught that.
///
/// The two primitives left out are row combinators rather than query stages:
/// `__merge` overlays rows and is rejected as a query body, and `__mapValue` is
/// internal to the outer-join row laws, with a wrapper argument a user cannot
/// write. Neither has a `CheckedQuery` constructor to lower into.
#[test]
fn the_lowering_covers_the_relational_vocabulary() {
    const PRELUDE: &str = include_str!("../../../prelude.cagara");
    // A declaration's signature may span several lines before the `primitive`
    // keyword, so signature text is accumulated — and cleared at every blank
    // line as well as at every body. Without the blank-line reset a multi-line
    // signature bleeds into the next declaration, which is how `rows` (a
    // `bound -> bound -> frame`) was first mistaken for a stage.
    let mut stages: Vec<String> = Vec::new();
    let mut signature = String::new();
    for line in PRELUDE.lines() {
        if line.trim().is_empty() {
            signature.clear();
            continue;
        }
        let Some(pos) = line.find("primitive \"") else {
            // A comment belongs to no signature.
            if !line.trim_start().starts_with('#') {
                signature.push_str(line);
                signature.push(' ');
            }
            continue;
        };
        signature.push_str(&line[..pos]);
        let rest = &line[pos + "primitive \"".len()..];
        if let Some(end) = rest.find('\"') {
            // A stage takes a query and gives one back, so `query` appears at
            // least twice in its own signature.
            if signature.matches("query").count() >= 2 {
                stages.push(rest[..end].to_string());
            }
        }
        signature.clear();
    }
    stages.sort();
    stages.dedup();

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

    // Each stage, written the way the language spells it, applied to a query.
    // The spelling is the primitive's name without its leading underscores, and
    // the join operators take their arguments differently, so they are written
    // out.
    let program_for = |prim: &str| -> String {
        let tables = "users : query { id = int, name = string } = table \"s\" \"users\"\n\
                      other : query { id = int, name = string } = table \"s\" \"other\"\n";
        let body = match prim {
            "__table" => "q = users\n".to_string(),
            "__where" => "q = users & where (.id > 1)\n".to_string(),
            "__select" => "q = users & select { .id }\n".to_string(),
            "__update" => "q = users & update { n = .id }\n".to_string(),
            "__agg" => "q = users & agg { k = group .id, n = count }\n".to_string(),
            "__order" => "q = users & order [asc .id]\n".to_string(),
            "__limit" => "q = users & limit 1\n".to_string(),
            "__offset" => "q = users & offset 1\n".to_string(),
            "__distinct" => "q = users & distinct\n".to_string(),
            "__omit" => "q = users & omit \"name\"\n".to_string(),
            "__prefix" => "q = users & prefix \"p_\"\n".to_string(),
            "__suffix" => "q = users & suffix \"_s\"\n".to_string(),
            "__innerJoin" => "q = users & innerJoin other (.<id == .>id)\n".to_string(),
            "__leftJoin" => "q = users & leftJoin other (.<id == .>id)\n".to_string(),
            "__rightJoin" => "q = users & rightJoin other (.<id == .>id)\n".to_string(),
            "__fullJoin" => "q = users & fullJoin other (.<id == .>id)\n".to_string(),
            "__semiJoin" => "q = users & semiJoin other (.<id == .>id)\n".to_string(),
            "__antiJoin" => "q = users & antiJoin other (.<id == .>id)\n".to_string(),
            "__union" | "__unionAll" | "__intersect" | "__except" => {
                let op = prim.trim_start_matches("__");
                format!("q = users & {op} other\n")
            }
            other => panic!("no source spelling for `{other}`; add it to this test"),
        };
        format!("{tables}{body}")
    };

    let mut unlowered = Vec::new();
    let mut inspected = 0usize;
    for prim in &stages {
        let src = program_for(prim);
        let ws = Workspace::from_source(&src);
        let tc = check(&ws);
        assert!(
            tc.errors().is_empty(),
            "{prim}: the program this test writes is not legal, so it proves \
             nothing: {:?}",
            tc.errors()
        );
        let facts = WorkspaceFacts::new(&ws, &tc);
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .expect("`q`");
        let body = Body {
            module: ws.root,
            def,
            expr: &ws.modules[ws.root].module.defs[def].body,
        };
        let resolved = |m: usize, id: u32| facts.resolution(m, id);
        let mut cx = Normalizer::new(Box::new(resolved));
        inspected += 1;
        let term = cx
            .in_module(ws.root, |cx| {
                cagara_hir::term_oracle::expression(&facts, cx, &body)
            })
            .unwrap_or_else(|g| panic!("{prim} did not normalize: {}", g.message));
        if let Err(g) = lower_query(&term) {
            if g.message.contains("is not lowered yet") {
                unlowered.push(prim.to_string());
            }
        }
    }
    // A floor, so a change that stops the scan reaching any stage fails rather
    // than passing with nothing examined — which is exactly how the previous
    // version of this test was green.
    assert!(
        inspected >= 19,
        "only {inspected} of {} relational stages were inspected; the scan is not \
         looking where it thinks it is",
        stages.len()
    );
    assert!(
        unlowered.is_empty(),
        "these relational primitives are declared by the prelude and have no \
         lowering case: {unlowered:?}"
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
