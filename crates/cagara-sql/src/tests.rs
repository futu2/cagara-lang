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
        .map(|(n, r)| {
            (
                n,
                r.map_err(|d| d.message)
                    .and_then(|rel| compile(&rel, Options::default())),
            )
        })
        .collect()
}

fn sql_with(src: &str, name: &str, opts: Options) -> String {
    let full = format!("{USERS}{src}");
    let ws = Workspace::from_source(&full);
    assert!(ws.diags.is_empty(), "load diagnostics: {:?}", ws.diags);
    let (_, r) = root_queries(&ws)
        .into_iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no query `{name}`"));
    let s = r
        .map_err(|d| d.message)
        .and_then(|rel| compile(&rel, opts))
        .unwrap_or_else(|e| panic!("`{name}` failed: {e}"));
    eprintln!("{name}: {s}");
    s
}

fn dialect(src: &str, name: &str, d: &str) -> Result<String, String> {
    let full = format!("{USERS}{src}");
    let ws = Workspace::from_source(&full);
    let (_, r) = root_queries(&ws)
        .into_iter()
        .find(|(n, _)| n == name)
        .expect("no such query");
    let opts = Options {
        dialect: Dialect::from_str(d).expect("dialect"),
        ..Options::default()
    };
    r.map_err(|d| d.message).and_then(|rel| compile(&rel, opts))
}

fn sql(src: &str, name: &str) -> String {
    let out = run(src);
    let r = out
        .into_iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no query `{name}`"))
        .1;
    let s = r.unwrap_or_else(|e| panic!("`{name}` failed: {e}"));
    eprintln!("{name}: {s}");
    s
}

fn error(src: &str, name: &str) -> String {
    let out = run(src);
    let r = out
        .into_iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no query `{name}`"))
        .1;
    match r {
        Ok(s) => panic!("`{name}` should fail but compiled to {s}"),
        Err(e) => e,
    }
}

#[test]
fn table_is_plain_select() {
    assert_eq!(
        sql("", "users"),
        "SELECT id, name, age, active FROM public.users"
    );
}

#[test]
fn shipped_valid_examples_compile_to_sql() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    for file in ["report.cagara", "public.cagara", "schema.cagara"] {
        let ws = Workspace::open(&dir.join(file));
        assert!(ws.diags.is_empty(), "{file}: {:?}", ws.diags);
        for (name, rel) in root_queries(&ws) {
            let rel = rel.unwrap_or_else(|d| panic!("{file} / `{name}`: {d}"));
            compile(&rel, Options::default()).unwrap_or_else(|e| panic!("{file} / `{name}`: {e}"));
        }
    }
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
    assert!(
        s.contains("(SELECT"),
        "must wrap before filtering aggregates: {s}"
    );
}

#[test]
fn window_then_filter_wraps() {
    let s = sql(
        "spec = { partition = [.user_id], order = [desc .created_at] }\nq = orders\n  & select { id = .id, rn = rowNumber spec }\n  & where (.rn <= 3)\n",
        "q",
    );
    assert!(
        s.contains("ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY created_at DESC NULLS LAST)"),
        "{s}"
    );
    assert!(s.contains("(SELECT"), "{s}");
}

#[test]
fn running_total_frame() {
    let s = sql(
        "q = orders & select { id = .id, running = sumOver { partition = [.user_id], order = [.created_at], frame = runningFrame } .amount }\n",
        "q",
    );
    assert!(
        s.contains("ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"),
        "{s}"
    );
}

#[test]
fn join_uses_sides() {
    let s = sql(
        "q = orders & select { order_id = .id, user_id = .user_id } & inner users (.<user_id == .>id)\n  & select { order_id = .order_id, name = .name }\n",
        "q",
    );
    assert!(s.contains("INNER JOIN public.users AS t2"), "{s}");
    assert!(s.contains("t1.user_id = t2.id"), "{s}");
}

#[test]
fn distinct_case_cast_and_membership() {
    let s = sql(
        "q = users & where (inList [1, 2] .id) & distinct\n\
         & select { id = .id, label = ifThenElse (.id == 1) \"yes\" \"no\", n = toFloat .id }\n",
        "q",
    );
    // `distinct` dedupes the input row, so the projection that follows it is
    // computed outside the DISTINCT.
    assert!(
        s.contains("SELECT DISTINCT id, name, age, active FROM public.users"),
        "{s}"
    );
    assert!(!s.starts_with("SELECT DISTINCT"), "{s}");
    assert!(s.contains("id IN (1, 2)"), "{s}");
    assert!(
        s.contains("CASE WHEN (id = 1) THEN 'yes' ELSE 'no' END"),
        "{s}"
    );
    assert!(s.contains("CAST(id AS FLOAT) AS n"), "{s}");
}

#[test]
fn distinct_is_a_lowering_barrier() {
    // A stage that changes the row cannot fold into `distinct`: it would
    // dedupe on the new columns instead of the input row.
    let agg = sql("q = users & distinct & agg { n = count }\n", "q");
    assert!(agg.contains("COUNT(*) AS n FROM (SELECT DISTINCT"), "{agg}");
    let win = sql(
        "q = users & distinct & select { id = .id, rn = rowNumber { order = [asc .id] } }\n",
        "q",
    );
    assert!(win.contains("FROM (SELECT DISTINCT"), "{win}");
    assert!(win.contains("ROW_NUMBER()"), "{win}");
    let pick = sql("q = users & distinct & select {.id, .name, .active}\n", "q");
    assert!(pick.contains("FROM (SELECT DISTINCT"), "{pick}");
    // `order` sorts outside the dedup, so an emulated NULLS LAST key never
    // lands in a DISTINCT select list (T-SQL rejects that).
    let ordered = dialect("q = users & distinct & order [asc .name]\n", "q", "tsql").unwrap();
    assert!(
        ordered.contains("FROM (SELECT DISTINCT id, name, age, active"),
        "{ordered}"
    );
    assert!(
        ordered.contains("ORDER BY CASE WHEN name IS NULL"),
        "{ordered}"
    );
    // A projection *before* `distinct` defines the row, so it still fuses.
    let fused = sql("q = users & select { n = .name } & distinct\n", "q");
    assert_eq!(fused, "SELECT DISTINCT name AS n FROM public.users");
}

