use super::check;
use crate::workspace::Workspace;

const TABLES: &str = "users : query { id = int, name = string, age = int, active = bool } = table \"public\" \"users\"\n\
orders : query { id = int, user_id = int, amount = float, status = string } = table \"public\" \"orders\"\n";

/// Type of each root definition, or its error message.
fn types(src: &str) -> Vec<(String, Result<String, String>)> {
    let ws = Workspace::from_source(&format!("{TABLES}{src}"));
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let tc = check(&ws);
    for e in &tc.errors {
        assert_eq!(
            e.module, ws.root,
            "error outside the root module: {}",
            e.diag
        );
    }
    let m = ws.root;
    ws.modules[m]
        .module
        .defs
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let r = match tc.error_for(m, i) {
                Some(e) => Err(e.message.clone()),
                None => Ok(tc.type_of(m, i).unwrap_or("?").to_string()),
            };
            (d.name.clone(), r)
        })
        .collect()
}

fn ty(src: &str, name: &str) -> String {
    let r = types(src)
        .into_iter()
        .find(|(n, _)| n == name)
        .expect("no such definition")
        .1;
    r.unwrap_or_else(|e| panic!("`{name}` failed: {e}"))
}

fn err(src: &str, name: &str) -> String {
    match types(src)
        .into_iter()
        .find(|(n, _)| n == name)
        .expect("no such definition")
        .1
    {
        Ok(t) => panic!("`{name}` should fail but has type {t}"),
        Err(e) => e,
    }
}

#[test]
fn prelude_and_tables_check() {
    assert_eq!(
        ty("", "users"),
        "query { id = int, name = string, age = int, active = bool }"
    );
}

#[test]
fn row_polymorphic_predicate() {
    // The literal stays polymorphic until the column type is known.
    assert_eq!(
        ty("adult = .age >= 18\n", "adult"),
        "expr { age = a | b } bool"
    );
    assert_eq!(
        ty("named = .name == \"x\"\n", "named"),
        "expr { name = a | b } bool"
    );
}

#[test]
fn aggregate_types_print_wrapped() {
    assert_eq!(
        ty("total = sum .amount\n", "total"),
        "agg (expr { amount = a | b } (maybe a))"
    );
    assert_eq!(
        ty(
            "q = orders & agg { t = sum .amount, a = avg .user_id }\n",
            "q"
        ),
        "query { t = maybe float, a = maybe float }"
    );
}

#[test]
fn query_output_rows() {
    assert_eq!(
        ty("q = users & where .active & select { id = .id, label = upper .name, next = .age + 1 }\n", "q"),
        "query { id = int, label = string, next = int }"
    );
    assert_eq!(
        ty(
            "q = orders & agg { user_id = group .user_id, revenue = sum .amount, n = count }\n",
            "q"
        ),
        "query { user_id = int, revenue = maybe float, n = int }"
    );
    assert_eq!(
        ty("q = orders & inner users (.<user_id == .>id)\n", "q"),
        "query { id = int, user_id = int, amount = float, status = string, name = string, age = int, active = bool }"
    );
    // Later stages keep the query's column order.
    assert_eq!(
        ty("q = users & select { id = .id, label = upper .name } & order [asc .label] & where (.id > 1)\n", "q"),
        "query { id = int, label = string }"
    );
}

#[test]
fn constants_lift_and_widen() {
    assert_eq!(
        ty("q = orders & select { x = .amount * 2, one = 1 }\n", "q"),
        "query { x = float, one = int }"
    );
    assert_eq!(ty("q = users & where true\n", "q"), ty("", "users"));
}

#[test]
fn let_polymorphism_across_schemas() {
    let src = "people : query { age = int, city = string } = table \"p\" \"people\"\n\
               adult = .age >= 18\n\
               a = users & where adult\n\
               b = people & where adult\n";
    ty(src, "a");
    ty(src, "b");
}

#[test]
fn scalar_type_errors() {
    assert!(
        err("q = users & select { x = .name <> 1 }\n", "q").contains("expected string, found int")
    );
    assert!(err("q = users & limit .id\n", "q").contains("type mismatch"));
    let e = err("f = x => { a = x <> \"!\", b = x + 1 }\n", "f");
    assert!(
        e.contains("no overload of `+`"),
        "lambda parameters are monomorphic: {e}"
    );
}

