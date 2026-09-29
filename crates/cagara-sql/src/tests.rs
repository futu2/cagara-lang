//! End-to-end tests: Cagara source -> SQL (ANSI).

use crate::{compile, Dialect, Options};
use cagara_hir::{root_queries, Workspace};

const USERS: &str = "users : query { id = int, name = string, age = int, active = bool } = table \"public\" \"users\"\n\
orders : query { id = int, user_id = int, amount = float, status = string, created_at = date } = table \"public\" \"orders\"\n";

fn run(src: &str) -> Vec<(String, Result<String, String>)> {
    let full = format!("{USERS}{src}");
    let ws = Workspace::from_source(&full);
    assert!(ws.diags.is_empty(), "load diagnostics: {:?}", ws.diags);
    root_queries(&ws)
        .into_iter()
        .map(|(n, r)| (n, r.map_err(|d| d.message).and_then(|rel| compile(&rel, Options::default()))))
        .collect()
}

fn sql_with(src: &str, name: &str, opts: Options) -> String {
    let full = format!("{USERS}{src}");
    let ws = Workspace::from_source(&full);
    assert!(ws.diags.is_empty(), "load diagnostics: {:?}", ws.diags);
    let (_, r) = root_queries(&ws).into_iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("no query `{name}`"));
    let s = r.map_err(|d| d.message).and_then(|rel| compile(&rel, opts)).unwrap_or_else(|e| panic!("`{name}` failed: {e}"));
    eprintln!("{name}: {s}");
    s
}

fn dialect(src: &str, name: &str, d: &str) -> Result<String, String> {
    let full = format!("{USERS}{src}");
    let ws = Workspace::from_source(&full);
    let (_, r) = root_queries(&ws).into_iter().find(|(n, _)| n == name).expect("no such query");
    let opts = Options { dialect: Dialect::from_str(d).expect("dialect"), ..Options::default() };
    r.map_err(|d| d.message).and_then(|rel| compile(&rel, opts))
}

fn sql(src: &str, name: &str) -> String {
    let out = run(src);
    let r = out.into_iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("no query `{name}`")).1;
    let s = r.unwrap_or_else(|e| panic!("`{name}` failed: {e}"));
    eprintln!("{name}: {s}");
    s
}

fn error(src: &str, name: &str) -> String {
    let out = run(src);
    let r = out.into_iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("no query `{name}`")).1;
    match r {
        Ok(s) => panic!("`{name}` should fail but compiled to {s}"),
        Err(e) => e,
    }
}

#[test]
fn table_is_plain_select() {
    assert_eq!(sql("", "users"), "SELECT id, name, age, active FROM public.users");
}

#[test]
fn filter_select_order_limit_fuse() {
    let s = sql(
        "q = users\n  & where (.age >= 18 && .active)\n  & select { id = .id, label = upper .name }\n  & order [asc .label]\n  & limit 10\n",
        "q",
    );
    assert!(s.contains("FROM public.users"), "{s}");
    assert!(s.contains("WHERE"), "{s}");
    assert!(s.contains("UPPER(name) AS label"), "{s}");
    assert!(s.contains("ORDER BY"), "{s}");
    assert!(s.contains("LIMIT 10"), "{s}");
    assert!(!s.contains("(SELECT"), "should not nest: {s}");
}

#[test]
fn filter_after_agg_wraps() {
    let s = sql(
        "q = orders\n  & where (.status == \"paid\")\n  & agg { user_id = group .user_id, revenue = sum .amount, n = count }\n  & where (.n >= 5)\n",
        "q",
    );
    assert!(s.contains("GROUP BY user_id"), "{s}");
    assert!(s.contains("SUM(amount) AS revenue"), "{s}");
    assert!(s.contains("(SELECT"), "must wrap before filtering aggregates: {s}");
}

#[test]
fn window_then_filter_wraps() {
    let s = sql(
        "spec = { partition = [.user_id], order = [desc .created_at] }\nq = orders\n  & select { id = .id, rn = rowNumber spec }\n  & where (.rn <= 3)\n",
        "q",
    );
    assert!(s.contains("ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY created_at DESC)"), "{s}");
    assert!(s.contains("(SELECT"), "{s}");
}

#[test]
fn running_total_frame() {
    let s = sql(
        "q = orders & select { id = .id, running = sumOver { partition = [.user_id], order = [.created_at], frame = runningFrame } .amount }\n",
        "q",
    );
    assert!(s.contains("ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"), "{s}");
}

#[test]
fn join_uses_sides() {
    let s = sql(
        "q = orders & rename { id = \"order_id\" } & inner users (.<user_id == .>id)\n  & select { order_id = .order_id, name = .name }\n",
        "q",
    );
    assert!(s.contains("INNER JOIN public.users AS t2"), "{s}");
    assert!(s.contains("t1.user_id = t2.id"), "{s}");
}