#[test]
fn set_operations_and_reused_queries_use_sql_set_ops_and_ctes() {
    let s = sql(
        "active = users & where .active\nq = union active active\n",
        "q",
    );
    assert!(s.contains("WITH cagara_cte"), "{s}");
    assert!(s.contains("UNION"), "{s}");
    assert_eq!(s.matches("WHERE active").count(), 1, "{s}");
}

#[test]
fn a_generated_cte_does_not_shadow_a_table() {
    // `cagara_cte1` is the first generated CTE name, and a CTE hides a
    // same-named table for the whole statement — the generated name must
    // move out of the way (case-insensitively, as engines fold identifiers).
    let src = "cagara_cte1 : query { id = int, name = string, age = int, active = bool } = \
               table \"public\" \"cagara_cte1\"\n\
               active = cagara_cte1 & where .active\n\
               q = union active active\n";
    let s = sql(src, "q");
    assert!(s.contains("WITH cagara_cte2"), "{s}");
    assert!(s.contains("FROM public.cagara_cte1"), "{s}");
}

#[test]
fn a_set_branch_with_its_own_limit_is_wrapped() {
    // A set operator's LIMIT applies to the whole operation, so a branch
    // that has one cannot be a bare SELECT: `SELECT ... LIMIT 3 UNION
    // SELECT ... LIMIT 2` is rejected by every engine. Such a branch goes
    // behind a derived table, which is portable.
    let s = sql("q = union (users & limit 3) (users & limit 2)\n", "q");
    assert!(
        s.contains("FROM (SELECT id, name, age, active FROM public.users LIMIT 3) AS t1"),
        "{s}"
    );
    assert!(
        s.contains("FROM (SELECT id, name, age, active FROM public.users LIMIT 2) AS t2"),
        "{s}"
    );
    // No LIMIT is left attached to a branch of the set operation itself.
    assert!(!s.contains("LIMIT 3 UNION"), "{s}");
    // The common case is untouched: a branch without pagination stays flat.
    let plain = sql(
        "q = union (users & where (.age > 1)) (users & where (.age > 2))\n",
        "q",
    );
    assert!(plain.contains("UNION SELECT"), "{plain}");
    assert!(
        !plain.contains("UNION SELECT t"),
        "no wrapper for a plain branch: {plain}"
    );
    assert!(!plain.contains("LIMIT"), "{plain}");
}

#[test]
fn a_set_branch_with_its_own_offset_is_wrapped() {
    let s = sql(
        "p = users & order [asc .id] & offset 5\nq = union p (users & where .active)\n",
        "q",
    );
    // The offset branch is a derived table, and its ORDER BY stays inside it
    // where the OFFSET needs it.
    assert!(
        s.contains("OFFSET 5) AS t"),
        "the offset branch should be a derived table: {s}"
    );
    assert!(!s.contains("OFFSET 5 UNION"), "{s}");
}

#[test]
fn nested_blocks_get_dialect_rewrites() {
    // Regression: a reused query becomes a CTE, and a set operation is not a
    // `Select`, so neither used to reach the expression rewriter. Their
    // `CAGARA_*` intrinsics, `||`, and T-SQL conditions leaked unchanged.
    let reused = "act = users & where .active & select { n = length .name, s = .name <> \"!\" }\n\
                  q = union act act\n";
    let ansi = dialect(reused, "q", "ansi").unwrap();
    assert!(!ansi.contains("CAGARA_"), "{ansi}");
    assert!(ansi.contains("LENGTH(name)"), "{ansi}");
    let mysql = dialect(reused, "q", "mysql").unwrap();
    assert!(mysql.contains("CHAR_LENGTH(name)"), "{mysql}");
    assert!(mysql.contains("CONCAT(name, '!')"), "{mysql}");
    let tsql = dialect(reused, "q", "tsql").unwrap();
    assert!(tsql.contains("CONCAT(name, '!')"), "{tsql}");
    assert!(tsql.contains("(LEN(name + 'x') - 1)"), "{tsql}");

    // The same, without reuse: the branches are the set operation's own.
    let once = "q = union (users & select { n = length .name })\n\
                (users & select { n = length .name })\n";
    let mysql = dialect(once, "q", "mysql").unwrap();
    assert!(!mysql.contains("CAGARA_"), "{mysql}");
    assert!(mysql.contains("CHAR_LENGTH(name)"), "{mysql}");
}

#[test]
fn semi_and_anti_joins_use_exists_without_right_columns() {
    let semi = sql("q = users & semiJoin users (.<id == .>id)\n", "q");
    assert!(semi.contains("EXISTS"), "{semi}");
    assert!(!semi.contains("JOIN"), "{semi}");
    let anti = sql("q = users & antiJoin users (.<id == .>id)\n", "q");
    assert!(anti.contains("NOT EXISTS"), "{anti}");
    assert!(!anti.contains("JOIN"), "{anti}");
}