#[test]
fn signatures_are_checked() {
    assert!(err("f : int -> string = x => x + 1\n", "f").contains("does not match its signature"));
    assert!(err("g : a -> int = x => x\n", "g").contains("signature"));
    ty("h : a -> a = x => x\n", "h");
    assert!(err("bad : agg (int) = count\n", "bad").contains("wraps an expression type"));
}

#[test]
fn schema_errors() {
    assert!(err("q = users & where (.salary > 1)\n", "q").contains("no column `salary`"));
    assert!(
        err("q = users & select { id = .id } & where (.age > 1)\n", "q")
            .contains("no column `age`")
    );
    assert!(err("q = users & where .name\n", "q").contains("bool"));
    assert!(err("q = users & where (.name >= 18)\n", "q").contains("expected string, found int"));
}

#[test]
fn phase_errors() {
    assert!(err("q = users & agg { t = sum (sum .age) }\n", "q").contains("cannot nest"));
    assert!(err("q = users & agg { n = count, name = .name }\n", "q").contains("not grouped"));
    assert!(err(
        "q = users & agg { x = coalesce 0 (sum .age) + .age }\n",
        "q"
    )
    .contains("ungrouped"));
    assert!(err("q = users & agg { x = .age + sum .age }\n", "q").contains("ungrouped"));
    assert!(err("q = users & where (count >= 1)\n", "q").contains("aggregate"));
    assert!(err("q = users & select { n = count }\n", "q").contains("belong in `agg`"));
    assert!(err(
        "q = users & where (rowNumber { order = [.id] } <= 3)\n",
        "q"
    )
    .contains("window"));
    assert!(err(
        "q = users & select { r = rowNumber { order = [sum .id] } }\n",
        "q"
    )
    .contains("plain column"));
}

#[test]
fn window_types() {
    let src = "spec = { partition = [.user_id], order = [desc .id] }\n\
               q = orders & select { id = .id, rn = rowNumber spec, prev = lag spec .amount }\n";
    assert_eq!(
        ty(src, "q"),
        "query { id = int, rn = int, prev = maybe float }"
    );
    assert!(err(
        "q = users & select { r = rowNumber { bogus = [.id] } }\n",
        "q"
    )
    .contains("unknown window spec field"));
}

#[test]
fn join_errors() {
    assert!(err("q = orders & inner users (.user_id == .id)\n", "q").contains("which input"));
    assert!(err("q = users & where (.<id == 1)\n", "q").contains("join predicate"));
    assert!(err("q = orders & inner users (.<user_id == .>name)\n", "q").contains("type mismatch"));
    assert!(err("q = orders & inner users (.<nope == .>id)\n", "q").contains("no column `nope`"));
}

#[test]
fn update_merges_over_the_input_row() {
    // An overwritten field keeps its position; the rest pass through.
    assert_eq!(
        ty("q = users & update { age = .age + 1 }\n", "q"),
        "query { id = int, name = string, age = int, active = bool }"
    );
    // A name the input does not have is appended, in field order.
    assert_eq!(
        ty("q = users & update { label = .name }\n", "q"),
        "query { id = int, name = string, age = int, active = bool, label = string }"
    );
    assert_eq!(
        ty("q = users & update { age = 0, tag = \"x\" }\n", "q"),
        "query { id = int, name = string, age = int, active = bool, tag = string }"
    );
    // An overwrite may change a field's type.
    assert_eq!(
        ty("q = users & update { age = 1.5 }\n", "q"),
        "query { id = int, name = string, age = float, active = bool }"
    );
    // Only the first field's own position matters: `update` is not `select`.
    assert_eq!(
        ty("q = users & update { name = \"x\", id = 9 }\n", "q"),
        "query { id = int, name = string, age = int, active = bool }"
    );
    // It composes with the rest of the pipeline, and a later stage sees the
    // new column.
    assert_eq!(
        ty(
            "q = users & update { n = .age + 1 } & where (.n > 3) & select { a = .n }\n",
            "q"
        ),
        "query { a = int }"
    );
}