#[test]
fn pick_omit_rename() {
    assert_eq!(sql("q = users & pick [\"id\", \"name\"]\n", "q"), "SELECT id, name FROM public.users");
    assert_eq!(
        sql("q = users & omit [\"active\"] & rename { name = \"label\" }\n", "q"),
        "SELECT id, name AS label, age FROM public.users"
    );
}

#[test]
fn constant_global_agg_has_one_row() {
    let s = sql("q = users & agg { one = 1 }\n", "q");
    assert!(s.contains("COUNT(*)"), "{s}");
}

#[test]
fn errors() {
    assert!(error("q = users & agg { t = sum (sum .age) }\n", "q").contains("cannot nest"));
    assert!(error("q = users & agg { n = count, name = .name }\n", "q").contains("not grouped"));
    assert!(error("q = users & where (count >= 1)\n", "q").contains("aggregate"));
    assert!(error("q = users & where (.salary > 1)\n", "q").contains("no column `salary`"));
    assert!(error("q = orders & inner users (.user_id == .id)\n", "q").contains("which input"));
    assert!(error("q = users & pick [\"nope\"]\n", "q").contains("no column `nope`"));
    assert!(error("q = table \"s\" \"t\"\n", "q").contains("unknown"));
    assert!(error("q = q\n", "q").contains("refers to itself"));
    assert!(error("q = users & agg { x = coalesce (sum .age) 0 + .age }\n", "q").contains("ungrouped"));
    let w = error("q = users & where (rowNumber { order = [.id] } <= 3)\n", "q");
    assert!(w.contains("window"), "{w}");
}

#[test]
fn overloads_dispatch_to_sql() {
    let s = sql("q = users & select { s = .name <> \" \" <> upper .name, a = .age + 1 }\n", "q");
    // `<>` is right-associative.
    assert!(s.contains("name || (' ' || UPPER(name)) AS s"), "{s}");
    assert!(s.contains("age + 1 AS a"), "{s}");
    // One generic helper, dispatched at int and float in the same query
    // (the choices are also checked in cagara-hir's overload tests).
    let s = sql("twice = x => x + x\nquad = x => twice (twice x)\nq = orders & select { a = quad .user_id, b = twice .amount }\n", "q");
    assert!(s.contains("AS a"), "{s}");
    assert!(s.contains("amount + amount AS b"), "{s}");
    let s = sql(
        "describe : expr r int -> expr r string = sql \"CAST($1 AS TEXT)\"\n\
         describe : expr r bool -> expr r string = sql \"CASE WHEN $1 THEN 'yes' ELSE 'no' END\"\n\
         q = users & select { a = describe .age, b = describe .active }\n",
        "q",
    );
    assert!(s.contains("CAST(age AS TEXT)"), "{s}");
    assert!(s.contains("CASE WHEN active THEN 'yes' ELSE 'no' END"), "{s}");
}

#[test]
fn nulls_and_outer_joins() {
    let s = sql(
        "q = orders & leftJoin users (.<user_id == .>id)\n  & select { id = .id, who = coalesce .name \"?\", known = isNotNull .name }\n",
        "q",
    );
    assert!(s.contains("LEFT JOIN public.users"), "{s}");
    assert!(s.contains("COALESCE(t2.name, '?') AS who"), "{s}");
    assert!(s.contains("t2.name IS NOT NULL AS known"), "{s}");
    let s = sql("q = orders & agg { total = coalesce (sum .amount) 0.0 }\n", "q");
    assert!(s.contains("COALESCE(SUM(amount), 0.0) AS total"), "{s}");
    assert!(error("q = orders & leftJoin users (.<user_id == .>id) & where (.name == \"x\")\n", "q").contains("maybe"));
}