#[test]
fn field_shorthand_and_update() {
    // `{.a, .b}` is `{a = .a, b = .b}`.
    assert_eq!(
        sql("q = users & select {.id, .name}\n", "q"),
        "SELECT id, name FROM public.users"
    );
    // An updated name the input does not have is appended.
    assert_eq!(
        sql(
            "q = users & select {.id, .name, .age} & update { label = .name }\n",
            "q"
        ),
        "SELECT id, name, age, name AS label FROM public.users"
    );
    // `update` overwrites in place, so a recomputed column keeps its slot.
    assert_eq!(
        sql("q = users & update { age = .age + 1 }\n", "q"),
        "SELECT id, name, age + 1 AS age, active FROM public.users"
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
    assert!(error("q = users & select {.nope}\n", "q").contains("no column `nope`"));
    assert!(error("q = table \"s\" \"t\"\n", "q").contains("unknown"));
    assert!(error("q = q\n", "q").contains("refers to itself"));
    assert!(error(
        "q = users & agg { x = coalesce 0 (sum .age) + .age }\n",
        "q"
    )
    .contains("ungrouped"));
    let w = error(
        "q = users & where (rowNumber { order = [.id] } <= 3)\n",
        "q",
    );
    assert!(w.contains("window"), "{w}");
}

#[test]
fn overloads_dispatch_to_sql() {
    let s = sql(
        "q = users & select { s = .name <> \" \" <> upper .name, a = .age + 1 }\n",
        "q",
    );
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
    assert!(
        s.contains("CASE WHEN active THEN 'yes' ELSE 'no' END"),
        "{s}"
    );
}

#[test]
fn nulls_and_outer_joins() {
    let s = sql(
        "q = orders & leftJoin users (.<user_id == .>id)\n  & select { id = .id, who = coalesce \"?\" .name, known = isNotNull .name }\n",
        "q",
    );
    assert!(s.contains("LEFT JOIN public.users"), "{s}");
    assert!(s.contains("COALESCE(t2.name, '?') AS who"), "{s}");
    assert!(s.contains("t2.name IS NOT NULL AS known"), "{s}");
    let s = sql(
        "q = orders & agg { total = coalesce 0.0 (sum .amount) }\n",
        "q",
    );
    assert!(s.contains("COALESCE(SUM(amount), 0.0) AS total"), "{s}");
    assert!(error(
        "q = orders & leftJoin users (.<user_id == .>id) & where (.name == \"x\")\n",
        "q"
    )
    .contains("maybe"));
}

#[test]
fn join_inputs_are_inlined_when_safe() {
    // An update before a join needs no derived table.
    let s = sql("q = orders & select { order_id = .id, user_id = .user_id } & inner users (.<user_id == .>id) & select { o = .order_id, n = .name }\n", "q");
    assert_eq!(s, "SELECT t1.id AS o, t2.name AS n FROM public.orders AS t1 INNER JOIN public.users AS t2 ON t1.user_id = t2.id");
    // An outer join's input is a relation, so a filter on either side stays
    // in that side's derived table. Hoisting the preserved side's filter to
    // WHERE would drop the unmatched rows the join keeps; hoisting the
    // null-extended side's into ON would widen the input back to the
    // unfiltered relation.
    let s = sql("q = orders & where (.amount > 10.0) & leftJoin (users & where .active) (.<user_id == .>id)\n", "q");
    assert_eq!(
        s,
        "SELECT t1.id, t1.user_id, t1.amount, t1.status, t1.created_at, t2.name, t2.age, t2.active \
         FROM (SELECT id, user_id, amount, status, created_at FROM public.orders WHERE (amount > 10.0)) AS t1 \
         LEFT JOIN (SELECT id, name, age, active FROM public.users WHERE active) AS t2 ON t1.user_id = t2.id"
    );
    // The null-extended side of a left join may still keep its filter in ON,
    // which is what lets `nasc`-style filters stay inline; but only when the
    // preserved side contributes no filter of its own.
    let s = sql(
        "q = orders & leftJoin (users & where .active) (.<user_id == .>id)\n",
        "q",
    );
    assert_eq!(
        s,
        "SELECT t1.id, t1.user_id, t1.amount, t1.status, t1.created_at, t2.name, t2.age, t2.active \
         FROM public.orders AS t1 \
         LEFT JOIN (SELECT id, name, age, active FROM public.users WHERE active) AS t2 ON t1.user_id = t2.id"
    );
    // Right / full join: every filtered side keeps a derived table.
    let s = sql(
        "q = orders & where (.amount > 10.0) & fullJoin users (.<user_id == .>id)\n",
        "q",
    );
    assert!(
        s.contains("FROM public.orders WHERE (amount > 10.0)) AS t1 FULL JOIN public.users AS t2"),
        "{s}"
    );
    let s = sql(
        "q = orders & where (.amount > 10.0) & rightJoin users (.<user_id == .>id)\n",
        "q",
    );
    assert!(s.contains("(SELECT"), "{s}");
    // A right join's *right* input is the preserved side, so a filter there
    // stays in its derived table rather than becoming an ON predicate (which
    // would resurrect the filtered-out rows as unmatched ones).
    let s = sql(
        "q = orders & rightJoin (users & where .active) (.<user_id == .>id)\n",
        "q",
    );
    assert_eq!(
        s,
        "SELECT t1.id, t1.user_id, t1.amount, t1.status, t1.created_at, t2.name, t2.age, t2.active \
         FROM public.orders AS t1 \
         RIGHT JOIN (SELECT id, name, age, active FROM public.users WHERE active) AS t2 ON t1.user_id = t2.id"
    );
    // Chains of joins stay flat, including a self-join with an updated column.
    let s = sql("q = orders & inner users (.<user_id == .>id) & leftJoin (users & select { uid = .id, name = .name, age = .age, active = .active }) (.<user_id == .>uid)\n", "q");
    assert!(!s.contains("(SELECT"), "{s}");
    assert!(
        s.contains("LEFT JOIN public.users AS t3 ON t1.user_id = t3.id"),
        "{s}"
    );
    // A join on the right is a derived table.
    let s = sql(
        "q = orders & inner (users & inner orders (.<id == .>user_id)) (.<user_id == .>id)\n",
        "q",
    );
    assert!(s.contains("INNER JOIN (SELECT"), "{s}");
}

#[test]
fn dialects_rewrite_every_block() {
    let q = "q = users & select { id = .id, s = .name <> \"!\" } & order [asc .s] & limit 10 & offset 5\n";
    assert!(dialect(q, "q", "postgres").unwrap().contains("name || '!'"));
    let my = dialect(q, "q", "mysql").unwrap();
    assert!(!my.contains("||"), "`||` is OR in MySQL: {my}");
    // MySQL has no NULLS LAST: a leading `IS NULL` key sorts them last.
    assert!(
        my.contains("CONCAT(name, '!') AS s")
            && my.contains("ORDER BY CASE WHEN CONCAT(name, '!') IS NULL THEN 1 ELSE 0 END, CONCAT(name, '!') LIMIT 10"),
        "{my}"
    );
    // The outer query keeps the order (a derived table's order is lost).
    assert!(
        my.contains("AS t1 ORDER BY CASE WHEN s IS NULL THEN 1 ELSE 0 END, s LIMIT"),
        "{my}"
    );
    assert!(
        my.contains("LIMIT 18446744073709551615 OFFSET 5"),
        "MySQL needs a LIMIT with OFFSET: {my}"
    );
    let ts = dialect(
        "q = users & order [asc .id] & offset 5 & limit 3\n",
        "q",
        "tsql",
    )
    .unwrap();
    assert!(
        ts.contains("ORDER BY CASE WHEN id IS NULL THEN 1 ELSE 0 END, id OFFSET 5 ROWS FETCH NEXT 3 ROWS ONLY"),
        "{ts}"
    );
    let ts = dialect(
        "q = users & order [asc .name] & limit 3 & select { s = .name <> .name }\n",
        "q",
        "tsql",
    )
    .unwrap();
    assert!(
        ts.contains("TOP 3") && ts.contains("CONCAT(name, name)"),
        "{ts}"
    );
    // The order carries out of the paged derived table, so T-SQL's OFFSET
    // has the ORDER BY it needs.
    let ts = dialect(q, "q", "tsql").unwrap();
    assert!(
        ts.contains("SELECT TOP 10")
            && ts.contains("AS t1 ORDER BY CASE WHEN s IS NULL THEN 1 ELSE 0 END, s OFFSET 5 ROWS"),
        "{ts}"
    );
    assert!(dialect("q = users & offset 5\n", "q", "tsql")
        .unwrap_err()
        .contains("needs an `order` before `offset`"));
    // A lone `offset` is not valid everywhere, so the "no limit" is spelled
    // per dialect: MySQL's max BIGINT, SQLite's `LIMIT -1`, and for T-SQL a
    // FETCH count, since `OFFSET .. ROWS` is not a statement on its own.
    let solo = "q = users & order [asc .id] & offset 5\n";
    let my = dialect(solo, "q", "mysql").unwrap();
    assert!(my.contains("LIMIT 18446744073709551615 OFFSET 5"), "{my}");
    let sq = dialect(solo, "q", "sqlite").unwrap();
    assert!(
        sq.ends_with("ORDER BY id NULLS LAST LIMIT -1 OFFSET 5"),
        "{sq}"
    );
    // SQLite rejects the MySQL spelling (`datatype mismatch`), so the two
    // must not share a branch.
    assert!(!sq.contains("18446744073709551615"), "{sq}");
    let ts = dialect(solo, "q", "tsql").unwrap();
    assert!(
        ts.contains("OFFSET 5 ROWS FETCH NEXT 9223372036854775807 ROWS ONLY"),
        "T-SQL needs a FETCH with OFFSET: {ts}"
    );
    // Dialects that accept a bare OFFSET keep it.
    let pg = dialect(solo, "q", "postgres").unwrap();
    assert!(pg.contains("ORDER BY id OFFSET 5"), "{pg}");
    assert!(!pg.contains("LIMIT"), "{pg}");
    // Joins and windows go through every dialect.
    let j = "q = orders & leftJoin users (.<user_id == .>id) & select { n = coalesce \"?\" .name <> \"!\", rn = rowNumber { order = [desc .amount] } }\n";
    for d in [
        "postgres",
        "mysql",
        "sqlite",
        "duckdb",
        "tsql",
        "bigquery",
        "snowflake",
    ] {
        let s = dialect(j, "q", d).unwrap_or_else(|e| panic!("{d}: {e}"));
        // NULLs sort last: explicit only where DESC puts them first.
        let want = if matches!(d, "postgres" | "snowflake") {
            "ROW_NUMBER() OVER (ORDER BY t1.amount DESC NULLS LAST)"
        } else {
            "ROW_NUMBER() OVER (ORDER BY t1.amount DESC)"
        };
        assert!(s.contains(want), "{d}: {s}");
    }
}

#[test]
fn optimizer_keeps_stage_boundaries() {
    let opts = Options {
        optimize: true,
        ..Options::default()
    };
    // A filter after a window, LIMIT, or aggregate must stay outside it.
    for q in [
        "q = orders & select { id = .id, amount = .amount, rn = rowNumber { order = [desc .amount] } } & where (.amount > 10.0)\n",
        "q = orders & order [desc .amount] & limit 5 & where (.amount > 10.0)\n",
        "q = orders & agg { u = group .user_id, n = count } & where (.n > 3)\n",
    ] {
        let s = sql_with(q, "q", opts);
        assert!(s.contains(") AS t1 WHERE"), "filter moved across a boundary: {s}");
    }
    let s = sql_with(
        "q = orders & where (1 + 1 == 2 && .amount > 1.0)\n",
        "q",
        opts,
    );
    assert!(!s.contains("1 + 1"), "{s}");
}

#[test]
fn validator_errors_point_at_the_stage() {
    // `q` reads the output of a definition only known at evaluation time, so
    // the IR validator (not the checker) reports the missing column.
    let src = format!("{USERS}u = users & select {{.id, .name}}\nq = u\n  & where (.nope > 1)\nb = table \"s\" \"t\" & select {{ x = .x }}\n");
    let ws = Workspace::from_source(&src);
    let out = root_queries(&ws);
    let get = |n: &str| {
        out.iter()
            .find(|(k, _)| k == n)
            .unwrap()
            .1
            .clone()
            .unwrap_err()
    };
    let d = get("q");
    assert!(d.message.contains("no column `nope`"), "{d}");
    assert_eq!(d.source.trim(), "& where (.nope > 1)", "{d}");
    assert_eq!((d.col, d.width), (5, "where".len()), "{d}");
    let d = get("b");
    assert!(d.message.contains("unknown"), "{d}");
    assert_eq!((d.col, d.width), (5, "table \"s\" \"t\"".len()), "{d}");
    assert!(d.to_string().contains("^^^^^"), "{d}");
}

const EVENTS: &str =
    "ev : query { id = int, at = timestamp, d = date, s = string } = table \"public\" \"ev\"\n";

/// Compile `select { x = <e> }` over `ev` for each dialect.
fn ev(e: &str, d: &str) -> String {
    let q = format!("{EVENTS}q = ev & select {{ x = {e} }}\n");
    dialect(&q, "q", d).unwrap_or_else(|err| panic!("{d}: {err}"))
}

#[test]
fn date_arithmetic_per_dialect() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "addDays 7 .d",
            &[
                ("ansi", "CAST((d + 7 * INTERVAL '1' DAY) AS DATE)"),
                ("postgres", "(d + 7 * INTERVAL '1' DAY)::DATE"),
                ("mysql", "DATE_ADD(d, INTERVAL 7 DAY)"),
                ("sqlite", "DATE(d, 7 || ' days')"),
                ("duckdb", "CAST((d + TO_DAYS(7)) AS DATE)"),
                ("tsql", "DATEADD(DAY, 7, d)"),
                ("bigquery", "DATE_ADD(d, INTERVAL 7 DAY)"),
                ("snowflake", "DATEADD(DAY, 7, d)"),
            ],
        ),
        (
            "addWeeks 2 .d",
            &[("mysql", "DATE_ADD(d, INTERVAL (2 * 7) DAY)")],
        ),
        (
            "addQuarters 1 .d",
            &[("tsql", "DATEADD(MONTH, (1 * 3), d)")],
        ),
        (
            "addMonths 2 .at",
            &[
                ("postgres", "(at + 2 * INTERVAL '1' MONTH)::TIMESTAMP"),
                ("sqlite", "DATETIME(at, 2 || ' months', 'floor')"),
                (
                    "bigquery",
                    "CAST(DATETIME_ADD(CAST(at AS DATETIME), INTERVAL 2 MONTH) AS TIMESTAMP)",
                ),
            ],
        ),
        (
            "addHours 3 .at",
            &[("bigquery", "TIMESTAMP_ADD(at, INTERVAL 3 HOUR)")],
        ),
        (
            "daysBetween .d currentDate",
            &[
                ("postgres", "(CURRENT_DATE - d)"),
                ("mysql", "DATEDIFF(CURRENT_DATE(), d)"),
                (
                    "sqlite",
                    "CAST((JULIANDAY(CURRENT_DATE) - JULIANDAY(d)) AS INT)",
                ),
                ("duckdb", "DATE_DIFF('day', d, CURRENT_DATE)"),
                ("tsql", "DATEDIFF(DAY, d, CAST(GETDATE() AS DATE))"),
                ("bigquery", "DATE_DIFF(CURRENT_DATE, d, DAY)"),
            ],
        ),
        (
            "now",
            &[
                ("ansi", "CURRENT_TIMESTAMP AS x"),
                ("sqlite", "DATETIME('now')"),
                ("tsql", "GETDATE()"),
            ],
        ),
        (
            "toTimestamp .d",
            &[
                ("mysql", "CAST(d AS DATETIME)"),
                ("sqlite", "DATETIME(d)"),
                ("tsql", "CAST(d AS DATETIME2)"),
            ],
        ),
    ];
    for (e, want) in cases {
        for (d, sql) in *want {
            let s = ev(e, d);
            assert!(s.contains(sql), "{e} / {d}: {s}");
        }
    }
}