#[test]
fn update_errors() {
    // An aggregate is not allowed in `update` any more than in `select`.
    assert!(err("q = users & update { n = count }\n", "q").contains("belong in `agg`"));
    assert!(err("q = users & update { n = .nope }\n", "q").contains("no column `nope`"));
    assert!(err("q = users & update { a = 1, a = 2 }\n", "q").contains("twice"));
    assert!(err("q = users & update { }\n", "q").contains("at least one field"));
}

#[test]
fn field_shorthand_selects_columns() {
    // `{.a, .b}` is `{a = .a, b = .b}`: selecting columns is plain `select`
    // now that the shorthand exists.
    assert_eq!(
        ty("q = users & select {.name, .id}\n", "q"),
        "query { name = string, id = int }"
    );
    assert_eq!(
        ty("q = users & select {.id, label = .name}\n", "q"),
        "query { id = int, label = string }"
    );
    assert_eq!(
        ty("q = users & select {.id, .name, .age, .active}\n", "q"),
        "query { id = int, name = string, age = int, active = bool }"
    );
    // The shorthand works anywhere a record does, including `update`.
    assert_eq!(
        ty("q = orders & select {.id, amount = .amount + 1.0}\n", "q"),
        "query { id = int, amount = float }"
    );
    assert_eq!(
        ty("q = users & select {.id} & select {.id}\n", "q"),
        "query { id = int }"
    );
    // A missing column is still a missing column.
    assert!(err("q = users & select {.nope}\n", "q").contains("no column `nope`"));
}

#[test]
fn update_renames_and_recomputes() {
    // Renaming a column is `update` with the value moved to the new name;
    // the old name keeps its place unless it is redefined.
    assert_eq!(
        ty(
            "q = users & update { user_id = .id, label = .name }\n",
            "q"
        ),
        "query { id = int, name = string, age = int, active = bool, user_id = int, label = string }"
    );
    // An update feeding a join and a later select is fully typed.
    assert_eq!(
        ty(
            "q = orders & update { order_id = .id } & inner users (.<user_id == .>id) \
             & select { o = .order_id, n = .name }\n",
            "q"
        ),
        "query { o = int, n = string }"
    );
    // A transformation is reusable through an ordinary definition.
    let src = "keep = q => q & select {.id}\na = users & keep\nb = orders & keep\n";
    assert_eq!(ty(src, "a"), "query { id = int }");
    assert_eq!(ty(src, "b"), "query { id = int }");
    // A list of strings is just a list of strings now, so it is not a sort
    // key: that used to be a key mapper's argument list.
    assert!(err("q = users & order [\"id\"]\n", "q").contains("type mismatch"));
}

#[test]
fn overloads_resolve_by_type() {
    assert_eq!(
        ty(
            "q = users & select { a = .age + 1, s = .name <> \"!\", f = negate .age }\n",
            "q"
        ),
        "query { a = int, s = string, f = int }"
    );
    // Leftover literals default before overloads are forced.
    assert_eq!(
        ty("q = users & select { x = 1 + 2 }\n", "q"),
        "query { x = int }"
    );
    // A helper with an open overload stays generic and works at both types.
    let src = "people : query { n = int, x = float } = table \"p\" \"people\"\n\
               twice = x => x + x\nq = people & select { a = twice .n, b = twice .x }\n";
    assert_eq!(ty(src, "q"), "query { a = int, b = float }");
    assert!(err("q = users & select { b = .active + .active }\n", "q")
        .contains("no overload of `+` matches"));
    // `+` is numeric only; strings use `<>`, and there are no conversions.
    assert!(err("q = users & select { s = .name + .name }\n", "q")
        .contains("no overload of `+` matches"));
    assert!(err("q = orders & select { x = .amount + .user_id }\n", "q")
        .contains("expected int, found float"));
    assert!(err("q = users & select { s = .name <> .age }\n", "q").contains("type mismatch"));
}

#[test]
fn user_overloads() {
    let src = "describe : expr r int -> expr r string = sql \"CAST($1 AS TEXT)\"\n\
               describe : expr r bool -> expr r string = sql \"CASE WHEN $1 THEN 'yes' ELSE 'no' END\"\n\
               q = users & select { a = describe .age, b = describe .active }\n";
    assert_eq!(ty(src, "q"), "query { a = string, b = string }");
    let amb = "pick2 : expr r int -> expr r int = sql \"$1\"\n\
               pick2 : expr r int -> expr r float = sql \"$1\"\n\
               q = users & select { a = pick2 .age }\n";
    assert!(err(amb, "q").contains("ambiguous use of `pick2`"));
}