#[test]
fn join_inputs_are_inlined_when_safe() {
    // A rename before a join needs no derived table.
    let s = sql("q = orders & rename { id = \"order_id\" } & inner users (.<user_id == .>id) & select { o = .order_id, n = .name }\n", "q");
    assert_eq!(s, "SELECT t1.id AS o, t2.name AS n FROM public.orders AS t1 INNER JOIN public.users AS t2 ON t1.user_id = t2.id");
    // Left join: the preserved side's filter goes to WHERE, the other into ON.
    let s = sql("q = orders & where (.amount > 10.0) & leftJoin (users & where .active) (.<user_id == .>id)\n", "q");
    assert!(!s.contains("(SELECT"), "{s}");
    assert!(s.contains("ON (t1.user_id = t2.id) AND t2.active WHERE (t1.amount > 10.0)"), "{s}");
    // Right / full join: a filtered null-extended side keeps a derived table.
    let s = sql("q = orders & where (.amount > 10.0) & fullJoin users (.<user_id == .>id)\n", "q");
    assert!(s.contains("FROM public.orders WHERE (amount > 10.0)) AS t1 FULL JOIN public.users AS t2"), "{s}");
    let s = sql("q = orders & where (.amount > 10.0) & rightJoin users (.<user_id == .>id)\n", "q");
    assert!(s.contains("(SELECT"), "{s}");
    // Chains of joins stay flat, including a self-join with renamed columns.
    let s = sql("q = orders & inner users (.<user_id == .>id) & leftJoin (users & rename { id = \"uid\", name = \"n2\", active = \"a2\" }) (.<user_id == .>uid)\n", "q");
    assert!(!s.contains("(SELECT"), "{s}");
    assert!(s.contains("LEFT JOIN public.users AS t3 ON t1.user_id = t3.id"), "{s}");
    // A join on the right is a derived table.
    let s = sql("q = orders & inner (users & inner orders (.<id == .>user_id)) (.<user_id == .>id)\n", "q");
    assert!(s.contains("INNER JOIN (SELECT"), "{s}");
}

#[test]
fn dialects_rewrite_every_block() {
    let q = "q = users & select { id = .id, s = .name <> \"!\" } & order [asc .s] & limit 10 & offset 5\n";
    assert!(dialect(q, "q", "postgres").unwrap().contains("name || '!'"));
    let my = dialect(q, "q", "mysql").unwrap();
    assert!(!my.contains("||"), "`||` is OR in MySQL: {my}");
    assert!(my.contains("CONCAT(name, '!') AS s") && my.contains("ORDER BY CONCAT(name, '!')"), "{my}");
    assert!(my.contains("LIMIT 18446744073709551615 OFFSET 5"), "MySQL needs a LIMIT with OFFSET: {my}");
    let ts = dialect("q = users & order [asc .id] & offset 5 & limit 3\n", "q", "tsql").unwrap();
    assert!(ts.contains("ORDER BY id OFFSET 5 ROWS FETCH NEXT 3 ROWS ONLY"), "{ts}");
    let ts = dialect("q = users & order [asc .name] & limit 3 & select { s = .name <> .name }\n", "q", "tsql").unwrap();
    assert!(ts.contains("TOP 3") && ts.contains("CONCAT(name, name)"), "{ts}");
    assert!(dialect(q, "q", "tsql").unwrap_err().contains("needs an `order` before `offset`"));
    // Joins and windows go through every dialect.
    let j = "q = orders & leftJoin users (.<user_id == .>id) & select { n = coalesce .name \"?\" <> \"!\", rn = rowNumber { order = [desc .amount] } }\n";
    for d in ["postgres", "mysql", "sqlite", "duckdb", "tsql", "bigquery", "snowflake"] {
        let s = dialect(j, "q", d).unwrap_or_else(|e| panic!("{d}: {e}"));
        assert!(s.contains("ROW_NUMBER() OVER (ORDER BY t1.amount DESC)"), "{d}: {s}");
    }
}

#[test]
fn optimizer_keeps_stage_boundaries() {
    let opts = Options { optimize: true, ..Options::default() };
    // A filter after a window, LIMIT, or aggregate must stay outside it.
    for q in [
        "q = orders & select { id = .id, amount = .amount, rn = rowNumber { order = [desc .amount] } } & where (.amount > 10.0)\n",
        "q = orders & order [desc .amount] & limit 5 & where (.amount > 10.0)\n",
        "q = orders & agg { u = group .user_id, n = count } & where (.n > 3)\n",
    ] {
        let s = sql_with(q, "q", opts);
        assert!(s.contains(") AS t1 WHERE"), "filter moved across a boundary: {s}");
    }
    let s = sql_with("q = orders & where (1 + 1 == 2 && .amount > 1.0)\n", "q", opts);
    assert!(!s.contains("1 + 1"), "{s}");
}

#[test]
fn validator_errors_point_at_the_stage() {
    let src = format!("{USERS}q = users\n  & keyMap (prefix \"u_\")\n  & where (.id > 1)\nb = table \"s\" \"t\" & select {{ x = .x }}\n");
    let ws = Workspace::from_source(&src);
    let out = root_queries(&ws);
    let get = |n: &str| out.iter().find(|(k, _)| k == n).unwrap().1.clone().unwrap_err();
    let d = get("q");
    assert!(d.message.contains("no column `id`"), "{d}");
    assert_eq!(d.source.trim(), "& where (.id > 1)", "{d}");
    assert_eq!((d.col, d.width), (5, "where (.id > 1)".len()), "{d}");
    let d = get("b");
    assert!(d.message.contains("unknown"), "{d}");
    assert_eq!((d.col, d.width), (5, "table \"s\" \"t\"".len()), "{d}");
    assert!(d.to_string().contains("^^^^^"), "{d}");
}