#[test]
fn date_trunc_and_parts_per_dialect() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("truncMonth .d", &[
            ("postgres", "DATE_TRUNC('MONTH', d)::DATE"),
            ("mysql", "CAST(DATE_FORMAT(d, '%Y-%m-01') AS DATE)"),
            ("sqlite", "DATE(d, 'start of month')"),
            ("tsql", "DATETRUNC(MONTH, d)"),
            ("bigquery", "DATE_TRUNC(d, MONTH)"),
            ("snowflake", "DATE_TRUNC('MONTH', d)"),
        ]),
        ("truncWeek .d", &[
            ("mysql", "CAST(DATE_SUB(DATE(d), INTERVAL WEEKDAY(d) DAY) AS DATE)"),
            ("sqlite", "DATE(d, 'start of day', '-' || ((CAST(STRFTIME('%w', d) AS INT) + 6) % 7) || ' days')"),
            ("tsql", "DATETRUNC(ISO_WEEK, d)"),
            ("bigquery", "DATE_TRUNC(d, ISOWEEK)"),
        ]),
        ("truncQuarter .d", &[("mysql", "CAST(DATE_ADD(MAKEDATE(YEAR(d), 1), INTERVAL ((QUARTER(d) - 1) * 3) MONTH) AS DATE)")]),
        ("truncHour .at", &[("sqlite", "STRFTIME('%Y-%m-%d %H:00:00', at)"), ("bigquery", "TIMESTAMP_TRUNC(at, HOUR)")]),
        ("year .d", &[("postgres", "EXTRACT(YEAR FROM d)::INT"), ("sqlite", "CAST(STRFTIME('%Y', d) AS INT)"), ("tsql", "DATEPART(YEAR, d)")]),
        ("dayOfWeek .d", &[
            ("postgres", "EXTRACT(DOW FROM d)::INT"),
            ("mysql", "(DAYOFWEEK(d) - 1)"),
            ("sqlite", "CAST(STRFTIME('%w', d) AS INT)"),
            ("bigquery", "CAST(FORMAT_DATE('%w', d) AS BIGINT)"),
        ]),
        ("quarter .d", &[("sqlite", "((CAST(STRFTIME('%m', d) AS INT) + 2) / 3)")]),
    ];
    for (e, want) in cases {
        for (d, sql) in *want {
            let s = ev(e, d);
            assert!(s.contains(sql), "{e} / {d}: {s}");
        }
    }
}