#[test]
fn overloads_need_signatures() {
    let ws = Workspace::from_source("f = 1\nf = 2\n");
    assert!(
        ws.diags
            .iter()
            .any(|d| d.message.contains("needs a type signature")),
        "{:?}",
        ws.diags
    );
}

#[test]
fn checker_rejects_what_the_evaluator_cannot_build() {
    // `asc` takes an expression, not a sort key: `asc (desc .id)` used to
    // type-check and then fail in the evaluator.
    let e = err("q = users & order [asc (desc .id)]\n", "q");
    assert!(e.contains("sortkey"), "{e}");
    // A template needs its signature, or the evaluator cannot size it.
    let e = err("q = users & select { x = sql \"1\" }\n", "q");
    assert!(e.contains("signature"), "{e}");
}

#[test]
fn sql_templates_need_signatures() {
    // A template's arity and phase come from its signature; without one the
    // checker used to hand out a fresh variable and the evaluator rejected it.
    let e = err("f = sql \"1\"\n", "f");
    assert!(e.contains("needs a type signature"), "{e}");
}

#[test]
fn deep_forward_reference_chains_are_reported_not_a_crash() {
    // `def_scheme` recurses once per forward-referenced definition, so a long
    // chain used to overflow the stack before any diagnostic was reported.
    let chain = |n: usize| {
        let mut src = String::new();
        for i in 0..n - 1 {
            src.push_str(&format!("h{i} = x => h{} x\n", i + 1));
        }
        src.push_str(&format!("h{} = x => x\nq = h0 1\n", n - 1));
        src
    };
    // Well under the limit: every definition checks.
    let ok = types(&chain(20));
    assert!(ok.iter().all(|(_, r)| r.is_ok()), "{ok:?}");
    // Past it: reported once, and the rest of the chain still checks.
    let deep = types(&chain(100));
    let failed: Vec<&String> = deep.iter().filter_map(|(_, r)| r.as_ref().err()).collect();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(failed[0].contains("forward-referenced"), "{failed:?}");
}

const NULLABLE: &str = "people : query { id = int, email = maybe string, score = maybe int } = table \"p\" \"people\"\n";

#[test]
fn nullable_columns_are_explicit() {
    let q = |body: &str| format!("{NULLABLE}q = people & {body}\n");
    assert_eq!(
        ty(&q("select { e = .email }"), "q"),
        "query { e = maybe string }"
    );
    assert_eq!(
        ty(&q("where (isNotNull .email) & select { e = coalesce \"-\" .email, s = coalesce 0 .score + 1 }"), "q"),
        "query { e = string, s = int }"
    );
    let e = err(&q("where (.email == \"x\")"), "q");
    assert!(e.contains("non-null") && e.contains("coalesce"), "{e}");
    assert!(err(&q("select { s = .score + 1 }"), "q").contains("no overload of `+`"));
    // `just` makes a value nullable explicitly; there is no implicit lift.
    assert_eq!(
        ty(&q("select { m = just .id }"), "q"),
        "query { m = maybe int }"
    );
    // `coalesce`'s default is non-null (like Haskell's `fromMaybe`).
    assert!(err(&q("select { m = coalesce (just .id) .score }"), "q").contains("non-null"));
    let e = err(&q("select { m = coalesce 0 .id }"), "q");
    assert!(
        e.contains("field `id`") && e.contains("only one side is nullable"),
        "{e}"
    );
    // `where` needs `bool`; `isTrue` treats NULL as false.
    let src = format!("{NULLABLE}flags : query {{ ok = maybe bool }} = table \"p\" \"f\"\nq = flags & where .ok\nr = flags & where (isTrue .ok)\n");
    assert!(err(&src, "q").contains("bool"));
    ty(&src, "r");
}

