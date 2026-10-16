use super::{check, RowStage};
use crate::core::ScalarType;
use crate::workspace::Workspace;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const TABLES: &str = "users : query { id = int, name = string, age = int, active = bool } = table \"public\" \"users\"\n\
orders : query { id = int, user_id = int, amount = float, status = string } = table \"public\" \"orders\"\n";

/// Type of each root definition, or its error message.
fn types(src: &str) -> Vec<(String, Result<String, String>)> {
    let ws = Workspace::from_source(&format!("{TABLES}{src}"));
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let tc = check(&ws);
    for e in tc.errors() {
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

/// The row a query definition produces, as a set of `name = type` pairs.
///
/// A key stage's stored type may still carry its `keyMap (prefix "u_") …` term
/// wrapper: the *row inside* is already the renamed one, and the wrapper is the
/// honest general form when the mapper is polymorphic. Tests about renaming
/// care about the columns, so they compare this.
fn row_of(src: &str, name: &str) -> Vec<String> {
    let t = ty(src, name);
    let inner = match t.find('{') {
        Some(i) => &t[i..],
        None => panic!("`{name}` has no row: {t}"),
    };
    inner
        .trim_start_matches('{')
        .trim_end_matches(['}', ')', ' '])
        .split(',')
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

/// One recorded row-rewrite stage: what it applied, the columns it read, and
/// the columns it produced. A column is its name; `None` means the row is
/// still open.
type RecordedRowStage = (RowStage, Option<Vec<String>>, Option<Vec<String>>);

/// The row-rewrite stages a definition recorded.
fn row_stage_nodes(src: &str, name: &str) -> Vec<RecordedRowStage> {
    let ws = Workspace::from_source(&format!("{TABLES}{src}"));
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let tc = check(&ws);
    let m = ws.root;
    let i = ws.modules[m]
        .module
        .defs
        .iter()
        .position(|d| d.name == name)
        .expect("no such definition");
    let names = |fs: &Option<Vec<(String, ScalarType)>>| {
        fs.as_ref()
            .map(|fs| fs.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>())
    };
    tc.row_stages(m, i)
        .iter()
        .map(|s| (s.kind, names(&s.input), names(&s.output)))
        .collect()
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

/// The checker's rejection of `name`, or `None` if the checker accepted it.
///
/// The counterpart to [`ty`] for a program that may be stopped later in the
/// pipeline instead: a definition the checker takes can still fail to compile,
/// and the message there is the honest one to assert on.
fn ty_err(src: &str, name: &str) -> Option<String> {
    types(src)
        .into_iter()
        .find(|(n, _)| n == name)
        .expect("no such definition")
        .1
        .err()
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
        ty("q = orders & innerJoin users (.<user_id == .>id)\n", "q"),
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

/// `case { cond => value; ... }`: like `ifThenElse`, one row and one result
/// type for every arm, and one phase for the whole expression.
#[test]
fn case_types_like_a_multi_way_if() {
    assert_eq!(
        ty(
            "q = orders & select { b = case { .amount > 100.0 => \"big\"; .amount > 10.0 => \"mid\"; true => \"small\" } }\n",
            "q"
        ),
        "query { b = string }"
    );
    // The result type comes from the arms, not from a `true` arm, and the
    // conditions are lifted like any other literal (`0` becomes a float
    // when it shares an arm's type with one).
    assert_eq!(
        ty(
            "q = orders & select { b = case { .amount > 100.0 => 1; true => 0 } }\n",
            "q"
        ),
        "query { b = int }"
    );
    assert_eq!(
        ty(
            "q = orders & select { b = case { .amount > 100.0 => 1.5; true => 0 } }\n",
            "q"
        ),
        "query { b = float }"
    );
    // An arm may be a whole expression over the input row, and the default
    // arm may stand alone when there is nothing to test against.
    assert_eq!(
        ty(
            "people : query { id = int, score = int } = table \"p\" \"people\"\n\
             q = people & select { b = case { .score > 10 => .score; true => 0 } }\n",
            "q"
        ),
        "query { b = int }"
    );
    assert_eq!(
        ty(
            "people : query { id = int, score = int } = table \"p\" \"people\"\n\
             q = people & select { b = case { true => 7 } }\n",
            "q"
        ),
        "query { b = int }"
    );
    // The arms' phase is the expression's: a case of aggregates is an
    // aggregate, and is allowed exactly where `sum` is. Both arms must agree
    // on type, so each is `coalesce`d — `sum` itself is a `maybe`.
    assert_eq!(
        ty(
            "q = orders & agg { k = group .user_id, b = case { count > 3 => coalesce 0.0 (sum .amount); true => 0.0 } }\n",
            "q"
        ),
        "query { k = int, b = float }"
    );
    // And with `maybe` arms, the result is a `maybe`.
    assert_eq!(
        ty(
            "q = orders & agg { k = group .user_id, b = case { count > 3 => sum .amount; true => sum .amount } }\n",
            "q"
        ),
        "query { k = int, b = maybe float }"
    );
}

/// The mistakes a `case` can make: arms that disagree, conditions that are not
/// booleans, conditions over a different row, and no arm at all.
///
/// The messages are asserted as observed, and each one names the *arm* at
/// fault rather than the `case` as a whole — which is what the `ExprKind::Case`
/// spans are for. The condition cases come back through `select`'s field, so
/// they carry the `field \`b\`:` prefix the projection adds.
#[test]
fn case_rejects_mismatched_arms() {
    let e = err(
        "q = orders & select { b = case { .amount > 1.0 => 1; true => \"x\" } }\n",
        "q",
    );
    assert!(e.contains("expected int, found string"), "{e}");
    // A condition has to be a bool, and the message names the condition —
    // not the arm and not the whole case.
    let e = err(
        "q = orders & select { b = case { .amount => 1; true => 2 } }\n",
        "q",
    );
    assert!(e.contains("expected bool, found float"), "{e}");
    // Every condition and value is read against the *same* row, so a
    // condition that names a column the input does not have fails here
    // rather than silently in SQL.
    let e = err(
        "q = orders & select { b = case { .nope > 1 => 1; true => 2 } }\n",
        "q",
    );
    assert!(e.contains("no column `nope`"), "{e}");
    // A `case` with no arm at all has no result type, so it is refused
    // rather than given a made-up one.
    let e = err("q = orders & select { b = case {} }\n", "q");
    assert!(e.contains("at least one arm"), "{e}");
    // A `case` whose last arm is conditional has a hole: SQL's `CASE` yields
    // NULL for a row no arm matches, which the arms present would type as
    // non-null. Refusing it is what keeps the result type honest.
    let e = err(
        "q = orders & select { b = case { .amount > 1.0 => 1 } }\n",
        "q",
    );
    assert!(e.contains("default arm"), "{e}");
    let e = err(
        "q = orders & select { b = case { .amount > 1.0 => 1; .amount > 2.0 => 2 } }\n",
        "q",
    );
    assert!(e.contains("default arm"), "{e}");
}

/// A condition that decides a branch must not be nullable, exactly as a
/// `where` predicate must not be: a NULL cannot choose an arm, and both SQL and
/// Cagara decline to define which one it would pick.
///
/// `case` gets this from the ordinary machinery — the condition is compared
/// against `expr r bool` like any other argument, and a `maybe` is refused
/// there. The message is the one `where` gives, which is the point: there is
/// one rule and one way of stating it, not a rule per conditional form.
#[test]
fn case_rejects_a_nullable_condition() {
    let nullable = "people : query { id = int, score = maybe int } = table \"p\" \"people\"\n";
    let e = err(
        &format!(
            "{nullable}q = people & select {{ b = case {{ .score > 10 => 1; true => 2 }} }}\n"
        ),
        "q",
    );
    assert!(e.contains("non-null") && e.contains("coalesce"), "{e}");
    // `coalesce` clears it, and what the arm *produces* may be a `maybe` as
    // long as every arm agrees.
    assert_eq!(
        ty(
            &format!("{nullable}q = people & select {{ b = case {{ coalesce 0 .score > 10 => 1; true => 2 }} }}\n"),
            "q"
        ),
        "query { b = int }"
    );
}

/// The arms of a `case` share one phase, so a case of aggregates is an
/// aggregate and belongs where `sum` belongs.
#[test]
fn case_takes_the_phase_of_its_arms() {
    assert_eq!(
        ty(
            "q = orders & agg { k = group .user_id, b = case { count > 3 => coalesce 0.0 (sum .amount); true => 0.0 } }\n",
            "q"
        ),
        "query { k = int, b = float }"
    );
    // ...which is why one does not fit in a `select` field beside plain
    // columns: the same rule that rejects `select { n = sum .amount }`.
    let e = err(
        "q = orders & select { k = .user_id, b = case { count > 3 => coalesce 0.0 (sum .amount); true => 0.0 } }\n",
        "q",
    );
    assert!(e.contains("aggregate"), "{e}");
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
    assert!(err("q = users & limit .name\n", "q").contains("type mismatch"));
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
    ty(
        "a : agg (expr r (row s)) -> query r -> query s = agg\n",
        "a",
    );
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
    assert!(err("q = orders & innerJoin users (.user_id == .id)\n", "q").contains("which input"));
    assert!(err("q = users & where (.<id == 1)\n", "q").contains("join predicate"));
    assert!(
        err("q = orders & innerJoin users (.<user_id == .>name)\n", "q").contains("type mismatch")
    );
    assert!(
        err("q = orders & innerJoin users (.<nope == .>id)\n", "q").contains("no column `nope`")
    );
}

#[test]
fn update_merges_over_the_input_row() {
    assert_eq!(
        ty("u = update\n", "u"),
        "expr a (row b) -> query a -> query merge(a, b)"
    );
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
            "q = orders & update { order_id = .id } & innerJoin users (.<user_id == .>id) \
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
fn checker_rejects_invalid_sort_keys_and_templates() {
    // `asc` takes an expression, not another sort key.
    let e = err("q = users & order [asc (desc .id)]\n", "q");
    assert!(e.contains("sortkey"), "{e}");
    // A template needs its signature to determine its argument shape.
    let e = err("q = users & select { x = sql \"1\" }\n", "q");
    assert!(e.contains("signature"), "{e}");
}

#[test]
fn sql_templates_need_signatures() {
    // A template's arity and phase come from its signature.
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

    let helpers = format!(
        "{NULLABLE}flags : query {{ ok = maybe bool }} = table \"p\" \"f\"\n\
         q = flags & whereTrue .ok\n\
         r = people & select {{ same = eqMaybe .email .email, cleared = nullIf (coalesce 0 .score) 0, b = toBool .id }}\n"
    );
    ty(&helpers, "q");
    assert_eq!(
        ty(&helpers, "r"),
        "query { same = bool, cleared = maybe int, b = bool }"
    );
}

#[test]
fn outer_joins_make_the_missing_side_maybe() {
    let j = |kind: &str| {
        ty(&format!("q = orders & {kind} users (.<user_id == .>id) & select {{ o = .amount, n = .name }}\n"), "q")
    };
    assert_eq!(j("innerJoin"), "query { o = float, n = string }");
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
    assert!(check(&ws).errors().is_empty());
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
            .errors()
            .iter()
            .any(|e| e.module == ws.root && e.diag.message.contains("type mismatch")),
        "{:?}",
        after.errors()
    );
}

#[test]
fn compilation_reuses_the_workspace_cache_and_invalidates_dependents() {
    let root_path = PathBuf::from("/tmp/cagara-cache/root.cagara");
    let lib_path = PathBuf::from("/tmp/cagara-cache/lib.cagara");
    let root_src = "import \"lib.cagara\" as lib\nq = lib.value\n";
    let lib_src = "value = 42\n";
    let mut buffers = HashMap::new();
    buffers.insert(root_path.clone(), root_src.to_string());
    buffers.insert(lib_path.clone(), lib_src.to_string());
    let mut ws = Workspace::open_with_buffers(&root_path, root_src.to_string(), &buffers);

    let first = crate::compile::compile(&ws);
    assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
    let after_first = super::check_runs();

    let second = crate::compile::compile(&ws);
    assert!(second.diagnostics.is_empty(), "{:?}", second.diagnostics);
    assert_eq!(
        super::check_runs(),
        after_first,
        "repeated compilation should reuse cached module checks"
    );

    let root = ws.root;
    assert!(ws.set_source(
        root,
        "import \"lib.cagara\" as lib\nq = lib.value + 1\n".into()
    ));
    let after_root_edit = crate::compile::compile(&ws);
    assert!(
        after_root_edit.diagnostics.is_empty(),
        "{:?}",
        after_root_edit.diagnostics
    );
    assert_eq!(
        super::check_runs(),
        after_first + 1,
        "editing the root should only recheck the root"
    );

    let lib = ws
        .module_for_path(Path::new("/tmp/cagara-cache/lib.cagara"))
        .expect("imported module should be loaded");
    assert!(ws.set_source(lib, "value = true\n".into()));
    let after_lib_edit = crate::compile::compile(&ws);
    assert!(!after_lib_edit.diagnostics.is_empty());
    assert_eq!(
        super::check_runs(),
        after_first + 3,
        "editing an imported module should recheck it and its root dependent"
    );
}

#[test]
fn checks_share_immutable_module_results_across_requests() {
    let mut ws = Workspace::from_source("value = 1\n");
    let first = check(&ws);
    let repeated = check(&ws);
    for (a, b) in first.modules.iter().zip(&repeated.modules) {
        assert!(std::sync::Arc::ptr_eq(a, b));
    }
    assert!(ws.set_source(ws.root, "value = true\n".into()));
    let edited = check(&ws);
    assert!(std::sync::Arc::ptr_eq(
        &first.modules[0],
        &edited.modules[0]
    ));
    assert!(!std::sync::Arc::ptr_eq(
        &first.modules[ws.root],
        &edited.modules[ws.root]
    ));
    assert_eq!(first.type_of(ws.root, 0), Some("int"));
    assert_eq!(edited.type_of(ws.root, 0), Some("bool"));
}

#[test]
fn edits_update_names_checks_and_diagnostics() {
    let mut ws = Workspace::from_source(&format!("{TABLES}q = users & where (.age > 1)\n"));
    let root = ws.root;
    // A new definition is visible to later ones (scope is a query, not a
    // snapshot from load time), and to compilation.
    assert!(ws.set_source(
        root,
        format!("{TABLES}adult = .age >= 18\nq = users & where adult\n")
    ));
    assert!(check(&ws).errors().is_empty(), "{:?}", check(&ws).errors());
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
    let verdicts = crate::root_verdicts(ws, &tc);
    let mut out = Vec::new();
    for (i, d) in ws.modules[m].module.defs.iter().enumerate() {
        let r = match tc.error_for(m, i) {
            Some(e) => Err(e.message.clone()),
            None => {
                match verdicts
                    .iter()
                    .find(|(n, _, _)| *n == d.name)
                    .map(|(_, r, f)| (r, *f))
                {
                    // A definition the checker accepted but a later phase could
                    // not build is a *compiler capability gap*, and this helper
                    // deliberately does not treat it as a rule violation: its
                    // contract is "the checker accepts a program => the program
                    // compiles, and any rejection came from a rule the checker
                    // shares rather than from the IR validator alone". Counting
                    // the gap as a failure here would take the whole suite down
                    // for any un-migrated construct, hiding gaps instead of
                    // surfacing them.
                    //
                    // This reads the structured [`crate::Fault`] rather than
                    // matching the rendered message. The old substring test on
                    // "internal error in" classified by user-facing wording:
                    // rewording the diagnostic would have silently turned every
                    // gap into a panic here, and a *program* error that happened
                    // to quote that phrase would have been swallowed. The
                    // dedicated gates are
                    // `compile::tests::shipped_examples_have_no_compiler_gaps`,
                    // `…::accepted_programs_have_no_checker_errors`, and the
                    // fuzzer's totality check.
                    Some((Err(e), crate::Fault::Compiler)) => {
                        Err(format!("compiler gap: {}", e.message))
                    }
                    Some((Err(e), _)) => {
                        panic!("`{}` type-checks but fails later: {}", d.name, e.message)
                    }
                    _ => Ok(tc.type_of(m, i).unwrap_or("?").to_string()),
                }
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
    for n in [
        1, 2, 10, 40, 80, 100, 120, 140, 150, 160, 180, 190, 200, 250, 300, 1000,
    ] {
        let ws = Workspace::from_source(&chain(n));
        // Either the parser rejected it (a diagnostic) or the checker ran.
        // The point is that neither panics or recurses until the stack ends.
        let tc = check(&ws);
        let _ = tc.errors().len();
    }
}

/// The deepest chain the parser accepts must also check, so the parser's
/// budget stays below what the rest of the pipeline can walk. `MAX_DEPTH` in
/// `cagara-syntax` is 128; a chain of that many operators is rejected, and one
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
    assert!(check(&ws).errors().is_empty(), "{:?}", check(&ws).errors());
    // Past it: a syntax diagnostic, not a crash.
    let ws = Workspace::from_source(&chain(300));
    assert!(
        !ws.diags.is_empty(),
        "a chain past the parser budget must be reported"
    );
}

#[test]
fn omit_is_a_row_equation() {
    // `omit "k"` states `input ~ { k : t | output }`, so the unifier's own
    // leftover rule produces the columns — in the input's order — and a missing
    // key is the ordinary `no column` error. No bespoke rule is involved.
    let rs = consistent_src("no_id = omit \"id\"\na = users & no_id\nb = orders & no_id\n");
    // The equation travels in the scheme: one definition, two output rows.
    assert_eq!(
        result(&rs, "a").clone().expect("a checks"),
        "query { name = string, age = int, active = bool }"
    );
    assert_eq!(
        result(&rs, "b").clone().expect("b checks"),
        "query { user_id = int, amount = float, status = string }"
    );
    // Order is the input's: a middle column leaves the others in place.
    assert_eq!(
        ty("q = users & omit \"name\"\n", "q"),
        "query { id = int, age = int, active = bool }"
    );
    // A later stage sees the row the equation produced.
    assert_eq!(
        ty(
            "q = users & omit \"age\" & where (.active) & select {.id}\n",
            "q"
        ),
        "query { id = int }"
    );
}

#[test]
fn omit_errors() {
    assert!(err("q = users & omit \"nope\"\n", "q").contains("no column `nope`"));
    // A computed key cannot name a column when the query is checked.
    assert!(err("q = users & omit .id\n", "q").contains("needs a literal column name"));
    // The omitted column is gone for later stages too.
    assert!(err("q = users & omit \"age\" & where (.age > 1)\n", "q").contains("no column `age`"));
}

#[test]
fn prefix_rewrites_every_name() {
    // `prefix "u_"` applies the `prefix "u_"` mapper to every column name, and the types
    // follow the source column: `age` is renamed and stays an int.
    assert_eq!(
        row_of("q = users & prefix \"u_\"\n", "q"),
        vec![
            "u_id = int",
            "u_name = string",
            "u_age = int",
            "u_active = bool"
        ]
    );
    assert_eq!(
        row_of("q = users & suffix \"_v2\"\n", "q"),
        vec![
            "id_v2 = int",
            "name_v2 = string",
            "age_v2 = int",
            "active_v2 = bool"
        ]
    );
    assert_eq!(
        row_of("q = users & prefix \"c_\"\n", "q"),
        vec![
            "c_id = int",
            "c_name = string",
            "c_age = int",
            "c_active = bool"
        ]
    );
}

#[test]
fn prefix_composes() {
    // Chaining renames in order: `prefix` first, then `suffix`. This pins the
    // Composition direction is `g ∘ f` — apply the first, then the second.
    // Getting it backwards would produce `_v2u_id`.
    assert_eq!(
        row_of("q = users & prefix \"u_\" & suffix \"_v2\"\n", "q"),
        vec![
            "u_id_v2 = int",
            "u_name_v2 = string",
            "u_age_v2 = int",
            "u_active_v2 = bool"
        ]
    );
    // A later stage sees the renamed row.
    assert_eq!(
        row_of("q = users & prefix \"u_\" & select {.u_id, .u_name}\n", "q"),
        vec!["u_id = int", "u_name = string"]
    );
}

#[test]
fn stage_nodes_chain_their_rows() {
    // Each stage application records the row it read and the row it produced,
    // and the two chain: the second stage's input is the first stage's output.
    // That boundary is what a body with two key stages lost — the second stage
    // used to read the *composed* row as if it were its own input.
    let users: Vec<String> = ["id", "name", "age", "active"].map(str::to_string).to_vec();
    let renamed: Vec<String> = ["u_id", "u_name", "u_age", "u_active"]
        .map(str::to_string)
        .to_vec();
    let suffixed: Vec<String> = ["u_id_v2", "u_name_v2", "u_age_v2", "u_active_v2"]
        .map(str::to_string)
        .to_vec();
    let nodes = row_stage_nodes("q = users & prefix \"u_\" & suffix \"_v2\"\n", "q");
    assert_eq!(nodes.len(), 2, "{nodes:?}");
    assert_eq!(nodes[0].0, RowStage::Prefix);
    assert_eq!(nodes[0].1.as_ref(), Some(&users));
    assert_eq!(nodes[0].2.as_ref(), Some(&renamed));
    // The second stage reads exactly what the first one produced, and its own
    // output is the row the later stages see.
    assert_eq!(nodes[1].1, nodes[0].2);
    assert_eq!(nodes[1].0, RowStage::Suffix);
    assert_eq!(nodes[1].2.as_ref(), Some(&suffixed));
}

#[test]
fn two_deferred_key_stages_record_two_nodes() {
    // The reported program: two key stages whose affix is a parameter. Each
    // application owns a node, and the helper's rows stay open (they are its
    // parameter), which is exactly why the chain is instantiated at the use.
    let nodes = row_stage_nodes("two = p => q => q & prefix p & prefix p\n", "two");
    assert_eq!(nodes.len(), 2, "{nodes:?}");
    assert!(
        nodes.iter().all(|(kind, ..)| *kind == RowStage::Prefix),
        "{nodes:?}"
    );
    assert_eq!(nodes[0].1, None, "a helper's input row is its parameter");
    assert_eq!(
        nodes[1].1, nodes[0].2,
        "the second stage reads the first's output"
    );
}

#[test]
fn an_omit_node_records_the_removed_column() {
    let users: Vec<String> = ["id", "name", "age", "active"].map(str::to_string).to_vec();
    let kept: Vec<String> = ["id", "name", "active"].map(str::to_string).to_vec();
    let nodes = row_stage_nodes("q = users & omit \"age\"\n", "q");
    assert_eq!(nodes.len(), 1, "{nodes:?}");
    assert_eq!(nodes[0].0, RowStage::Omit);
    assert_eq!(nodes[0].1.as_ref(), Some(&users));
    assert_eq!(nodes[0].2.as_ref(), Some(&kept));
}

#[test]
fn a_deferred_wrapper_travels_in_the_scheme() {
    // `mapValue`'s witness is a label in its type now, so a helper whose
    // wrapper is a parameter checks — the rule travels in the definition's
    // scheme and each use instantiates it with its own wrapper. Before the
    // witness was in the type this was impossible: the wrapper was read at the
    // application, so a helper's parameter had nothing to bind.
    let src = "mv = w => q => q & mapValue w\n\
               a = users & mv \"nullable\"\n\
               b = users & mv \"list\"\n";
    assert_eq!(
        row_of(src, "a"),
        vec![
            "id = maybe int",
            "name = maybe string",
            "age = maybe int",
            "active = maybe bool"
        ]
    );
    // The same helper at another wrapper gives that wrapper's row, with no
    // trace of the first use.
    assert_eq!(
        row_of(src, "b"),
        vec![
            "id = list int",
            "name = list string",
            "age = list int",
            "active = list bool"
        ]
    );
    // An unknown wrapper is still the stage's own error, reported at the use.
    let e = err(
        "mv = w => q => q & mapValue w\nmain = users & mv \"nope\"\n",
        "main",
    );
    assert!(e.contains("nope"), "{e}");
}

#[test]
fn a_map_value_node_records_the_wrapped_row() {
    let users: Vec<String> = ["id", "name", "age", "active"].map(str::to_string).to_vec();
    let nodes = row_stage_nodes("q = users & mapValue \"nullable\"\n", "q");
    assert_eq!(nodes.len(), 1, "{nodes:?}");
    assert_eq!(nodes[0].0, RowStage::MapValue);
    assert_eq!(nodes[0].1.as_ref(), Some(&users));
    assert_eq!(nodes[0].2.as_ref(), Some(&users));
}

#[test]
fn two_key_stages_with_a_deferred_affix() {
    // The two programs the redesign exists for: a body that renames twice where
    // the first affix is a helper's parameter. `prefix`'s row rule belongs to
    // `prefix`'s own definition and is instantiated once per use, so the second
    // stage consumes the *first* stage's output instead of being recovered from
    // the composed result type.
    //
    // Reading that composed `keyMap (m, keyMap (m, r))` as one layer over `r`
    // — which is what a use site used to do — asked for `keyMap (m, r) ~ r`,
    // and the occurs check refused it.
    assert_eq!(
        row_of(
            "two = p => q => q & prefix p & prefix p\n\
             main = users & two \"b_\"\n",
            "main"
        ),
        vec![
            "b_b_id = int",
            "b_b_name = string",
            "b_b_age = int",
            "b_b_active = bool"
        ]
    );
    // A deferred stage followed by a literal one keeps *both* renames, in order.
    assert_eq!(
        row_of(
            "two = p => q => q & prefix p & prefix \"z_\"\n\
             main = users & two \"b_\"\n",
            "main"
        ),
        vec![
            "z_b_id = int",
            "z_b_name = string",
            "z_b_age = int",
            "z_b_active = bool"
        ]
    );
    // The helper's own type names both mappers, so the composition is visible
    // before any affix is known.
    let t = ty("two = p => q => q & prefix p & prefix p\n", "two");
    assert_eq!(t.matches("keyMap").count(), 2, "{t}");
}

#[test]
fn three_key_stages_compose_in_order() {
    assert_eq!(
        row_of(
            "three = p => q => q & prefix p & suffix \"_v2\" & prefix \"x_\"\n\
             main = users & three \"b_\"\n",
            "main"
        ),
        vec![
            "x_b_id_v2 = int",
            "x_b_name_v2 = string",
            "x_b_age_v2 = int",
            "x_b_active_v2 = bool"
        ]
    );
}

#[test]
fn one_helper_two_affixes_have_independent_rows() {
    // Each use instantiates the stage's rule with its own variables, so two
    // uses of one helper at two literal affixes cannot share a row.
    let src = "two = p => q => q & prefix p & prefix p\n\
               a = users & two \"b_\"\n\
               b = users & two \"c_\"\n";
    assert_eq!(
        row_of(src, "a"),
        vec![
            "b_b_id = int",
            "b_b_name = string",
            "b_b_age = int",
            "b_b_active = bool"
        ]
    );
    assert_eq!(
        row_of(src, "b"),
        vec![
            "c_c_id = int",
            "c_c_name = string",
            "c_c_age = int",
            "c_c_active = bool"
        ]
    );
}

#[test]
fn mixed_prefix_and_suffix_through_a_helper() {
    let src = "mix = p => q => q & prefix p & suffix \"_v2\"\n\
               a = users & mix \"u_\"\n\
               b = users & mix \"c_\"\n";
    assert_eq!(
        row_of(src, "a"),
        vec![
            "u_id_v2 = int",
            "u_name_v2 = string",
            "u_age_v2 = int",
            "u_active_v2 = bool"
        ]
    );
    assert_eq!(
        row_of(src, "b"),
        vec![
            "c_id_v2 = int",
            "c_name_v2 = string",
            "c_age_v2 = int",
            "c_active_v2 = bool"
        ]
    );
}

#[test]
fn a_projection_after_a_deferred_rename() {
    assert_eq!(
        row_of(
            "two = p => q => q & prefix p & prefix p\n\
             main = users & two \"b_\" & select { .b_b_id, .b_b_name }\n",
            "main"
        ),
        vec!["b_b_id = int", "b_b_name = string"]
    );
    // The un-renamed name is gone: the row the stage produced is the one later
    // stages see.
    let e = err("main = users & prefix \"b_\" & select { .id }\n", "main");
    assert!(e.contains("no column `id`"), "{e}");
}

#[test]
fn a_key_stage_rule_travels_in_the_scheme() {
    // `omit`'s rule is its own definition's, so a helper whose key is a
    // parameter is checked: the label stays a type variable until the use site
    // binds it to a literal, and the row equation is instantiated there.
    let src = "drop = k => q => q & omit k\n\
               a = users & drop \"id\"\n\
               b = users & drop \"name\"\n";
    assert_eq!(
        row_of(src, "a"),
        vec!["name = string", "age = int", "active = bool"]
    );
    assert_eq!(
        row_of(src, "b"),
        vec!["id = int", "age = int", "active = bool"]
    );
    // A missing column is still the stage's own error, reported at the use.
    let e = err(
        "drop = k => q => q & omit k\nmain = users & drop \"nope\"\n",
        "main",
    );
    assert!(e.contains("no column `nope`"), "{e}");
    // And a *computed* key still cannot name a column.
    let e = err(
        "drop = k => q => q & omit k\nmain = users & drop .id\n",
        "main",
    );
    assert!(e.contains("literal"), "{e}");
}

#[test]
fn prefix_over_an_open_row_is_retained() {
    // The mapping is part of the type, so a helper whose affix is a parameter
    // type-checks: the term is retained until the argument settles.
    let t = ty("prefixAll = p => q => q & prefix p\n", "prefixAll");
    assert!(t.contains("keyMap"), "{t}");
    // And the row reduces once the affix is known at a use site.
    assert_eq!(
        row_of(
            "addPrefix = p => q => q & prefix p\nr = users & addPrefix \"u_\"\n",
            "r"
        ),
        vec![
            "u_id = int",
            "u_name = string",
            "u_age = int",
            "u_active = bool"
        ]
    );
}

#[test]
fn polymorphic_map_keys() {
    // A helper that adds a prefix, with the affix as a parameter.
    assert_eq!(
        row_of(
            "prefixAll = p => query => query & prefix p\n\
             q = users & prefixAll \"u_\"\n",
            "q"
        ),
        vec![
            "u_id = int",
            "u_name = string",
            "u_age = int",
            "u_active = bool"
        ]
    );
    // The suffix form renames at the end, not the start.
    assert_eq!(
        row_of(
            "suffixAll = s => query => query & suffix s\n\
             q = users & suffixAll \"_v2\"\n",
            "q"
        ),
        vec![
            "id_v2 = int",
            "name_v2 = string",
            "age_v2 = int",
            "active_v2 = bool"
        ]
    );
}

#[test]
fn prefix_needs_a_literal() {
    // The affix decides the output row's labels, so a *computed* one is
    // refused rather than deferred: `upper "u"` is not a literal, and the
    // stage cannot know what to rename to.
    let e = err("q = users & prefix (upper \"u\")\n", "q");
    assert!(e.contains("literal") || e.contains("string"), "{e}");
}

#[test]
fn map_value_wraps_types() {
    // AsNullable wraps each field type in maybe
    assert_eq!(
        ty("q = users & mapValue \"nullable\"\n", "q"),
        "query { id = maybe int, name = maybe string, age = maybe int, active = maybe bool }"
    );
    // AsList wraps each field type in list
    assert_eq!(
        ty("q = users & mapValue \"list\"\n", "q"),
        "query { id = list int, name = list string, age = list int, active = list bool }"
    );
    // Id leaves types unchanged
    assert_eq!(
        ty("q = users & mapValue \"id\"\n", "q"),
        "query { id = int, name = string, age = int, active = bool }"
    );
}

#[test]
fn map_value_errors() {
    let e = err("q = users & mapValue \"nope\"\n", "q");
    assert!(e.contains("nope"), "{e}");
    // The wrapper is read at the application, so it has to be a literal.
    let e = err("q = users & mapValue (upper \"n\")\n", "q");
    assert!(e.contains("literal"), "{e}");
}

#[test]
fn polymorphic_map_value() {
    // mapValue is also row-polymorphic through a helper.
    assert_eq!(
        ty(
            "nullable = q => q & mapValue \"nullable\"\n\
             q = users & nullable\n",
            "q"
        ),
        "query { id = maybe int, name = maybe string, age = maybe int, active = maybe bool }"
    );
}

#[test]
fn compose_polymorphic_helpers() {
    // Compose a key helper and a value helper.
    assert_eq!(
        ty(
                        "prefixAll = p => query => query & prefix p\n\
             nullable = query => query & mapValue \"nullable\"\n\
             q = users & prefixAll \"u_\" & nullable\n",
            "q"
        ),
        "query { u_id = maybe int, u_name = maybe string, u_age = maybe int, u_active = maybe bool }"
    );
}

#[test]
fn prefix_suffix_type_level() {
    // prefix adds a string to the beginning of each column name
    assert_eq!(
        ty("q = users & prefix \"u_\"\n", "q"),
        "query { u_id = int, u_name = string, u_age = int, u_active = bool }"
    );
    // suffix adds a string to the end of each column name
    assert_eq!(
        ty("q = users & suffix \"_old\"\n", "q"),
        "query { id_old = int, name_old = string, age_old = int, active_old = bool }"
    );
    // The mapping is visible in the type when the row is not reducible yet:
    // `keyMap (prefix "u_") r` is the row term.
    assert_eq!(
        ty("q = users & prefix \"u_\"\n", "q"),
        "query { u_id = int, u_name = string, u_age = int, u_active = bool }"
    );
}

#[test]
fn key_map_is_a_type_level_row_former() {
    assert_eq!(
        ty(
            "q : query (keyMap (prefix \"u_\") { id = int, name = string, age = int, active = bool }) = \
             users & prefix \"u_\"\n",
            "q"
        ),
        "query { u_id = int, u_name = string, u_age = int, u_active = bool }"
    );
    assert_eq!(
        ty(
            "q : query (keyMap (suffix \"_v2\") { id = int, name = string, age = int, active = bool }) = \
             users & suffix \"_v2\"\n",
            "q"
        ),
        "query { id_v2 = int, name_v2 = string, age_v2 = int, active_v2 = bool }"
    );
    // The signature's row names the rename's **input** row — `keyMap m r`
    // denotes `m` applied to `r` — so a one-column signature describes a
    // one-column *input*, not a one-column result. The pipeline here is handed
    // all four of `users`' columns, so the composed rename produces four
    // columns and this signature is rejected.
    //
    // The narrowing this test once asserted here is not a feature: it was the
    // comparison being skipped for a *chained* key stage. The form that really
    // narrows is a projection, `& select { id = .id }`, which produces the
    // one-column input the signature describes.
    let e = err(
        "q : query (keyMap (compose (prefix \"u_\") (suffix \"_v2\")) { id = int }) = \
         users & prefix \"u_\" & suffix \"_v2\"\n",
        "q",
    );
    assert!(e.contains("no column `u_name_v2`"), "{e}");
    assert_eq!(
        ty(
            "q : query (keyMap (compose (prefix \"u_\") (suffix \"_v2\")) { id = int }) = \
             users & select { id = .id } & prefix \"u_\" & suffix \"_v2\"\n",
            "q"
        ),
        "query { u_id_v2 = int }"
    );
}

#[test]
fn key_map_rejects_a_non_mapper() {
    let e = err(
        "q : query (keyMap (AsNullable) { id = int }) = users\n",
        "q",
    );
    assert!(
        e.contains("key mapper") || e.contains("KeyMap") || e.contains("kind"),
        "{e}"
    );
}

#[test]
fn directional_affix_witness_is_shared_with_result_mapper() {
    // The label parameter and the result row must mention the same mapper.
    // Otherwise a signature could claim that `prefix` changes a row with an
    // unrelated mapping witness.
    //
    // The witness is a label now, so the mismatch is between two label
    // variables: `symbol k` is the parameter's, while the result names
    // `prependSym m`. Tying them is what the `keyLabel` path does, so this
    // asserts on the specific disagreement rather than on any error at all.
    let e = err(
        "bad : symbol k -> query r -> query (keyMap (prependSym m) r) = prefix\n",
        "bad",
    );
    assert!(e.contains("type mismatch"), "{e}");
    assert!(e.contains("expected m") && e.contains("found k"), "{e}");
}

#[test]
fn deferred_mapkey_polymorphic_helpers() {
    // A helper that adds a prefix (row is polymorphic)
    assert_eq!(
        ty(
            "addPrefix = p => q => q & prefix p\nresult = users & addPrefix \"u_\"\n",
            "result"
        ),
        "query { u_id = int, u_name = string, u_age = int, u_active = bool }"
    );

    // Helper function that adds a suffix
    assert_eq!(
        ty(
            "addSuffix = s => q => q & suffix s\nresult = users & addSuffix \"_v2\"\n",
            "result"
        ),
        "query { id_v2 = int, name_v2 = string, age_v2 = int, active_v2 = bool }"
    );

    // Composition through helpers
    assert_eq!(
        ty(
            "wrap = q => q & prefix \"tbl_\" & suffix \"_old\"\nresult = users & wrap\n",
            "result"
        ),
        "query { tbl_id_old = int, tbl_name_old = string, tbl_age_old = int, tbl_active_old = bool }"
    );
}

/// A key stage's rename must survive the stages around it.
///
/// `prefix`/`suffix` rename the row they are handed, and the rename is carried
/// as a `keyMap` term rather than a shape constraint. A `keyMap` term only
/// reduces once its inner row is closed, so a stage between the source and the
/// rename that narrows the row — `order`'s sort key, `where`'s predicate — must
/// still leave the term reducible by the time the row is used. It did not: the
/// rename was lost, so the columns the `&=` operator could see were the
/// *pre-rename* ones and every renamed column was reported missing.
#[test]
fn a_key_stage_survives_the_stages_around_it() {
    // Bare rename, and via a stage that does not constrain the row.
    assert!(ty("q = users & prefix \"lit_\"\n", "q").contains("lit_id"));
    assert!(ty("q = users & limit 1 & prefix \"lit_\"\n", "q").contains("lit_id"));

    // `order` constrains the row through its sort key; `where` through its
    // predicate. The rename must still land on the rows they produce.
    for stages in [
        "& order [asc .id] ",
        "& where (.age >= 1) ",
        "& order [asc .id] & limit 1 ",
    ] {
        let src = format!("q = users {stages}& prefix \"lit_\"\n");
        assert!(
            ty(&src, "q").contains("lit_id"),
            "the rename was dropped after {stages:?}: {}",
            ty(&src, "q")
        );
    }

    // The renamed columns must be *usable*, not merely present in a printed
    // type: reading one back through `&=` is what completion does.
    assert_eq!(
        ty(
            "q = users & order [asc .id] & prefix \"lit_\" &= {x = .lit_id}\n",
            "q"
        ),
        "query { x = int }"
    );
    assert_eq!(
        ty(
            "q = users & where (.age >= 1) & prefix \"lit_\" &= {x = .lit_name}\n",
            "q"
        ),
        "query { x = string }"
    );
}

/// A signature that names too few columns says so.
///
/// The two ways a signature's row can disagree with the body's used to read the
/// same. Naming a column the body lacks is `no column `x``, which is right, but
/// the mirror case — the body having a column the signature does not — was
/// reported the same way, over the *signature's* columns. That named a column
/// that does exist and listed the very set that was short, so a signature that
/// had simply forgotten a column read as a complaint about the query.
///
/// A signature does not *narrow* a row; it has to match it. Narrowing is a
/// projection, `& select { … }` or `&= { … }`, which is an operation on the row
/// rather than a claim about it. A key stage's rename is not an exception: it
/// renames whatever row reaches it, so a one-column signature over a four-column
/// pipeline is missing columns like any other.
#[test]
fn a_signature_that_is_missing_a_column_says_so() {
    let msg = err("q : query { id = int } = users\n", "q");
    assert!(msg.contains("signature is missing column `name`"), "{msg}");
    // It still lists what the body produced, which is what to add.
    assert!(
        msg.contains("the query has: id, name, age, active"),
        "{msg}"
    );

    // The opposite mistake keeps its own wording: the column really is absent.
    let msg = err(
        "q : query { id = int, name = string, age = int, active = bool, oops = int } = users\n",
        "q",
    );
    assert!(msg.contains("no column `oops`"), "{msg}");
    assert!(msg.contains("available: id, name, age, active"), "{msg}");

    // The same rule holds across a key stage: `prefix`/`suffix` rename every
    // column they are handed, so the pipeline below still produces four, and a
    // one-column signature is missing three of them. This once read as
    // accepted — not because narrowing was allowed there, but because a
    // *chained* key stage left the term looking unreduced and the comparison
    // was skipped, which also let a nonexistent column through.
    let msg = err(
        "q : query { u_id_v2 = int } = users & prefix \"u_\" & suffix \"_v2\"\n",
        "q",
    );
    assert!(msg.contains("no column `u_name_v2`"), "{msg}");

    // The projection is what narrows, and then the signature matches.
    assert_eq!(
        ty(
            "q : query { u_id_v2 = int } = users & select { id = .id } & prefix \"u_\" & suffix \"_v2\"\n",
            "q"
        ),
        "query { u_id_v2 = int }"
    );
}

/// A chained key stage is checked like any other row.
///
/// A key stage's result is a `keyMap` term, and a *chained* one nests:
/// `keyMap (suffix "_v2") (keyMap (prefix "u_") users)`. Deciding whether that
/// term was reducible followed only a top-level variable, so the nested inner
/// row still looked open, the term was judged "not reducible yet", and
/// `unify_inner` treats that as satisfied-and-pending — the signature was never
/// compared. One key stage checked correctly, two silently did not, so a
/// signature naming a column no stage produces was accepted.
#[test]
fn a_chained_key_stage_is_still_checked_against_its_signature() {
    // A column the pipeline never produces. A single stage rejects it (and
    // still does, below); chaining must not make it acceptable.
    let e = err(
        "q : query { bogus = int } = users & prefix \"u_\" & suffix \"_v2\"\n",
        "q",
    );
    assert!(e.contains("no column `u_id_v2`"), "{e}");
    let e = err(
        "q : query { bogus = int } = users & suffix \"_v2\" & prefix \"u_\"\n",
        "q",
    );
    assert!(e.contains("no column `u_id_v2`"), "{e}");

    // A wrong column type is caught too, rather than accepted silently.
    let e = err(
        "q : query { u_id_v2 = string } = users & prefix \"u_\" & suffix \"_v2\"\n",
        "q",
    );
    assert!(e.contains("expected string, found int"), "{e}");

    // Three stages chain the same way.
    let e = err(
        "q : query { bogus = int } = users & prefix \"u_\" & suffix \"_v2\" & suffix \"_v3\"\n",
        "q",
    );
    assert!(e.contains("no column `u_id_v2_v3`"), "{e}");

    // The single-stage behavior is unchanged, and the honest signature over a
    // chained rename still matches.
    assert!(
        err("q : query { bogus = int } = users & prefix \"u_\"\n", "q")
            .contains("no column `u_id`")
    );
    assert_eq!(
        ty(
            "q : query { u_id_v2 = int, u_name_v2 = string, u_age_v2 = int, u_active_v2 = bool } = \
             users & prefix \"u_\" & suffix \"_v2\"\n",
            "q"
        ),
        "query { u_id_v2 = int, u_name_v2 = string, u_age_v2 = int, u_active_v2 = bool }"
    );
}

#[test]
fn kind_checking_basic() {
    // `prefix` and `suffix` share a shape, so the direction has to come from
    // the parameter's own type. Both must be present and distinct in the
    // prelude, or `suffix` silently prepends.
    assert!(ty("q = users & prefix \"u_\"\n", "q").contains("u_id"));
    assert!(ty("q = users & suffix \"_v2\"\n", "q").contains("id_v2"));
}

#[test]
fn merge_basic() {
    // merge combines two rows: names in both take the right's type, names only
    // in the right are appended.
    let _ = ty("q = users & merge orders\n", "q");
}

/// Whether `name` is a program the whole pipeline accepts: the checker takes
/// it and it compiles. `Err` is whatever rejected it.
fn compiles(src: &str, name: &str) -> Result<(), String> {
    let ws = Workspace::from_source(&format!("{TABLES}{src}"));
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let tc = check(&ws);
    match crate::root_queries_checked(&ws, &tc)
        .into_iter()
        .find(|(n, _)| n == name)
    {
        Some((_, Ok(_))) => Ok(()),
        Some((_, Err(e))) => Err(e.message.clone()),
        None => Err(format!("`{name}` was not compiled")),
    }
}

/// A stage that *adds* a column is visible to the next stage of the same
/// helper.
///
/// Each stage of a helper is deferred: the helper's parameter is an open row
/// until the definition is applied to a query, so `q & update { up1 = … }`
/// types its result as the symbolic row `merge α { up1 = int }`. The next
/// stage's input is that term, and its columns have not been computed yet.
///
/// Every stage that reads its input's columns used to ask only "is this a bare
/// variable?", so an unreduced `merge` was read as a *finished* row and the
/// column the merge was about to contribute was reported missing. Only
/// `select` asked the right question, which is why the same pipeline worked
/// with `&=` and failed with `&+`, `&?`, `&.`, joins and `omit`. Every case
/// below was a rejection before the fix.
#[test]
fn a_stage_sees_the_column_the_previous_stage_added() {
    for (helper, expected) in [
        // The added column is read by the next `update`.
        (
            "h = q => q & update { up1 = .id } & update { up2 = .up1 }\n",
            "query { id = int, name = string, age = int, active = bool, up1 = int, up2 = int }",
        ),
        // ... by a `where`, and by an `order`.
        (
            "h = q => q & update { up1 = .id } & where (.up1 > 0)\n",
            "query { up1 = int | a }",
        ),
        (
            "h = q => q & update { up1 = .id } & order [asc .up1]\n",
            "query { up1 = a | b }",
        ),
        // ... and as a join's left key.
        (
            "h = q => q & update { up1 = .id } & leftJoin orders (.<up1 == .>user_id)\n",
            "query { id = int, name = string, age = int, active = bool, up1 = int, \
             user_id = maybe int, amount = maybe float, status = maybe string }",
        ),
        // `omit` names the added column — a row equation, so before the fix the
        // equation met the unfinished `merge` term and the key looked absent.
        (
            "h = q => q & update { up1 = .id } & omit \"up1\"\n",
            "query { id = int, name = string, age = int, active = bool }",
        ),
        // A following `select` reads it too. This spelling worked before, and
        // must keep working.
        (
            "h = q => q & update { up1 = .id } & select {.up1}\n",
            "query { up1 = int }",
        ),
    ] {
        let src = format!("{helper}q = users & h\n");
        assert_eq!(ty(&src, "q"), expected, "{helper}");
        assert!(
            compiles(&src, "q").is_ok(),
            "{helper}: {:?}",
            compiles(&src, "q")
        );
    }

    // A helper whose row is still open ends its type in an open tail (`| a`),
    // exactly as the pre-existing `keyMap` helper
    // `suffix \"_a\" & where (.id_a > 0)` does. That is the honest general
    // form: the row the helper is handed is what completes it.
    assert_eq!(
        ty(
            "h = q => q & suffix \"_a\" & where (.id_a > 0)\nq = users & h\n",
            "q"
        ),
        "query { id_a = int | a }"
    );
}

/// The fix must not *accept* a column that no stage produces.
///
/// The deferral above is only sound because the equation stays pending: once
/// the helper is applied, the row is closed and a name nothing contributes is
/// still rejected. Reading the unfinished `merge` as final was wrong; so would
/// be skipping the comparison entirely.
///
/// Which phase reports it depends on the stage. An `update` field, an `omit`
/// key, a join predicate and a `select` field are checked against the row the
/// helper closes to, so the checker rejects them. A `where` condition and an
/// `order` key keep their `Within`/`Filter` equation pending with an open tail
/// — the same shape the `keyMap` helper above has — so the *compiler* is what
/// reports the absent column, and the message still names it.
#[test]
fn a_stage_still_rejects_a_column_no_stage_adds() {
    for helper in [
        "h = q => q & update { up1 = .id } & update { up2 = .nope }\n",
        "h = q => q & update { up1 = .id } & where (.nope > 0)\n",
        "h = q => q & update { up1 = .id } & order [asc .nope]\n",
        "h = q => q & update { up1 = .id } & leftJoin orders (.<nope == .>user_id)\n",
        "h = q => q & update { up1 = .id } & omit \"nope\"\n",
        "h = q => q & update { up1 = .id } & select {.nope}\n",
    ] {
        let src = format!("{helper}q = users & h\n");
        // However the program is stopped — by the checker or by the compiler —
        // it is stopped, and the message names the column.
        let why = ty_err(&src, "q").unwrap_or_else(|| {
            compiles(&src, "q").expect_err("a column no stage adds must not compile")
        });
        assert!(why.contains("no column `nope`"), "{helper}: {why}");
    }
}

/// Diagnostics are rendered once per module text, inside the memoized check, so
/// a rendered message must still follow an edit to that text.
#[test]
fn a_rendered_diagnostic_follows_an_edit() {
    let src = "q : query { a = int } = table \"s\" \"t\" & where \"x\"\n";
    let mut ws = Workspace::from_source(src);
    let before = check(&ws);
    assert_eq!(before.errors().len(), 1, "{:?}", before.errors());
    assert_eq!(before.errors()[0].diag.line, 1);
    assert!(
        before.errors()[0].diag.source.contains("where \"x\""),
        "{:?}",
        before.errors()[0]
    );

    assert!(ws.set_source(ws.root, format!("# a comment line\n{src}")));
    let after = check(&ws);
    assert_eq!(after.errors().len(), 1, "{:?}", after.errors());
    assert_eq!(after.errors()[0].diag.line, 2, "{:?}", after.errors()[0]);
    assert!(
        after.errors()[0].diag.source.contains("where \"x\""),
        "{:?}",
        after.errors()[0]
    );
}

/// Checking mode pushes an expected type **down**; synthesis recovers it by
/// unifying afterwards.
///
/// # What these tests do and do not establish
///
/// They pin the *observable behavior* of the checker at the forms where an
/// expectation is available: a lambda parameter, a record field, a list
/// element, a `case` arm. That behavior is what the language promises, and it
/// is worth locking down.
///
/// They do **not** establish that the checking overrides are *necessary*.
/// Measured, not assumed: with `check_record`, `check_lambda` and `check_list`
/// all disabled so that every expression took the synthesize-then-coerce path,
/// this entire suite still passed. The existing constraint solver already
/// handles every case covered below — its `solve` worklist defers a comparison
/// and re-runs it, which recovers by scheduling what checking recovers by
/// direction.
///
/// So the overrides are a *replacement for a workaround*, not a fix for a
/// failing test, and Phase 1 of the consolidation is not yet earning its
/// complexity here. The direction becomes load-bearing when the deferral
/// protocol is removed: a case that currently waits for `solve` to re-run has
/// nowhere to wait once the expectation arrives up front. Deleting the
/// `Ok(false)` paths without first having this direction in place is where the
/// `5d34464` class of bug would return.
mod bidirectional {
    use super::*;

    /// A lambda's parameter type comes from the expected function type.
    ///
    /// `>>>` is `(a -> b) -> (b -> c) -> a -> c`, so both operands are checked
    /// against a *known* function type. Before checking mode, `infer_lambda`
    /// bound the parameter to a fresh variable and the body decided the type:
    /// sound, but it meant the expectation contributed nothing, and an
    /// annotated helper was the workaround when the body could not determine
    /// the parameter on its own.
    #[test]
    fn a_lambda_parameter_comes_from_the_expected_type() {
        // The inline lambda is checked against `expr r string -> expr r string`
        // from `>>>`'s signature, so `.name` is known to be a string column and
        // the whole expression is a string — not an unsolved variable.
        assert_eq!(
            ty("q = users & select { x = (f => upper f) .name }\n", "q"),
            "query { x = string }"
        );
        // The same shape at another column type must select that type, which is
        // only possible if the expectation reached the parameter.
        assert_eq!(
            ty("q = users & select { x = (f => f + 1) .age }\n", "q"),
            "query { x = int }"
        );
    }

    /// A record literal's field types come from the expected row.
    ///
    /// A stage's record argument is checked against the row that stage
    /// declares, so a field that is wrong is reported against that field rather
    /// than against the whole record.
    #[test]
    fn a_record_field_is_checked_against_the_expected_row() {
        // `ifThenElse` takes `expr r a` in both branches, so both are checked
        // at one type. A mismatched branch is a type error, not an unsolved
        // variable that later becomes one.
        assert_eq!(
            ty(
                "q = users & select { x = ifThenElse .active \"y\" \"n\" }\n",
                "q"
            ),
            "query { x = string }"
        );
        assert_eq!(
            ty("q = users & select { x = ifThenElse .active 1 2 }\n", "q"),
            "query { x = int }"
        );
    }

    /// A `case`'s conditions and values are checked against the types the form
    /// fixes in advance, so both directions agree.
    #[test]
    fn a_case_arm_is_checked_against_the_expected_type() {
        assert_eq!(
            ty(
                "q = users & select { x = case { .age > 18 => \"adult\"; true => \"minor\" } }\n",
                "q"
            ),
            "query { x = string }"
        );
        // Every value arm must be the *same* type, which is what checking all
        // of them against one expected type enforces.
        assert!(
            ty_err(
                "q = users & select { x = case { .age > 18 => 1; true => \"minor\" } }\n",
                "q"
            )
            .is_some(),
            "arms of different types must be rejected"
        );
    }

    /// A list literal's element type comes from the expected `list t`.
    ///
    /// `inList : list a -> expr r a -> expr r bool`, so once the second
    /// argument fixes `a` the first is checked against a *known* `list a`.
    /// Checking the list downwards is what lets the literal be typed by that
    /// element type rather than by a fresh variable the second argument later
    /// solves.
    #[test]
    fn a_list_is_checked_against_its_expected_element_type() {
        assert_eq!(
            ty("q = users & where (inList [\"a\", \"b\"] .name)\n", "q"),
            "query { name = string, id = int, age = int, active = bool }"
        );
        // Elements of the wrong type are rejected against the element type the
        // second argument established.
        assert!(
            ty_err("q = users & where (inList [1, 2] .name)\n", "q").is_some(),
            "an int list against a string column must be rejected"
        );
    }

    /// A duplicate record field is reported on both the checked and the
    /// synthesized path.
    ///
    /// The rule lives in `infer_record`; checking mode visits the fields
    /// itself, so it has to apply the same rule. Reporting it in only one
    /// direction would make the diagnostic depend on which way the type flowed.
    #[test]
    fn a_duplicate_record_field_is_reported_when_checked_too() {
        assert!(
            ty_err(
                "q = users & select { x = ifThenElse .active \"a\" \"a\" }\n",
                "q"
            )
            .is_none(),
            "a well-formed case must still check"
        );
        // Two different fields with the same name in one record literal.
        assert!(
            ty_err(
                "h : expr r (row { a = string }) -> expr r string = sql \"$1\"\n\
                 q = users & select { x = h { a = \"1\", a = \"2\" } }\n",
                "q"
            )
            .is_some(),
            "a duplicate field must be reported"
        );
    }
}

/// Every expression node is typed **completely**, rows included.
///
/// `use_ty` answers "what scalar is this node", which by construction answers
/// `None` for a query: it filters to `Scalar`/`Open` and drops everything else.
/// The checker does record the whole type per `ExprId`, but it stored
/// `self.resolve(&t)` — a *shallow* resolve that chases a top-level variable and
/// stops. `query r` therefore came back with `r` still a variable, so the row
/// was recorded and unreachable at the same time: `use_types` printed
/// `query { id = int }` while the structured read said `None`.
///
/// That is what forced a consumer to recover a `table`'s columns from the
/// *declaring definition's* signature — a second reading of a fact the node
/// already carried. This asserts the node answers for itself, on the shapes
/// where the old shallow resolve gave nothing: a table, a stage applied to one,
/// a projection that changes the row, and a join whose row is the merge of two.
#[test]
fn every_node_records_its_whole_type() {
    let ws = Workspace::from_source(
        "users : query { id = int, name = string } = table \"s\" \"users\"\n\
         orders : query { id = int, uid = int } = table \"s\" \"orders\"\n\
         q = users & where (.id > 1) & select { .name }\n\
         r = users & innerJoin orders (.<id == .>uid)\n",
    );
    let tc = check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());

    // The row each node denotes, found by its source text so the assertion names
    // the construct rather than an opaque id.
    let text = &ws.modules[ws.root].text;
    let fields_of = |needle: &str| -> Option<Vec<(String, crate::core::ScalarType)>> {
        tc.use_ids(ws.root).into_iter().find_map(|id| {
            let span = ws.modules[ws.root]
                .module
                .defs
                .iter()
                .flat_map(|d| all_exprs(&d.body))
                .find(|e| {
                    e.id == id
                        && text
                            .get(e.span.start as usize..e.span.end as usize)
                            .is_some_and(|s| s == needle)
                })
                .map(|e| e.span)?;
            let _ = span;
            match tc.use_scheme(ws.root, id)? {
                crate::check::SchemeView::Query { fields } => fields,
                _ => None,
            }
        })
    };

    let names = |fs: &[(String, crate::core::ScalarType)]| {
        fs.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>()
    };

    for (src, want) in [
        ("table \"s\" \"users\"", vec!["id", "name"]),
        ("table \"s\" \"orders\"", vec!["id", "uid"]),
        ("users & where (.id > 1)", vec!["id", "name"]),
        ("users & where (.id > 1) & select { .name }", vec!["name"]),
        (
            "users & innerJoin orders (.<id == .>uid)",
            vec!["id", "name", "uid"],
        ),
    ] {
        let got = fields_of(src).unwrap_or_else(|| {
            panic!("`{src}` recorded no query row; a node that carries a row must expose it")
        });
        assert_eq!(names(&got), want, "`{src}` recorded the wrong row: {got:?}");
    }
}

/// Every expression in a body, for a test locating a node by its source text.
fn all_exprs(e: &cagara_syntax::ast::Expr) -> Vec<&cagara_syntax::ast::Expr> {
    use cagara_syntax::ast::ExprKind as K;
    let mut out = vec![e];
    match &e.kind {
        K::App(f, args) => {
            out.extend(all_exprs(f));
            for a in args {
                out.extend(all_exprs(a));
            }
        }
        K::Record(fs) => {
            for (_, v) in fs {
                out.extend(all_exprs(v));
            }
        }
        K::Lambda(_, b) => out.extend(all_exprs(b)),
        K::List(xs) => {
            for x in xs {
                out.extend(all_exprs(x));
            }
        }
        K::Case(arms) => {
            for (c, v) in arms {
                out.extend(all_exprs(c));
                out.extend(all_exprs(v));
            }
        }
        K::Proj(b, _) => out.extend(all_exprs(b)),
        _ => {}
    }
    out
}

/// The checker records **what each name resolved to**, not only its type.
///
/// This is the fact normalization needs and could not previously get: `use_ty`
/// says an expression is a `string`, but not *which* definition produced it.
/// Without the resolution, any phase expanding a call has to look the name up
/// again — a second answer waiting to disagree with the checker's.
#[test]
fn the_checker_records_what_each_name_resolved_to() {
    let ws = Workspace::from_source(&format!(
        "{TABLES}up = upper\nq = users & select {{ x = up .name }}\n"
    ));
    let tc = check(&ws);
    // `up` in `up .name` is a name occurrence, so it has a resolution.
    let ids = tc.resolved_ids(ws.root);
    assert!(!ids.is_empty(), "no resolutions were recorded");
    // Every resolved occurrence must name a real definition in this module.
    let defs = &ws.modules[ws.root].module.defs;
    for id in &ids {
        match tc.resolution(ws.root, *id).expect("listed but absent") {
            crate::normalize::Resolved::Def((m, d)) => {
                assert!(
                    *d < ws.modules[*m].module.defs.len(),
                    "resolution points at a definition that does not exist"
                );
            }
            crate::normalize::Resolved::Overloads { module, defs: ds } => {
                assert!(!ds.is_empty(), "an overload resolution with no candidates");
                for d in ds {
                    assert!(*d < ws.modules[*module].module.defs.len());
                }
            }
            crate::normalize::Resolved::Primitive => {}
        }
    }
    // `upper` is reached through the alias `up`, and the resolution must name
    // the *definition the name was written as* — following the alias is
    // normalization's job, not the checker's.
    let up = defs.iter().position(|d| d.name == "up").unwrap();
    assert!(
        ids.iter().any(|id| matches!(
            tc.resolution(ws.root, *id),
            Some(crate::normalize::Resolved::Def((_, d))) if *d == up
        )),
        "the alias `up` was not recorded as the resolution of its own use"
    );
}

/// The resolution covers the cases that produced the alias and computed-head
/// bugs, including the shapes where a name is used *as a value* rather than
/// applied directly.
///
/// `07a7e12` and `3624dfc` were each about a call the checker accepted and the
/// elaborator refused, and each was a *different spelling* of the same program.
/// The resolution exists so the elaborator never has to re-derive what a name
/// meant; these are the spellings that must therefore be recorded.
///
/// A resolution may point into another module: `upper` is the prelude's, and
/// the occurrence is the user's. That is the point — the location of the *use*
/// and the location of the *definition* are independent, which is exactly what
/// a name-based second lookup in the elaborator could get wrong.
#[test]
fn resolution_covers_the_shapes_that_produced_the_alias_bugs() {
    for (what, src, want) in [
        // A direct call into the prelude.
        (
            "direct",
            "q = users & select { x = upper .name }\n",
            "upper",
        ),
        // A bare-name alias, applied. `f = upper` then `f .name`.
        (
            "alias",
            "f = upper\nq = users & select { x = f .name }\n",
            "f",
        ),
        // An alias of an alias.
        (
            "alias chain",
            "f = upper\ng = f\nq = users & select { x = g .name }\n",
            "g",
        ),
        // A name used as a *value*, passed to a composition rather than
        // applied. This is the path that failed differently from a direct call.
        (
            "value in composition",
            "p = trim >>> upper\nq = users & select { x = p .name }\n",
            "p",
        ),
        // An alias used as a value inside a composition.
        (
            "alias as value",
            "up = upper\np = trim >>> up\nq = users & select { x = p .name }\n",
            "up",
        ),
        // A parameter holding a function, applied through the body.
        (
            "function parameter",
            "app = g => (g .name)\nq = users & select { x = app upper }\n",
            "app",
        ),
    ] {
        let ws = Workspace::from_source(&format!("{TABLES}{src}"));
        let tc = check(&ws);
        assert!(tc.errors().is_empty(), "{what}: {:?}", tc.errors());
        // Look the wanted name up in *every* module: it may be the prelude's.
        let want_index = (0..ws.modules.len()).find_map(|m| {
            ws.modules[m]
                .module
                .defs
                .iter()
                .position(|d| d.name == want)
                .map(|i| (m, i))
        });
        let (want_module, want_def) =
            want_index.unwrap_or_else(|| panic!("{what}: no definition `{want}` anywhere"));
        let found = tc.resolved_ids(ws.root).iter().any(|id| {
            matches!(
                tc.resolution(ws.root, *id),
                Some(crate::normalize::Resolved::Def((m, d)))
                    if (*m, *d) == (want_module, want_def)
            )
        });
        assert!(
            found,
            "{what}: the use of `{want}` (module {want_module}, def {want_def}) was not \
             recorded as a resolution; the elaborator would have to look it up again"
        );
    }
}

/// The checker records a type for an **application** node, not only for its
/// head and arguments.
///
/// Found by the normalization migration: `infer_app` returned the function
/// result type but never pushed the application's own `ExprId` into `uses`, so
/// `use_ty(app_id)` had no answer. A consumer that builds a node from the
/// application — which is every consumer that wants the type of `upper .name`
/// rather than of `upper` — had nothing to read.
#[test]
fn an_application_records_its_own_type() {
    let ws = Workspace::from_source(&format!(
        "{TABLES}q = users & select {{ x = upper .name }}\n"
    ));
    let tc = check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    // Find the `upper .name` application's `ExprId` and ask for its type.
    // By *name*, not by index: `TABLES` already defines `users`, so position 1
    // is a table definition and its only application is the `table` call.
    let def = ws.modules[ws.root]
        .module
        .defs
        .iter()
        .find(|d| d.name == "q")
        .expect("the test program defines `q`");
    let mut apps = Vec::new();
    fn walk<'a>(e: &'a cagara_syntax::ast::Expr, out: &mut Vec<&'a cagara_syntax::ast::Expr>) {
        use cagara_syntax::ast::ExprKind as K;
        match &e.kind {
            K::App(f, args) => {
                out.push(e);
                walk(f, out);
                for a in args.iter() {
                    walk(a, out);
                }
            }
            K::Record(fs) => {
                for (_, v) in fs {
                    walk(v, out);
                }
            }
            K::Proj(b, _) => walk(b, out),
            K::Lambda(_, b) => walk(b, out),
            _ => {}
        }
    }
    walk(&def.body, &mut apps);
    assert!(!apps.is_empty(), "no application found in the test program");
    // The walk collects every application, including the outer pipeline
    // (`users & select {…}`), whose type is a `query` and therefore not a
    // scalar. What must have a recorded type is the *inside* one — the
    // `upper .name` whose type is `expr r string`. Asserting on any
    // application at all would pass for the wrong reason.
    let scalars: Vec<(u32, crate::core::ScalarType)> = apps
        .iter()
        .filter_map(|a| tc.use_ty(ws.root, a.id).map(|t| (a.id, t)))
        .collect();
    assert!(
        !scalars.is_empty(),
        "no scalar application had a recorded type; apps={:?} all-use-ids={:?}",
        apps.iter().map(|a| a.id).collect::<Vec<_>>(),
        tc.use_ids(ws.root)
    );
    assert!(
        scalars
            .iter()
            .any(|(_, t)| *t == crate::core::ScalarType::String),
        "the `upper .name` application should be a string, got {scalars:?}"
    );
}

/// `ExprId`s are unique within a module, which is what makes `(module, ExprId)`
/// a sound key for `use_ty` and the resolution map.
///
/// Worth pinning because both maps depend on it and neither would fail
/// *loudly* if it stopped being true: a colliding id would silently return
/// another expression's type, which is the kind of wrong answer that looks
/// like a plausible one.
#[test]
fn exprids_are_unique_within_a_module() {
    let ws = Workspace::from_source(
        "a : query { x = int } = table \"s\" \"t1\"\n\
         b : query { y = float } = table \"s\" \"t2\"\n",
    );
    let tc = check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    // Every expression id the checker recorded, with the type it recorded.
    let ids = tc.use_ids(ws.root);
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len(), "use_ids returned a duplicate");
    // Two definitions in one module still number their expressions into one
    // sequence, so their applications have distinct ids.
    let mut app_ids: Vec<u32> = Vec::new();
    for d in &ws.modules[ws.root].module.defs {
        if let cagara_syntax::ast::ExprKind::App(_, _) = &d.body.kind {
            app_ids.push(d.body.id);
        }
    }
    assert_eq!(app_ids.len(), 2, "expected two applications: {app_ids:?}");
    assert_ne!(
        app_ids[0], app_ids[1],
        "two definitions in one module must not share an ExprId"
    );
}

/// A literal is polymorphic, so every fork of a `case` ends up with the `case`'s
/// own type.
///
/// `1` is not `int`. It is a value that takes whatever numeric type its context
/// demands, like Haskell's `fromInteger`; `"paid"` is the same for
/// `fromString`. So a `case` whose forks mention literals of different kinds is
/// not a type error — each literal is instantiated at the one type the `case`
/// settles on, and the `case` therefore does not claim a type that one of its
/// own forks lacks.
///
/// This is what made `case { c => 1; true => 2.0 }` look wrong: the `1` fork was
/// being *recorded* at its default kind `int` while the `case` resolved to
/// `float`. The fix is not to reject the program but to record the literal at
/// the type it was lifted to, which is what `check` now does for every checked
/// literal.
#[test]
fn a_literal_fork_takes_the_cases_type() {
    // `1` beside `2.0` is one numeric type, and the literal takes it.
    assert_eq!(
        ty(
            "q = users & select { x = case { .active => 1; true => 2.0 } }\n",
            "q"
        ),
        "query { x = float }"
    );
    // The same in the other order, and with the literal in the default arm.
    assert_eq!(
        ty(
            "q = users & select { x = case { .active => 2.0; true => 1 } }\n",
            "q"
        ),
        "query { x = float }"
    );
    assert_eq!(
        ty(
            "q = users & select { x = case { .active => 1.5; true => 0 } }\n",
            "q"
        ),
        "query { x = float }"
    );
    // Two integer literals stay integer.
    assert_eq!(
        ty(
            "q = users & select { x = case { .active => 1; true => 2 } }\n",
            "q"
        ),
        "query { x = int }"
    );
    // A string literal beside a `date` column is instantiated at `date`: the
    // `fromString` half of the same rule. `TABLES` has no date column, so the
    // table is declared here.
    assert_eq!(
        ty(
            "d : query { id = int, at = date } = table \"s\" \"d\"\n\
             q = d & select { x = ifThenElse (.id > 0) \"2020-01-01\" .at }\n",
            "q"
        ),
        "query { x = date }"
    );

    // `ifThenElse` is the same construct written as a definition, so it must
    // agree with `case` rather than differing — the rule lives in `check`, not
    // in the `case` form.
    assert_eq!(
        ty("q = users & select { x = ifThenElse .active 1 2.0 }\n", "q"),
        "query { x = float }"
    );
    assert_eq!(
        ty("q = users & select { x = ifThenElse .active 1 2 }\n", "q"),
        "query { x = int }"
    );

    // The *recorded* fork type is the lifted one, not the literal's default
    // kind. This is the fact the type above cannot show: the fork `1` must be
    // recorded as `float`, or the `case` is typed `float` while a fork inside it
    // says `int`.
    let ws = Workspace::from_source(&format!(
        "{TABLES}q = users & select {{ x = case {{ .active => 1; true => 2.0 }} }}\n"
    ));
    let tc = check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    let body = &ws.modules[ws.root].module.defs[2].body;
    let mut lits = Vec::new();
    collect_lits(body, &mut lits);
    let one = lits
        .iter()
        .find(|(_, text)| text.starts_with("Int("))
        .expect("the `1` fork");
    assert_eq!(
        format!("{:?}", tc.use_ty(ws.root, one.0)),
        "Some(Float)",
        "`1` in a float `case` is a float, not an int"
    );

    fn collect_lits(e: &cagara_syntax::ast::Expr, out: &mut Vec<(u32, String)>) {
        use cagara_syntax::ast::ExprKind as K;
        if let K::Lit(l) = &e.kind {
            out.push((e.id, format!("{l:?}")));
        }
        match &e.kind {
            K::Record(fs) => {
                for (_, v) in fs {
                    collect_lits(v, out);
                }
            }
            K::Case(arms) => {
                for (c, v) in arms {
                    collect_lits(c, out);
                    collect_lits(v, out);
                }
            }
            K::Proj(b, _) => collect_lits(b, out),
            K::App(f, a) => {
                collect_lits(f, out);
                for x in a.iter() {
                    collect_lits(x, out);
                }
            }
            K::Lambda(_, b) => collect_lits(b, out),
            _ => {}
        }
    }
}