#[test]
fn string_functions_per_dialect() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "left 3 .s",
            &[("postgres", "LEFT(s, 3)"), ("sqlite", "SUBSTR(s, 1, 3)")],
        ),
        (
            "right 2 .s",
            &[
                ("tsql", "RIGHT(s, 2)"),
                (
                    "sqlite",
                    "CASE WHEN 2 <= 0 THEN '' ELSE SUBSTR(s, -2, 2) END",
                ),
            ],
        ),
        (
            "strpos \"x\" .s",
            &[
                ("postgres", "STRPOS(s, 'x')"),
                ("mysql", "LOCATE('x', s)"),
                ("sqlite", "INSTR(s, 'x')"),
                ("tsql", "CHARINDEX('x', s)"),
                ("snowflake", "CHARINDEX('x', s)"),
            ],
        ),
        (
            "length .s",
            &[
                ("mysql", "CHAR_LENGTH(s)"),
                // LEN ignores trailing spaces.
                ("tsql", "(LEN(s + 'x') - 1)"),
                ("bigquery", "LENGTH(s)"),
            ],
        ),
        (
            "endsWith \"z\" .s",
            &[("postgres", "RIGHT(s, LENGTH('z')) = 'z'")],
        ),
        (
            "substring 2 3 .s",
            &[
                ("postgres", "SUBSTRING(s, 2, 3)"),
                ("mysql", "SUBSTR(s, 2, 3)"),
            ],
        ),
        (
            "replaceAll \"a\" \"b\" .s",
            &[("ansi", "REPLACE(s, 'a', 'b')")],
        ),
        (
            "ilike \"A%\" .s",
            &[
                ("postgres", "s ILIKE 'A%'"),
                ("mysql", "LOWER(s) LIKE LOWER('A%')"),
            ],
        ),
        (
            "toString .id",
            &[
                ("mysql", "CAST(id AS CHAR)"),
                ("bigquery", "CAST(id AS STRING)"),
            ],
        ),
    ];
    for (e, want) in cases {
        for (d, sql) in *want {
            let s = ev(e, d);
            assert!(s.contains(sql), "{e} / {d}: {s}");
        }
    }
}