#[test]
fn outer_joins_make_the_missing_side_maybe() {
    let j = |kind: &str| {
        ty(&format!("q = orders & {kind} users (.<user_id == .>id) & select {{ o = .amount, n = .name }}\n"), "q")
    };
    assert_eq!(j("inner"), "query { o = float, n = string }");
    assert_eq!(j("leftJoin"), "query { o = float, n = maybe string }");
    assert_eq!(j("rightJoin"), "query { o = maybe float, n = string }");
    assert_eq!(j("fullJoin"), "query { o = maybe float, n = maybe string }");
    // An already nullable column is not wrapped twice.
    let src =
        format!("{NULLABLE}q = users & leftJoin people (.<id == .>id) & select {{ e = .email }}\n");
    assert_eq!(ty(&src, "q"), "query { e = maybe string }");
    // The predicate sees the plain column types.
    ty(
        "q = orders & leftJoin users (.<user_id == .>id && .>active)\n",
        "q",
    );
}

#[test]
fn aggregate_nullability_is_explicit() {
    let src = format!("{NULLABLE}q = people & agg {{ n = count, c = countOf .id, s = sum (coalesce 0 .score), t = coalesce \"\" (max (coalesce \"\" .email)) }}\n");
    assert_eq!(
        ty(&src, "q"),
        "query { n = int, c = int, s = maybe int, t = string }"
    );
    let e = err(
        "q = orders & agg { r = sum .amount } & where (.r > 100.0)\n",
        "q",
    );
    assert!(e.contains("maybe float") && e.contains("coalesce"), "{e}");
    ty(
        "q = orders & agg { r = coalesce 0.0 (sum .amount) } & where (.r > 100.0)\n",
        "q",
    );
    assert!(err(
        "q = orders & agg { u = group .user_id } & select { x = sum .u }\n",
        "q"
    )
    .contains("belong in `agg`"));
    let e = err(
        &format!("{NULLABLE}q = people & agg {{ s = sum .score }}\n"),
        "q",
    );
    assert!(
        e.contains("no overload of `sum`") && e.contains("maybe int"),
        "{e}"
    );
    let e = err(
        &format!(
            "{NULLABLE}spec = {{ partition = [.id], order = [desc .id] }}\nq = people & select {{ l = lag spec .email }}\n"
        ),
        "q",
    );
    assert!(e.contains("non-null") && e.contains("coalesce"), "{e}");
    let e = err(
        &format!("{NULLABLE}q = people & agg {{ c = countOf .email }}\n"),
        "q",
    );
    assert!(e.contains("non-null") && e.contains("maybe"), "{e}");
    let e = err(
        "bad : expr r (maybe a) -> agg (expr r (maybe a)) = sql \"SUM($1)\"\n",
        "bad",
    );
    assert!(
        e.contains("aggregate/window inputs cannot use `maybe`"),
        "{e}"
    );
}

#[test]
fn editing_the_root_rechecks_only_the_root() {
    let mut ws = Workspace::from_source(&format!("{TABLES}q = users & where (.age > 1)\n"));
    assert!(check(&ws).errors.is_empty());
    let runs = super::check_runs();
    let _ = check(&ws);
    assert_eq!(
        super::check_runs(),
        runs,
        "nothing changed, so nothing is rechecked"
    );
    // Edit through the workspace, as an editor does, so the db and the
    // workspace's cached text stay in step.
    assert!(ws.set_source(
        ws.root,
        format!("{TABLES}q = users & where (.age > \"x\")\n")
    ));
    let after = check(&ws);
    assert_eq!(
        super::check_runs(),
        runs + 1,
        "only the edited root is rechecked, not the prelude"
    );
    assert!(
        after
            .errors
            .iter()
            .any(|e| e.module == ws.root && e.diag.message.contains("type mismatch")),
        "{:?}",
        after.errors
    );
}

#[test]
fn edits_update_names_checks_and_diagnostics() {
    let mut ws = Workspace::from_source(&format!("{TABLES}q = users & where (.age > 1)\n"));
    let root = ws.root;
    // A new definition is visible to later ones (scope is a query, not a
    // snapshot from load time), to the checker, and to the evaluator.
    assert!(ws.set_source(
        root,
        format!("{TABLES}adult = .age >= 18\nq = users & where adult\n")
    ));
    assert!(check(&ws).errors.is_empty(), "{:?}", check(&ws).errors);
    let q = crate::root_queries(&ws)
        .into_iter()
        .find(|(n, _)| n == "q")
        .unwrap()
        .1;
    assert!(q.is_ok(), "{q:?}");
    // Syntax errors appear and disappear with the text.
    assert!(ws.set_source(root, format!("{TABLES}q = (\n")));
    assert!(
        ws.diags
            .iter()
            .any(|d| d.message.starts_with("syntax error")),
        "{:?}",
        ws.diags
    );
    assert!(ws.set_source(root, format!("{TABLES}q = users\n")));
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    // Changing imports needs a reload.
    assert!(!ws.set_source(root, format!("import \"other.cagara\"\n{TABLES}")));
}

