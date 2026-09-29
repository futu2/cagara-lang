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
    assert_eq!(ty("total = sum .amount\n", "total"), "agg (expr { amount = a | b } a)");
}

#[test]
fn query_output_rows() {
    assert_eq!(
        ty("q = users & where .active & select { id = .id, label = upper .name, next = .age + 1 }\n", "q"),
        "query { id = int, label = string, next = int }"
    );
    assert_eq!(
        ty("q = orders & agg { user_id = group .user_id, revenue = sum .amount, n = count }\n", "q"),
        "query { user_id = int, revenue = float, n = int }"
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
    assert!(err("q = users & select { x = .name + 1 }\n", "q").contains("expected string, found int"));
    assert!(err("q = users & limit .id\n", "q").contains("type mismatch"));
    let e = err("f = x => { a = x + 1, b = upper x }\n", "f");
    assert!(e.contains("type mismatch"), "lambda parameters are monomorphic: {e}");
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
    assert!(err("q = users & agg { x = sum .age + .age }\n", "q").contains("ungrouped"));
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
    assert_eq!(ty(src, "q"), "query { id = int, rn = int, prev = float }");
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