#[test]
fn date_functions_compose_and_type_check() {
    // Nesting keeps precedence; string literals widen to dates and timestamps.
    let q = format!(
        "{EVENTS}q = ev & where (.at >= \"2024-01-01 00:00:00\" && .d < addDays 1 (truncMonth currentDate))\n\
         & agg {{ m = group (truncMonth .d), n = count }}\n"
    );
    let s = dialect(&q, "q", "postgres").unwrap();
    assert!(
        s.contains("(DATE_TRUNC('MONTH', CURRENT_DATE)::DATE + 1 * INTERVAL '1' DAY)::DATE"),
        "{s}"
    );
    assert!(s.contains("GROUP BY DATE_TRUNC('MONTH', d)::DATE"), "{s}");
    // Sub-day units need a timestamp.
    let q = format!("{EVENTS}q = ev & select {{ x = addHours 1 .d }}\n");
    let e = dialect(&q, "q", "ansi").unwrap_err();
    assert!(e.contains("timestamp"), "{e}");
}

#[test]
fn unknown_intrinsic_is_an_error() {
    // A `sql` template naming a `CAGARA_*` function the backend does not
    // know must be a diagnostic, not invalid SQL passed through verbatim.
    let q = "nope : expr r int -> expr r int = sql \"CAGARA_NOPE($1)\"\n\
             q = users & select { v = nope .id }\n";
    let e = dialect(q, "q", "ansi").unwrap_err();
    assert!(e.contains("CAGARA_NOPE"), "{e}");
}

#[test]
fn data_last_functions_compose() {
    // Partial applications are reusable transformations.
    let q = format!(
        "{EVENTS}nextWeek = addDays 7 >>> truncWeek\n\
         clean = trim >>> lower >>> replaceAll \"-\" \"\"\n\
         q = ev & where (contains \"@\" .s) & select {{ w = nextWeek .d, c = clean .s, t = .s & left 3 }}\n"
    );
    let s = dialect(&q, "q", "postgres").unwrap();
    assert!(s.contains("STRPOS(s, '@') > 0"), "{s}");
    assert!(
        s.contains("DATE_TRUNC('WEEK', (d + 7 * INTERVAL '1' DAY)::DATE)::DATE AS w"),
        "{s}"
    );
    assert!(s.contains("REPLACE(LOWER(TRIM(s)), '-', '') AS c"), "{s}");
}

#[test]
fn operator_shorthands_match_the_long_forms() {
    let pairs = [
        ("q = users &? (.age >= 18) &= { id = .id, n = .name } &. [asc .n] &- 5\n",
         "q = users & where (.age >= 18) & select { id = .id, n = .name } & order [asc .n] & limit 5\n"),
        ("q = orders &* { u = group .user_id, total = sum .amount ?? 0.0 }\n",
         "q = orders & agg { u = group .user_id, total = coalesce 0.0 (sum .amount) }\n"),
        ("q = orders & users ? .<user_id == .>id &= { a = .amount, n = .name }\n",
         "q = orders & inner users (.<user_id == .>id) & select { a = .amount, n = .name }\n"),
        ("q = orders & users <? .<user_id == .>id &= { n = .name ?? \"?\" }\n",
         "q = orders & leftJoin users (.<user_id == .>id) & select { n = coalesce \"?\" .name }\n"),
        ("q = orders & users ?> .<user_id == .>id\n", "q = orders & rightJoin users (.<user_id == .>id)\n"),
        ("q = orders & users <?> .<user_id == .>id && .>active\n",
         "q = orders & fullJoin users (.<user_id == .>id && .>active)\n"),
    ];
    for (short, long) in pairs {
        assert_eq!(sql(short, "q"), sql(long, "q"), "{short}");
    }
    let s = sql(
        "q = orders & users <? .<user_id == .>id &= { n = .name ?? \"?\" }\n",
        "q",
    );
    assert!(
        s.contains("LEFT JOIN public.users AS t2 ON t1.user_id = t2.id"),
        "{s}"
    );
    assert!(s.contains("COALESCE(t2.name, '?') AS n"), "{s}");
}

#[test]
fn coalesce_operator_types() {
    // Chains and precedence: `.a ?? .b ?? 0`, `x ?? 0 + 1` is `(x ?? 0) + 1`.
    // In a full join both sides are nullable.
    let s = sql("q = orders & users <?> .<user_id == .>id &= { a = .age ?? .user_id ?? 0, b = .age ?? 0 + 1 }\n", "q");
    assert!(
        s.contains("COALESCE(t2.age, COALESCE(t1.user_id, 0)) AS a"),
        "{s}"
    );
    assert!(s.contains("COALESCE(t2.age, 0) + 1 AS b"), "{s}");
    // The left side must be nullable, the default must not be.
    assert!(error("q = users &= { a = .age ?? 0 }\n", "q").contains("maybe"));
    assert!(error(
        "q = orders & users <? .<user_id == .>id &= { a = .age ?? .user_id ?? 0 }\n",
        "q"
    )
    .contains("maybe"));
}

#[test]
fn shorthand_errors_point_at_the_stage() {
    let src = format!("{USERS}q = users\n  &? (.salary > 1)\n");
    let ws = Workspace::from_source(&src);
    let d = root_queries(&ws)
        .into_iter()
        .find(|(k, _)| k == "q")
        .unwrap()
        .1
        .unwrap_err();
    assert!(d.message.contains("salary"), "{d}");
    assert_eq!(d.source.trim(), "&? (.salary > 1)", "{d}");
}