#[test]
fn unused_definitions_still_report_impossible_overloads() {
    // Nothing uses `bad`, so it is never evaluated; its `+` cannot be resolved
    // for any type, and that must be reported anyway.
    let e = err("bad = .age + \"x\"\n", "bad");
    assert!(e.contains("no overload of `+`"), "{e}");
    assert!(e.contains("int") && e.contains("float"), "{e}");
    // A helper that *can* be instantiated keeps its open overloads, and stays
    // usable at more than one type.
    let poly = "twice = x => x + x\n";
    assert_eq!(ty(poly, "twice"), "expr a b -> expr a b");
    ty(
        &format!("{poly}q = users & select {{ a = twice .age }}\n"),
        "q",
    );
    let both = format!(
        "{poly}q = users & orders ? (.<id == .>user_id) & select {{ a = twice .age, b = twice .amount }}\n"
    );
    assert!(
        ty(&both, "q").contains("a = int") && ty(&both, "q").contains("b = float"),
        "{}",
        ty(&both, "q")
    );
}

#[test]
fn constants_keep_their_polymorphic_type_when_unused() {
    // `.age >= 18` may be int or float; checking it must not narrow the
    // literal, so the definition stays usable at both widths.
    let src = "adult = .age >= 18\n";
    assert_eq!(ty(src, "adult"), "expr { age = a | b } bool");
    // Used on an int column.
    ty(
        &format!("{src}q = users & where (adult) & select {{ a = .age }}\n"),
        "q",
    );
    // Used on a float column of the same name.
    let widths = "metrics : query { age = float } = table \"p\" \"metrics\"\n";
    ty(
        &format!("{src}{widths}q = metrics & where (adult) & select {{ a = .age }}\n"),
        "q",
    );
    // A column that does not exist is still an error.
    assert!(err(&format!("{src}q = orders & where (adult)\n"), "q").contains("no column `age`"));
}

#[test]
fn overload_candidates_resolve_their_own_overloads() {
    // `1 + 2` is open inside the int candidate. Uses pick a candidate by its
    // signature alone, so the candidate resolves it (defaulting the
    // literals) instead of leaving a hole no use can fill. This used to hit
    // an `unreachable!` in the checker.
    let src = "k = x => y => x\n\
               f : expr r int -> expr r int = x => k x (1 + 2)\n\
               f : expr r float -> expr r float = x => x\n\
               q = users & select { v = f .age, w = f 1.5 }\n";
    assert_eq!(ty(src, "q"), "query { v = int, w = float }");
    let ws = Workspace::from_source(&format!("{TABLES}{src}"));
    let tc = check(&ws);
    for (name, r) in crate::root_queries_checked(&ws, &tc) {
        assert!(r.is_ok(), "`{name}`: {:?}", r.err());
    }
}

#[test]
fn users_of_a_failed_definition_fail_too() {
    // `r` has a type error; `q` must not be compiled against a made-up type.
    let src = "r = users & select { s = .name, bad = .id + \"x\" }\n\
               q = r & select { z = .s + 1 }\n\
               q2 = q & select { y = .z }\n";
    assert!(err(src, "r").contains("found string"), "{}", err(src, "r"));
    assert!(
        err(src, "q").contains("`r` has a type error"),
        "{}",
        err(src, "q")
    );
    assert!(
        err(src, "q2").contains("`q` has a type error"),
        "{}",
        err(src, "q2")
    );
    // So nothing evaluates either.
    let ws = Workspace::from_source(&format!("{TABLES}{src}"));
    let tc = check(&ws);
    let out = crate::root_queries_checked(&ws, &tc);
    for name in ["r", "q", "q2"] {
        let (_, r) = out.iter().find(|(n, _)| n == name).expect("reported");
        assert!(r.is_err(), "`{name}` compiled");
    }
}

