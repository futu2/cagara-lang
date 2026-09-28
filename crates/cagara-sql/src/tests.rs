//! End-to-end tests: Cagara source -> SQL (ANSI).

use crate::{compile, Dialect};
use cagara_hir::{root_queries, Workspace};

const USERS: &str = "users : query { id = int, name = string, age = int, active = bool } = table \"public\" \"users\"\n\
orders : query { id = int, user_id = int, amount = float, status = string, created_at = date } = table \"public\" \"orders\"\n";

fn run(src: &str) -> Vec<(String, Result<String, String>)> {
    let full = format!("{USERS}{src}");
    let ws = Workspace::from_source(&full);
    assert!(ws.diags.is_empty(), "load diagnostics: {:?}", ws.diags);
    root_queries(&ws)
        .into_iter()
        .map(|(n, r)| (n, r.map_err(|d| d.message).and_then(|rel| compile(&rel, Dialect::Ansi, false))))
        .collect()
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
    assert!(error("q = users & agg { x = sum .age + .age }\n", "q").contains("ungrouped"));
    let w = error("q = users & where (rowNumber { order = [.id] } <= 3)\n", "q");
    assert!(w.contains("window"), "{w}");
}