#[test]
fn null_extended_join_inputs_compute_before_the_join() {
    // A computed column of the side a join may null-extend is NULL on
    // unmatched rows only if it is computed before the join.
    let q = "q = users & leftJoin (orders & select { user_id = .user_id, one = 1 }) (.<id == .>user_id)\n  & select { m = isNull .one }\n";
    let s = sql(q, "q");
    assert!(
        s.contains("(SELECT user_id, 1 AS one FROM public.orders) AS t2"),
        "{s}"
    );
    assert!(s.contains("t2.one IS NULL"), "{s}");
    // The left side of a right join, and both sides of a full join.
    let r = "r = (users & select { id = .id, k = .id + 1 }) & rightJoin orders (.<id == .>user_id) & select { k = .k }\n";
    assert!(
        sql(r, "r").contains("(SELECT id, id + 1 AS k FROM public.users) AS t1"),
        "{}",
        sql(r, "r")
    );
    let f = "f = users & fullJoin (orders & select { user_id = .user_id, st = coalesce \"none\" (just .status) }) (.<id == .>user_id) & select { s = .st }\n";
    assert!(
        sql(f, "f").contains("COALESCE(status, 'none') AS st FROM public.orders) AS t2"),
        "{}",
        sql(f, "f")
    );
    // Bare columns still inline, and so does anything on a preserved side.
    let b = "b = users & leftJoin (orders & select { user_id = .user_id }) (.<id == .>user_id) & select { u = .user_id }\n";
    assert_eq!(
        sql(b, "b"),
        "SELECT t2.user_id AS u FROM public.users AS t1 LEFT JOIN public.orders AS t2 ON t1.id = t2.user_id"
    );
}

#[test]
fn constant_keys_are_not_positions() {
    // `ORDER BY 1` / `GROUP BY 2` would refer to output columns.
    let o = "o = users & select { a = .name, b = 1 } & order [asc .b, asc .a]\n";
    assert_eq!(
        sql(o, "o"),
        "SELECT name AS a, 1 AS b FROM public.users ORDER BY name NULLS LAST"
    );
    // A constant group key is dropped; HAVING keeps an empty input empty.
    let g = "g = users & select { c = 2, name = .name } & agg { c = group .c, n = count }\n";
    assert_eq!(
        sql(g, "g"),
        "SELECT 2 AS c, COUNT(*) AS n FROM public.users HAVING COUNT(*) > 0"
    );
    let g2 = "g2 = users & select { c = \"x\", a = .age } & agg { c = group .c, a = group .a, n = count }\n";
    assert_eq!(
        sql(g2, "g2"),
        "SELECT 'x' AS c, age AS a, COUNT(*) AS n FROM public.users GROUP BY age"
    );
}

#[test]
fn negative_numbers_do_not_make_comments() {
    let q = "q = users & select { x = negate (-5), y = - -1.5, z = .age - -1 }\n";
    let s = sql(q, "q");
    assert!(!s.contains("--"), "{s}");
    assert!(
        s.contains("-(-5) AS x") && s.contains("1.5 AS y") && s.contains("age - (-1) AS z"),
        "{s}"
    );
}

#[test]
fn identifiers_are_quoted_when_needed() {
    let src =
        "t : query { id = int, order = int, userId = int } = table \"my schema\" \"select\"\n\
               q = t & select { moved = .order, id = .id, userId = .userId }\n";
    assert_eq!(
        sql(src, "q"),
        "SELECT \"order\" AS moved, id, \"userId\" FROM \"my schema\".\"select\""
    );
    // A column name that looks like SQL is quoted, never emitted bare.
    let src =
        "t : query { id = int, order = int, userId = int } = table \"my schema\" \"select\"\n\
               q = t & select { x = .order }\n";
    assert_eq!(
        sql(src, "q"),
        "SELECT \"order\" AS x FROM \"my schema\".\"select\""
    );
    // The quote character itself is doubled, in each dialect's quotes.
    let src = "t : query { id = int } = table \"a`b\" \"c\\\"d\"\nq = t & update { x = .id }\n";
    assert_eq!(sql(src, "q"), "SELECT id, id AS x FROM \"a`b\".\"c\"\"d\"");
    assert_eq!(
        dialect(src, "q", "mysql").unwrap(),
        "SELECT id, id AS x FROM `a``b`.`c\"d`"
    );
    assert_eq!(
        dialect(src, "q", "tsql").unwrap(),
        "SELECT id, id AS x FROM [a`b].[c\"d]"
    );
}

#[test]
fn backslashes_are_escaped_where_they_are_escapes() {
    // In MySQL `'a\''` is an unterminated string: `\'` escapes the quote.
    let q = "q = users & where (.name == \"a\\\\' OR 1=1 -- \") & select { id = .id }\n";
    assert_eq!(
        dialect(q, "q", "postgres").unwrap(),
        "SELECT id FROM public.users WHERE (name = 'a\\'' OR 1=1 -- ')"
    );
    for d in ["mysql", "bigquery", "snowflake", "spark"] {
        let s = dialect(q, "q", d).unwrap();
        assert!(s.contains("'a\\\\'' OR 1=1 -- '"), "{d}: {s}");
    }
    // Also inside a window spec, which `Expr::transform` skips.
    let w = "w = users & select { id = .id, rn = rowNumber { partition = [.name <> \"\\\\\"] } }\n";
    let s = dialect(w, "w", "mysql").unwrap();
    assert!(s.contains("CONCAT(name, '\\\\')"), "{s}");
}

#[test]
fn intrinsics_are_lowered_inside_window_specs() {
    let w = "w = orders & select { id = .id, rn = rowNumber { partition = [year .created_at], order = [asc .id] } }\n";
    let s = sql(w, "w");
    assert!(!s.contains("CAGARA_"), "{s}");
    assert!(
        s.contains("PARTITION BY CAST(EXTRACT(YEAR FROM created_at) AS INT)"),
        "{s}"
    );
}