/// Every root definition the checker accepts must also evaluate and pass the
/// IR validator: the two share their rules (`crate::rules`), and a program
/// that only the validator rejects is a checker bug. Returns each checked
/// definition's type or error, like `types`.
fn consistent(ws: &Workspace) -> Vec<(String, Result<String, String>)> {
    let tc = check(ws);
    let m = ws.root;
    let evaluated = crate::root_queries_checked(ws, &tc);
    let mut out = Vec::new();
    for (i, d) in ws.modules[m].module.defs.iter().enumerate() {
        let r = match tc.error_for(m, i) {
            Some(e) => Err(e.message.clone()),
            None => {
                if let Some((_, Err(e))) = evaluated.iter().find(|(n, _)| *n == d.name) {
                    panic!("`{}` type-checks but fails later: {}", d.name, e.message);
                }
                Ok(tc.type_of(m, i).unwrap_or("?").to_string())
            }
        };
        out.push((d.name.clone(), r));
    }
    out
}

fn consistent_src(src: &str) -> Vec<(String, Result<String, String>)> {
    let ws = Workspace::from_source(&format!("{TABLES}{src}"));
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    consistent(&ws)
}

fn result<'a>(
    rs: &'a [(String, Result<String, String>)],
    name: &str,
) -> &'a Result<String, String> {
    &rs.iter()
        .find(|(n, _)| n == name)
        .expect("no such definition")
        .1
}

#[test]
fn examples_are_consistent() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    for f in [
        "report.cagara",
        "public.cagara",
        "errors.cagara",
        "schema.cagara",
    ] {
        let ws = Workspace::open(&dir.join(f));
        assert!(ws.diags.is_empty(), "{f}: {:?}", ws.diags);
        let results = consistent(&ws);
        if f != "errors.cagara" {
            for (name, result) in results {
                assert!(result.is_ok(), "{f} / `{name}`: {result:?}");
            }
        }
    }
}

#[test]
fn a_select_helper_types_at_each_use() {
    // One helper whose stage is applied to two different rows: each use is
    // checked against its own input, so both are valid.
    let rs = consistent_src(
        "h = p => users & select { id = .id } & p\n\
         a = h (select { z = .id })\n\
         b = users & select {.id}\n",
    );
    assert_eq!(result(&rs, "a").as_deref(), Ok("query { z = int }"));
    assert_eq!(result(&rs, "b").as_deref(), Ok("query { id = int }"));
}

#[test]
fn select_fields_stay_out_of_the_aggregate_phase() {
    // `e` is open inside `h`; `select` now fixes it to row-or-window, so
    // passing an aggregate fails at the use instead of in the validator.
    let h = "h = e => users & select { x = e + 1, id = .id }\n";
    let rs = consistent_src(&format!(
        "{h}q = h count\nok = h .age\nwin = h (rowNumber {{ order = [asc .id] }})\n"
    ));
    assert!(
        result(&rs, "q")
            .as_ref()
            .is_err_and(|e| e.contains("`select`")),
        "{rs:?}"
    );
    assert!(result(&rs, "ok").is_ok(), "{rs:?}");
    assert!(result(&rs, "win").is_ok(), "{rs:?}");
}

#[test]
fn windows_compose_in_an_expression() {
    // A window is an expression like any other: it may be the whole field, an
    // operand, or part of a larger expression. Only a filter, a key, and a
    // join predicate reject it (a window is computed per row).
    let rs = consistent_src(
        "spec = { order = [asc .id] }\n\
         whole = users & select { id = .id, rn = rowNumber spec }\n\
         operand = users & select { id = .id, x = rowNumber spec + 1 }\n\
         two = users & select { id = .id, x = rowNumber spec + rowNumber spec }\n\
         filtered = users & where (rowNumber spec <= 3)\n\
         scoped = users & select { id = .id, rn = countOver spec }\n",
    );
    assert_eq!(
        result(&rs, "whole").as_deref(),
        Ok("query { id = int, rn = int }")
    );
    assert_eq!(
        result(&rs, "operand").as_deref(),
        Ok("query { id = int, x = int }")
    );
    assert_eq!(
        result(&rs, "two").as_deref(),
        Ok("query { id = int, x = int }")
    );
    assert!(
        result(&rs, "filtered")
            .as_ref()
            .is_err_and(|e| e.contains("window")),
        "{rs:?}"
    );
    assert_eq!(
        result(&rs, "scoped").as_deref(),
        Ok("query { id = int, rn = int }")
    );
}

