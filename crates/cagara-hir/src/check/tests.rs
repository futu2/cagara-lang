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
        assert_eq!(e.module, ws.root, "error outside the root module: {}", e.diag);
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
    let r = types(src).into_iter().find(|(n, _)| n == name).expect("no such definition").1;
    r.unwrap_or_else(|e| panic!("`{name}` failed: {e}"))
}

fn err(src: &str, name: &str) -> String {
    match types(src).into_iter().find(|(n, _)| n == name).expect("no such definition").1 {
        Ok(t) => panic!("`{name}` should fail but has type {t}"),
        Err(e) => e,
    }
}

#[test]
fn prelude_and_tables_check() {
    assert_eq!(ty("", "users"), "query { id = int, name = string, age = int, active = bool }");
}

#[test]
fn row_polymorphic_predicate() {
    // The literal stays polymorphic until the column type is known.
    assert_eq!(ty("adult = .age >= 18\n", "adult"), "expr { age = a | b } bool");
    assert_eq!(ty("named = .name == \"x\"\n", "named"), "expr { name = a | b } bool");
}

#[test]
fn aggregate_types_print_wrapped() {
    assert_eq!(ty("total = sum .amount\n", "total"), "agg (expr { amount = a | b } (maybe c))");
    assert_eq!(
        ty("q = orders & agg { t = sum .amount, a = avg .user_id }\n", "q"),
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
        ty("q = orders & agg { user_id = group .user_id, revenue = sum .amount, n = count }\n", "q"),
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
    assert_eq!(ty("q = orders & select { x = .amount * 2, one = 1 }\n", "q"), "query { x = float, one = int }");
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
    assert!(err("q = users & select { x = .name <> 1 }\n", "q").contains("expected string, found int"));
    assert!(err("q = users & limit .id\n", "q").contains("type mismatch"));
    let e = err("f = x => { a = x <> \"!\", b = x + 1 }\n", "f");
    assert!(e.contains("no overload of `+`"), "lambda parameters are monomorphic: {e}");
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
    assert!(err("q = users & select { id = .id } & where (.age > 1)\n", "q").contains("no column `age`"));
    assert!(err("q = users & where .name\n", "q").contains("bool"));
    assert!(err("q = users & where (.name >= 18)\n", "q").contains("expected string, found int"));
}

#[test]
fn phase_errors() {
    assert!(err("q = users & agg { t = sum (sum .age) }\n", "q").contains("cannot nest"));
    assert!(err("q = users & agg { n = count, name = .name }\n", "q").contains("not grouped"));
    assert!(err("q = users & agg { x = coalesce 0 (sum .age) + .age }\n", "q").contains("ungrouped"));
    assert!(err("q = users & agg { x = .age + sum .age }\n", "q").contains("ungrouped"));
    assert!(err("q = users & where (count >= 1)\n", "q").contains("aggregate"));
    assert!(err("q = users & select { n = count }\n", "q").contains("belong in `agg`"));
    assert!(err("q = users & where (rowNumber { order = [.id] } <= 3)\n", "q").contains("window"));
    assert!(err("q = users & select { r = rowNumber { order = [sum .id] } }\n", "q").contains("plain column"));
}

#[test]
fn window_types() {
    let src = "spec = { partition = [.user_id], order = [desc .id] }\n\
               q = orders & select { id = .id, rn = rowNumber spec, prev = lag spec .amount }\n";
    assert_eq!(ty(src, "q"), "query { id = int, rn = int, prev = maybe float }");
    assert!(err("q = users & select { r = rowNumber { bogus = [.id] } }\n", "q").contains("unknown window spec field"));
}

#[test]
fn join_errors() {
    assert!(err("q = orders & inner users (.user_id == .id)\n", "q").contains("which input"));
    assert!(err("q = users & where (.<id == 1)\n", "q").contains("join predicate"));
    assert!(err("q = orders & inner users (.<user_id == .>name)\n", "q").contains("type mismatch"));
    assert!(err("q = orders & inner users (.<nope == .>id)\n", "q").contains("no column `nope`"));
}

#[test]
fn key_mappers_are_typed() {
    assert_eq!(ty("q = users & pick [\"name\", \"id\"]\n", "q"), "query { name = string, id = int }");
    assert_eq!(ty("q = users & omit [\"active\", \"age\"]\n", "q"), "query { id = int, name = string }");
    assert_eq!(
        ty("q = users & rename { id = \"user_id\", name = \"label\" }\n", "q"),
        "query { user_id = int, label = string, age = int, active = bool }"
    );
    // A rename feeding a join and a later select is fully typed.
    assert_eq!(
        ty("q = orders & rename { id = \"order_id\" } & inner users (.<user_id == .>id) & select { o = .order_id, n = .name }\n", "q"),
        "query { o = int, n = string }"
    );
    // Reusable mappers through ordinary definitions.
    let src = "keep = pick [\"id\"]\na = users & keep\nb = orders & keep\n";
    assert_eq!(ty(src, "a"), "query { id = int }");
    assert_eq!(ty(src, "b"), "query { id = int }");
    // `prefix` is not static: the row is left to the IR validator.
    assert_eq!(ty("q = users & keyMap (prefix \"u_\")\n", "q"), "query a");
}

#[test]
fn key_mapper_errors() {
    assert!(err("q = users & pick [\"nope\"]\n", "q").contains("no column `nope`"));
    assert!(err("q = users & pick [\"id\", \"id\"]\n", "q").contains("listed twice"));
    assert!(err("q = users & rename { id = \"name\" }\n", "q").contains("`name` twice"));
    assert!(err("q = users & omit [\"age\"] & where (.age > 1)\n", "q").contains("no column `age`"));
    assert!(err("q = users & rename { name = \"label\" } & select { n = .name }\n", "q").contains("no column `name`"));
    assert!(err("q = users & pick 5\n", "q").contains("list of column names"));
    // Literal lists still work as ordinary values.
    assert!(err("q = users & order [\"id\"]\n", "q").contains("type mismatch"));
}

#[test]
fn overloads_resolve_by_type() {
    assert_eq!(
        ty("q = users & select { a = .age + 1, s = .name <> \"!\", f = negate .age }\n", "q"),
        "query { a = int, s = string, f = int }"
    );
    // Leftover literals default before overloads are forced.
    assert_eq!(ty("q = users & select { x = 1 + 2 }\n", "q"), "query { x = int }");
    // A helper with an open overload stays generic and works at both types.
    let src = "people : query { n = int, x = float } = table \"p\" \"people\"\n\
               twice = x => x + x\nq = people & select { a = twice .n, b = twice .x }\n";
    assert_eq!(ty(src, "q"), "query { a = int, b = float }");
    assert!(err("q = users & select { b = .active + .active }\n", "q").contains("no overload of `+` matches"));
    // `+` is numeric only; strings use `<>`, and there are no conversions.
    assert!(err("q = users & select { s = .name + .name }\n", "q").contains("no overload of `+` matches"));
    assert!(err("q = orders & select { x = .amount + .user_id }\n", "q").contains("expected int, found float"));
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
    assert!(ws.diags.iter().any(|d| d.message.contains("needs a type signature")), "{:?}", ws.diags);
}

const NULLABLE: &str = "people : query { id = int, email = maybe string, score = maybe int } = table \"p\" \"people\"\n";

#[test]
fn nullable_columns_are_explicit() {
    let q = |body: &str| format!("{NULLABLE}q = people & {body}\n");
    assert_eq!(ty(&q("select { e = .email }"), "q"), "query { e = maybe string }");
    assert_eq!(
        ty(&q("where (isNotNull .email) & select { e = coalesce \"-\" .email, s = coalesce 0 .score + 1 }"), "q"),
        "query { e = string, s = int }"
    );
    let e = err(&q("where (.email == \"x\")"), "q");
    assert!(e.contains("non-null") && e.contains("coalesce"), "{e}");
    assert!(err(&q("select { s = .score + 1 }"), "q").contains("no overload of `+`"));
    // `just` makes a value nullable explicitly; there is no implicit lift.
    assert_eq!(ty(&q("select { m = just .id }"), "q"), "query { m = maybe int }");
    // `coalesce`'s default is non-null (like Haskell's `fromMaybe`).
    assert!(err(&q("select { m = coalesce (just .id) .score }"), "q").contains("non-null"));
    let e = err(&q("select { m = coalesce 0 .id }"), "q");
    assert!(e.contains("field `id`") && e.contains("only one side is nullable"), "{e}");
    // `where` needs `bool`; `isTrue` treats NULL as false.
    let src = format!("{NULLABLE}flags : query {{ ok = maybe bool }} = table \"p\" \"f\"\nq = flags & where .ok\nr = flags & where (isTrue .ok)\n");
    assert!(err(&src, "q").contains("bool"));
    ty(&src, "r");
}

#[test]
fn outer_joins_make_the_missing_side_maybe() {
    let j = |kind: &str| ty(&format!("q = orders & {kind} users (.<user_id == .>id) & select {{ o = .amount, n = .name }}\n"), "q");
    assert_eq!(j("inner"), "query { o = float, n = string }");
    assert_eq!(j("leftJoin"), "query { o = float, n = maybe string }");
    assert_eq!(j("rightJoin"), "query { o = maybe float, n = string }");
    assert_eq!(j("fullJoin"), "query { o = maybe float, n = maybe string }");
    // An already nullable column is not wrapped twice.
    let src = format!("{NULLABLE}q = users & leftJoin people (.<id == .>id) & select {{ e = .email }}\n");
    assert_eq!(ty(&src, "q"), "query { e = maybe string }");
    // The predicate sees the plain column types.
    ty("q = orders & leftJoin users (.<user_id == .>id && .>active)\n", "q");
}

#[test]
fn aggregates_are_nullable() {
    let src = format!("{NULLABLE}q = people & agg {{ n = count, c = countOf .email, s = sum .score, t = coalesce \"\" (max .email) }}\n");
    assert_eq!(ty(&src, "q"), "query { n = int, c = int, s = maybe int, t = string }");
    let e = err("q = orders & agg { r = sum .amount } & where (.r > 100.0)\n", "q");
    assert!(e.contains("maybe float") && e.contains("nullable"), "{e}");
    ty("q = orders & agg { r = coalesce 0.0 (sum .amount) } & where (.r > 100.0)\n", "q");
    assert!(err("q = orders & agg { u = group .user_id } & select { x = sum .u }\n", "q").contains("belong in `agg`"));
}

#[test]
fn editing_the_root_rechecks_only_the_root() {
    let mut ws = Workspace::from_source(&format!("{TABLES}q = users & where (.age > 1)\n"));
    assert!(check(&ws).errors.is_empty());
    let runs = super::check_runs();
    let _ = check(&ws);
    assert_eq!(super::check_runs(), runs, "nothing changed, so nothing is rechecked");
    let file = *ws.inputs[ws.root].file(&ws.db);
    file.set_contents(&mut ws.db, format!("{TABLES}q = users & where (.age > \"x\")\n"));
    let after = check(&ws);
    assert_eq!(super::check_runs(), runs + 1, "only the edited root is rechecked, not the prelude");
    assert!(after.errors.iter().any(|e| e.module == ws.root && e.diag.message.contains("type mismatch")), "{:?}", after.errors);
}

#[test]
fn edits_update_names_checks_and_diagnostics() {
    let mut ws = Workspace::from_source(&format!("{TABLES}q = users & where (.age > 1)\n"));
    let root = ws.root;
    // A new definition is visible to later ones (scope is a query, not a
    // snapshot from load time), to the checker, and to the evaluator.
    assert!(ws.set_source(root, format!("{TABLES}adult = .age >= 18\nq = users & where adult\n")));
    assert!(check(&ws).errors.is_empty(), "{:?}", check(&ws).errors);
    let q = crate::root_queries(&ws).into_iter().find(|(n, _)| n == "q").unwrap().1;
    assert!(q.is_ok(), "{q:?}");
    // Syntax errors appear and disappear with the text.
    assert!(ws.set_source(root, format!("{TABLES}q = (\n")));
    assert!(ws.diags.iter().any(|d| d.message.starts_with("syntax error")), "{:?}", ws.diags);
    assert!(ws.set_source(root, format!("{TABLES}q = users\n")));
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    // Changing imports needs a reload.
    assert!(!ws.set_source(root, format!("import \"other.cagara\"\n{TABLES}")));
}