#[test]
fn order_survives_a_derived_table() {
    // `where` on a window needs a derived table; SQL does not keep its order,
    // so the outer query sorts again, by a hidden column when the key is
    // not an output.
    let q = "q = users & order [desc .age] & select { id = .id, rn = rowNumber { order = [asc .id] } } & where (.rn <= 3)\n";
    assert_eq!(
        sql(q, "q"),
        "SELECT id, rn FROM (SELECT id, ROW_NUMBER() OVER (ORDER BY id NULLS LAST) AS rn, age AS __k1 \
         FROM public.users) AS t2 WHERE (rn <= 3) ORDER BY __k1 DESC NULLS LAST"
    );
    // With a LIMIT the inner query keeps it too.
    let p = "p = users & order [asc .name] & limit 3 & where (.age > 1) & select { n = .name }\n";
    assert_eq!(
        sql(p, "p"),
        "SELECT name AS n FROM (SELECT id, name, age, active FROM public.users ORDER BY name NULLS LAST LIMIT 3) AS t1 \
         WHERE (age > 1) ORDER BY name NULLS LAST"
    );
    // A join does not keep its inputs' order: no ORDER BY in a join input.
    let j = "j = (users & order [asc .name] & select { id = .id, n = .name }) & inner orders (.<id == .>user_id) & select { n = .n }\n";
    assert!(!sql(j, "j").contains("ORDER BY"), "{}", sql(j, "j"));
}

#[test]
fn template_placeholders_must_stand_alone() {
    let bad = [
        (
            "f : expr r int -> expr r int = sql \"x$1\"",
            "must stand alone",
        ),
        (
            "f : expr r int -> expr r string = sql \"'$1'\"",
            "must stand alone",
        ),
        // Caught at the definition already.
        (
            "f : expr r int -> expr r int = sql \"$2 + 1\"",
            "placeholders up to $2",
        ),
    ];
    for (f, msg) in bad {
        let e = error(&format!("{f}\nq = users & select {{ y = f .age }}\n"), "q");
        assert!(e.contains(msg), "{f}: {e}");
    }
}

#[test]
fn tsql_conditions_as_columns_become_bits() {
    let q = "q = users & select { e = .age > 1, n = isNull (just .name), a = .active }\n";
    let s = dialect(q, "q", "tsql").unwrap();
    assert!(
        s.contains("CASE WHEN age > 1 THEN 1 WHEN NOT (age > 1) THEN 0 END AS e"),
        "{s}"
    );
    assert!(
        s.contains("CASE WHEN name IS NULL THEN 1 WHEN NOT (name IS NULL) THEN 0 END AS n"),
        "{s}"
    );
    // A bool column already is a bit.
    assert!(s.contains(", active AS a FROM"), "{s}");
}

#[test]
fn trino_and_spark_spellings() {
    let src = "ev : query { i = int, j = int, s = string, d = date, t = timestamp } = table \"p\" \"ev\"\n";
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            ".i / .j",
            &[
                ("trino", "(i / j)"),
                ("spark", "DIV(i, j)"),
                ("duckdb", "DIVIDE(i, j)"),
                ("bigquery", "DIV(i, j)"),
                ("mysql", "CAST(((i - (i % j)) / j) AS SIGNED)"),
            ],
        ),
        (".i % .j", &[("bigquery", "MOD(i, j)")]),
        (
            "dayOfWeek .d",
            &[
                ("trino", "(EXTRACT(DOW FROM d) % 7)"),
                ("spark", "(EXTRACT(DOW FROM d) - 1)"),
            ],
        ),
        (
            "addMonths 1 .d",
            &[
                ("trino", "DATE_ADD('month', 1, d)"),
                ("spark", "ADD_MONTHS(d, 1)"),
            ],
        ),
        (
            "addDays 2 .t",
            &[("spark", "(t + MAKE_INTERVAL(0, 0, 0, 2, 0, 0, 0))")],
        ),
        (
            "truncMonth .d",
            &[
                ("trino", "DATE_TRUNC('MONTH', d)"),
                ("spark", "TRUNC(d, 'MONTH')"),
            ],
        ),
        (
            "daysBetween .d .d",
            &[
                ("trino", "DATE_DIFF('day', d, d)"),
                ("spark", "DATEDIFF(d, d)"),
            ],
        ),
        (
            "right 2 .s",
            &[("trino", "SUBSTRING(s, GREATEST(LENGTH(s) - 2 + 1, 1))")],
        ),
        (
            "toString .i",
            &[
                ("trino", "CAST(i AS VARCHAR)"),
                ("spark", "CAST(i AS STRING)"),
            ],
        ),
    ];
    for (e, want) in cases {
        let q = format!("{src}q = ev & select {{ x = {e} }}\n");
        for (d, frag) in *want {
            let s = dialect(&q, "q", d).unwrap_or_else(|m| panic!("{e} / {d}: {m}"));
            assert!(s.contains(frag), "{e} / {d}: {s}");
        }
    }
}

#[test]
fn omit_drops_a_column_in_place() {
    assert_eq!(
        sql("q = users & omit \"age\"\n", "q"),
        "SELECT id, name, active FROM public.users"
    );
    // The columns that stay keep their positions.
    assert_eq!(
        sql("q = users & omit \"name\"\n", "q"),
        "SELECT id, age, active FROM public.users"
    );
    // A later stage reads the row the equation produced.
    let s = sql("q = users & omit \"age\" & where (.active)\n", "q");
    assert!(s.contains("WHERE"), "{s}");
    assert!(!s.contains("age"), "{s}");
}

#[test]
fn map_keys_renames_by_pattern() {
    // Prefix, suffix, and a single rename are one stage with different
    // arguments.
    assert_eq!(
        sql("q = users & mapKeys \"^\" \"u_\"\n", "q"),
        "SELECT id AS u_id, name AS u_name, age AS u_age, active AS u_active FROM public.users"
    );
    assert_eq!(
        sql("q = users & mapKeys \"$\" \"_v2\"\n", "q"),
        "SELECT id AS id_v2, name AS name_v2, age AS age_v2, active AS active_v2 FROM public.users"
    );
    assert_eq!(
        sql("q = users & mapKeys \"^id$\" \"user_id\"\n", "q"),
        "SELECT id AS user_id, name, age, active FROM public.users"
    );
}