#[test]
fn a_scalar_template_over_an_aggregate_is_an_aggregate() {
    let rs = consistent_src(
        "inc : agg (expr r int) -> expr r int = sql \"$1 + 1\"\n\
         bad = users & select { v = inc count, id = .id }\n\
         good = users & agg { v = inc count }\n\
         both : agg (expr r int) -> win (expr r int) -> expr r int = sql \"$1 + $2\"\n",
    );
    assert!(
        result(&rs, "bad")
            .as_ref()
            .is_err_and(|e| e.contains("aggregate")),
        "{rs:?}"
    );
    assert_eq!(result(&rs, "good").as_deref(), Ok("query { v = int }"));
    assert!(
        result(&rs, "both")
            .as_ref()
            .is_err_and(|e| e.contains("window")),
        "{rs:?}"
    );
}

#[test]
fn select_needs_at_least_one_field() {
    let rs = consistent_src(
        "a = users & select { }\n\
         b = users & select {.active}\n",
    );
    assert!(
        result(&rs, "a")
            .as_ref()
            .is_err_and(|e| e.contains("at least one field")),
        "{rs:?}"
    );
    assert_eq!(result(&rs, "b").as_deref(), Ok("query { active = bool }"));
}

#[test]
fn outer_joins_wrap_once_even_through_helpers() {
    // The right input's column types are unknown inside `f`; wrapping them
    // there made `u.v : maybe int` a `maybe (maybe int)`.
    let rs = consistent_src(
        "u : query { uid = int, v = maybe int, n = int } = table \"p\" \"u\"\n\
         f = q => users & leftJoin (q & select { uid = .uid, v = .v, n = .n }) (.<id == .>uid)\n\
         q = f u & select { w = .v ?? 0, n = .n ?? 0 }\n",
    );
    assert_eq!(
        result(&rs, "q").as_deref(),
        Ok("query { w = int, n = int }")
    );
}

/// A long left-associative chain is one level of parser recursion but a
/// left-nested AST of that depth, and the checker walks it recursively. With
/// the parser's budget at 256 the checker ran out of stack on a 200-operator
/// chain: the `cagara` binary aborted (`SIGABRT`) rather than reporting
/// anything, so a user could crash the compiler with a long expression. The
/// parser budget is now low enough that the checker never sees a chain it
/// cannot descend.
///
/// This runs at every length around the limit, since the failure appeared in a
/// window (200..250) rather than at one value.
#[test]
fn a_long_operator_chain_is_checked_or_reported_but_never_crashes() {
    let chain = |n: usize| {
        let e = vec![".a"; n + 1].join(" + ");
        format!("t : query {{ a = int }} = table \"s\" \"t\"\nq = t & select {{ x = {e} }}\n")
    };
    for n in [1, 2, 10, 40, 80, 100, 120, 140, 150, 160, 180, 190, 200, 250, 300, 1000] {
        let ws = Workspace::from_source(&chain(n));
        // Either the parser rejected it (a diagnostic) or the checker ran.
        // The point is that neither panics or recurses until the stack ends.
        let tc = check(&ws);
        let _ = tc.errors.len();
    }
}

/// The deepest chain the parser accepts must also check, so the parser's
/// budget stays below what the rest of the pipeline can walk. `MAX_DEPTH` in
/// `cagara-syntax` is 192; a chain of that many operators is rejected, and one
/// comfortably under it is accepted and checked.
#[test]
fn the_parser_budget_leaves_the_checker_room() {
    let chain = |n: usize| {
        let e = vec![".a"; n + 1].join(" + ");
        format!("t : query {{ a = int }} = table \"s\" \"t\"\nq = t & select {{ x = {e} }}\n")
    };
    // Well inside the budget: accepted and well-typed.
    let ws = Workspace::from_source(&chain(100));
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    assert!(check(&ws).errors.is_empty(), "{:?}", check(&ws).errors);
    // Past it: a syntax diagnostic, not a crash.
    let ws = Workspace::from_source(&chain(300));
    assert!(
        !ws.diags.is_empty(),
        "a chain past the parser budget must be reported"
    );
}
