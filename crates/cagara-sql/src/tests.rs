//! End-to-end tests: Cagara source -> SQL (ANSI).

use crate::{compile, compile_located, Dialect, Options};
use cagara_hir::{root_queries, Workspace};
use std::collections::HashMap;

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

/// A refusal that names the stage it is about.
///
/// Every user-triggerable failure in this crate used to reach the user as `0:0` —
/// `cagara check` printed `file.cagara:0:0: error: error in `q`: …` — because `Rel`
/// is built bottom-up and only `Rel::At` remembers where a stage was written. A
/// diagnostic at the top of the file points at the wrong place, and an editor puts
/// the squiggle there.
///
/// The location has to be the *innermost* stage: a `distinct` that cannot follow an
/// `order` is about the `distinct`, not about the table the pipeline starts from.
#[test]
fn a_refusal_names_the_stage_it_is_about() {
    let full = format!("{USERS}q = users & order [asc (.age + 1)] & select {{.age}} & distinct\n");
    let ws = Workspace::from_source(&full);
    let (_, r) = root_queries(&ws)
        .into_iter()
        .find(|(n, _)| n == "q")
        .expect("q");
    let rel = r.expect("q type-checks");
    let (loc, message) = compile_located(&rel, Options::default())
        .expect_err("a `distinct` after a dropped sort key is refused");
    assert!(message.contains("cannot follow an `order`"), "{message}");
    let loc = loc.expect("the refusal carries a location");
    // The span is a *stage*, not a token: `select {.age}` is what the `distinct`
    // cannot follow, and underlining it is more useful than underlining the word
    // `distinct` — the reader has to change one of the two, and this is the one with
    // the sort key in it. What matters is that it is *in the query* rather than at
    // `0:0`.
    let text = &full[loc.span.start as usize..loc.span.end as usize];
    assert!(
        text.contains("select") || text.contains("distinct"),
        "the span names a stage of the query, not the file: {text:?}"
    );
    assert!(loc.span.start > 0, "not the top of the file");
}

/// A dialect refusal names the construct, not the query.
///
/// `fullJoin` used to be the example here. It is emulated now, so the example is a
/// construct that genuinely has no equivalent: a `groups` frame, whose unit no
/// rewrite can stand in for.
#[test]
fn a_dialect_refusal_names_the_construct() {
    let full = format!(
        "{USERS}q = users & select {{ s = over {{ order = [asc .id], \
         frame = groups (preceding 1) currentRow }} (count) }}\n"
    );
    let ws = Workspace::from_source(&full);
    let (_, r) = root_queries(&ws)
        .into_iter()
        .find(|(n, _)| n == "q")
        .expect("q");
    let rel = r.expect("q type-checks");
    let (loc, message) = compile_located(
        &rel,
        Options {
            dialect: Dialect::Mysql,
            ..Options::default()
        },
    )
    .expect_err("MySQL has no GROUPS frame");
    assert!(message.contains("`groups` window frame"), "{message}");
    let loc = loc.expect("the refusal carries a location");
    let text = &full[loc.span.start as usize..loc.span.end as usize];
    assert!(
        text.contains("frame = groups"),
        "the span covers the window that refused: {text:?}"
    );
    assert!(loc.span.start > 0, "not the top of the file");
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

/// The generated relation-alias prefix `word` is built from, if any.
fn alias_prefix(word: &str) -> Option<&'static str> {
    for prefix in ["cagara_cte", "t", "__k"] {
        if let Some(rest) = word.strip_prefix(prefix) {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                return Some(prefix);
            }
        }
    }
    None
}

/// Rename machine-chosen relation aliases to a stable scheme.
///
/// `Lowerer` numbers derived tables (`t1`, `t2`) and hidden window keys
/// (`__k1`) in traversal order. Pinning those numbers makes any unrelated
/// change to the walk break a dozen tests at once, while dropping them would
/// stop the tests from noticing that two relations traded places. This maps
/// each alias to `prefix#n` in order of first appearance, so which relation
/// comes first and which references which are still compared; only the chosen
/// numbers are ignored.
fn normalize_aliases(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut next: HashMap<&'static str, usize> = HashMap::new();
    let mut word = String::new();
    let mut in_string = false;
    for c in sql.chars() {
        if c == '\'' {
            in_string = !in_string;
        }
        if !in_string && (c.is_ascii_alphanumeric() || c == '_') {
            word.push(c);
            continue;
        }
        push_normalized(&mut out, &mut word, &mut seen, &mut next);
        out.push(c);
    }
    push_normalized(&mut out, &mut word, &mut seen, &mut next);
    out
}

fn push_normalized(
    out: &mut String,
    word: &mut String,
    seen: &mut HashMap<String, String>,
    next: &mut HashMap<&'static str, usize>,
) {
    if word.is_empty() {
        return;
    }
    match alias_prefix(word) {
        Some(prefix) => {
            let name = seen.entry(word.clone()).or_insert_with(|| {
                let n = next.entry(prefix).or_insert(0);
                let name = format!("{prefix}#{n}");
                *n += 1;
                name
            });
            out.push_str(name);
        }
        None => out.push_str(word),
    }
    word.clear();
}

/// [`assert_eq!`] for generated SQL, ignoring the aliases `Lowerer` numbers.
#[track_caller]
fn assert_sql(actual: &str, expected: &str) {
    assert_eq!(normalize_aliases(actual), normalize_aliases(expected));
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
fn filter_pipeline_over_ten_stages_compiles() {
    let mut src = String::from("q = users");
    for n in 0..40 {
        src.push_str(&format!(" & where (.age > {n})"));
    }
    src.push('\n');
    let s = sql(&src, "q");
    assert_eq!(s.matches("age >").count(), 40, "{s}");
}

#[test]
fn a_filter_over_an_aggregate_is_having() {
    // The predicate is written against an aggregate output, so it belongs in
    // `HAVING` — where every column it can name is a grouping key or an
    // aggregate value — and the query stays one statement.
    let s = sql(
        "q = orders\n  & where (.status == \"paid\")\n  & agg { user_id = group .user_id, revenue = sum .amount, n = count }\n  & where (.n >= 5)\n",
        "q",
    );
    assert!(s.contains("GROUP BY user_id"), "{s}");
    assert!(s.contains("SUM(amount) AS revenue"), "{s}");
    assert!(s.contains("HAVING COUNT(*) >= 5"), "{s}");
    assert!(!s.contains(") AS t1 WHERE"), "no derived table: {s}");
}

#[test]
fn filter_after_agg_stays_outside_paging() {
    // A `limit` runs after grouping, so a filter written after it must run
    // after the paging — a derived table, not `HAVING`.
    let paged = sql(
        "q = orders & agg { u = group .user_id, n = count } & limit 5 & where (.n > 3)\n",
        "q",
    );
    assert!(paged.contains(") AS t1 WHERE"), "{paged}");
    assert!(!paged.contains("HAVING"), "{paged}");

    // A stage with both an aggregate and a `DISTINCT` wraps too — the aggregate
    // is what blocks the filter, not the dedupe.
    let distinct = sql(
        "q = orders & agg { u = group .user_id, n = count } & distinct & where (.n > 3)\n",
        "q",
    );
    assert!(distinct.contains(") AS t1 WHERE"), "{distinct}");

    // A bare `DISTINCT` does not block it: deduping whole rows and filtering on
    // them commute, since two identical rows agree on every predicate.
    let bare = sql(
        "q = users & select {.name} & distinct & where (.name != \"bob\")\n",
        "q",
    );
    assert_eq!(
        bare,
        "SELECT DISTINCT name FROM public.users WHERE (name <> 'bob')"
    );

    // `order` does not: it runs after `HAVING`, so both stay in one statement.
    let ordered = sql(
        "q = orders & agg { u = group .user_id, n = count } & order [desc .n] & where (.n > 3)\n",
        "q",
    );
    assert!(ordered.contains("HAVING COUNT(*) > 3"), "{ordered}");
    assert!(ordered.contains("ORDER BY"), "{ordered}");
}

#[test]
fn consecutive_aggregate_filters_conjoin_in_having() {
    // Conjunction is a property of consecutive filters on the *same* stage,
    // and the stage here is the group.
    let s = sql(
        "q = orders & agg { u = group .user_id, n = count } & where (.n > 3) & where (.n < 9)\n",
        "q",
    );
    assert!(
        s.contains("HAVING (COUNT(*) > 3) AND (COUNT(*) < 9)"),
        "consecutive filters should conjoin: {s}"
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
        "q = orders & select { id = .id, running = over { partition = [.user_id], order = [asc .created_at], frame = runningFrame } (sum .amount) }\n",
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
        "q = orders & select { order_id = .id, user_id = .user_id } & innerJoin (users & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id)\n  & select { order_id = .order_id, name = .name }\n",
        "q",
    );
    assert!(s.contains("INNER JOIN public.users AS t2"), "{s}");
    assert!(s.contains("t1.user_id = t2.id"), "{s}");
}

#[test]
fn case_lowers_to_a_searched_case() {
    // Three arms: one `WHEN` per condition, and the trailing `true =>` arm
    // becomes the `ELSE`.
    assert_eq!(
        sql(
            "q = users & select { b = case { .age > 40 => \"old\"; .age > 20 => \"mid\"; true => \"young\" } }\n",
            "q"
        ),
        "SELECT CASE WHEN (age > 40) THEN 'old' WHEN (age > 20) THEN 'mid' ELSE 'young' END AS b FROM public.users"
    );
    // A lone `true =>` arm is just its value: there is no condition to test,
    // so no `CASE` is emitted.
    assert_eq!(
        sql(
            "q = users & select { b = case { true => \"only\" } }\n",
            "q"
        ),
        "SELECT 'only' AS b FROM public.users"
    );
    // A `case` is an expression like any other, so it works as a `where`
    // predicate...
    assert_eq!(
        sql(
            "q = users & where (case { .active => true; true => .age > 18 })\n",
            "q"
        ),
        "SELECT id, name, age, active FROM public.users WHERE CASE WHEN active THEN TRUE ELSE (age > 18) END"
    );
    // ...and its arms may be aggregates, in which case the whole `case` is
    // one, exactly as `ifThenElse` over `sum` is.
    assert_eq!(
        sql(
            "q = users & agg { n = count, b = case { count > 3 => coalesce 0 (sum .age); true => 0 } }\n",
            "q"
        ),
        "SELECT COUNT(*) AS n, CASE WHEN (COUNT(*) > 3) THEN COALESCE(SUM(age), 0) ELSE 0 END AS b FROM public.users"
    );
}

#[test]
fn case_is_nested_and_dialect_independent() {
    // A `case` inside a `case`'s arm, and inside a record field, both lower
    // through the ordinary expression path.
    assert_eq!(
        sql(
            "q = users & select { b = case { .active => case { .age > 40 => \"a\"; true => \"b\" }; true => \"c\" } }\n",
            "q"
        ),
        "SELECT CASE WHEN active THEN CASE WHEN (age > 40) THEN 'a' ELSE 'b' END ELSE 'c' END AS b FROM public.users"
    );
    // Every supported dialect spells a searched `CASE` the same way, so no
    // rewrite is needed and none is applied.
    let src = "q = users & select { b = case { .age > 40 => \"old\"; true => \"young\" } }\n";
    let ansi = sql(src, "q");
    for d in ["postgres", "mysql", "tsql", "sqlite", "trino", "snowflake"] {
        assert_eq!(dialect(src, "q", d).unwrap(), ansi, "{d}");
    }
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
    assert!(s.contains("CAST(id AS DOUBLE PRECISION) AS n"), "{s}");
}

#[test]
fn empty_membership_is_a_constant_false() {
    let s = sql("q = users & where (inList [] .id)\n", "q");
    assert!(s.contains("WHERE FALSE"), "{s}");
    let tsql = dialect("q = users & where (inList [] .id)\n", "q", "tsql").unwrap();
    assert!(tsql.contains("WHERE 1 = 0"), "{tsql}");
}

#[test]
fn nullable_aggregates_keep_sql_null_semantics() {
    let s = sql(
        r#"nulls : query { x = maybe int } = table "public" "nulls"
q = nulls & agg {
  s = sum .x, a = avg .x, lo = min .x, hi = max .x,
  n = countOfMaybe .x, d = countDistinctMaybe .x
}
"#,
        "q",
    );
    // A nullable column takes the same aggregate name and passes straight
    // through: SQL already skips NULL in these aggregates. Coalescing first
    // would change `avg` (and `min`/`max`) rather than merely satisfy the
    // checker.
    for expected in [
        "SUM(x) AS s",
        "AVG(x) AS a",
        "MIN(x) AS lo",
        "MAX(x) AS hi",
        "COUNT(x) AS n",
        "COUNT(DISTINCT x) AS d",
    ] {
        assert!(s.contains(expected), "missing `{expected}` in {s}");
    }
}

#[test]
fn casts_use_the_language_widths() {
    let s = sql(
        "q = users & select { i = toInt (toFloat .id), f = toFloat .id }\n",
        "q",
    );
    assert!(s.contains("AS BIGINT"), "{s}");
    assert!(s.contains("AS DOUBLE PRECISION"), "{s}");
    assert!(!s.contains("AS INT)"), "{s}");
    assert!(!s.contains("AS FLOAT)"), "{s}");
}

#[test]
fn let_bindings_are_inlined() {
    let s = sql(
        "q = users & select { x = let y = .age in y + 1, z = let a = .age in let b = 2 in a + b }\n",
        "q",
    );
    assert!(s.contains("age + 1 AS x"), "{s}");
    assert!(s.contains("age + 2 AS z"), "{s}");
    assert!(!s.contains("let"), "{s}");
}

#[test]
fn nothing_is_sql_null() {
    let s = sql(
        "q = users & select { x = nothing, y = coalesce 0 nothing }\n",
        "q",
    );
    assert!(s.contains("NULL AS x"), "{s}");
    assert!(s.contains("COALESCE(NULL, 0) AS y"), "{s}");
}

/// `fullJoin` on the MySQL family: the two outer halves, unioned.
///
/// MySQL has no `FULL [OUTER] JOIN`, and refusing the query used to be the answer —
/// with a message telling the reader to write `leftJoin`/`rightJoin` and
/// `unionAll` themselves. That is exactly what the compiler can write for them, so
/// it does, and the language stops depending on the target.
///
/// The second branch is the right rows with no match, and it is an anti-join rather
/// than `RIGHT JOIN … WHERE <left> IS NULL`: a matched row whose every column is
/// NULL would be kept twice by the latter.
#[test]
fn full_join_is_emulated_for_the_mysql_family() {
    let src = "q = orders & fullJoin (users & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id)\n";
    let m = dialect(src, "q", "mysql").expect("MySQL can express a full join as a union");
    assert!(m.contains("LEFT JOIN"), "{m}");
    assert!(m.contains("UNION ALL"), "{m}");
    assert!(!m.contains("FULL JOIN"), "{m}");
    assert!(m.contains("NOT EXISTS"), "{m}");
    // The predicate is the same one with the sides exchanged, because the anti-join
    // reads the right input as its left.
    assert!(m.contains("t4.user_id = t3.id"), "{m}");
    // And a target that has the join keeps it.
    let p = dialect(src, "q", "postgres").expect("postgres");
    assert!(p.contains("FULL JOIN"), "{p}");
    assert!(!p.contains("UNION ALL"), "{p}");
}

#[test]
fn math_helpers_are_available() {
    let s = sql(
        r#"q = users & select {
  a = abs .age, b = roundTo 2 (toFloat .age), c = floor (toFloat .age),
  d = ceil (toFloat .age), e = sqrt (toFloat .age),
  f = power 2 (toFloat .age), g = greatest .age 18, h = least .age 18,
  i = toString .active
}
"#,
        "q",
    );
    for expected in [
        "ABS(age)",
        "ROUND(CAST(age AS DOUBLE PRECISION), 2)",
        "FLOOR(CAST(age AS DOUBLE PRECISION))",
        "CEIL(CAST(age AS DOUBLE PRECISION))",
        "SQRT(CAST(age AS DOUBLE PRECISION))",
        "CASE WHEN age >= 18 THEN age ELSE 18 END",
        "CASE WHEN active THEN 'true' ELSE 'false' END",
    ] {
        assert!(s.contains(expected), "missing `{expected}` in {s}");
    }
}

#[test]
fn analytic_and_try_date_helpers_are_available() {
    let s = sql(
        r#"q = users & select {
  f = firstValue {order = [asc .id]} .name,
  l = lastValue {order = [asc .id]} .name,
  n = ntile {order = [asc .id]} 4,
  p = percentRank {order = [asc .id]},
  c = cumeDist {order = [asc .id]}
}
"#,
        "q",
    );
    for expected in [
        "FIRST_VALUE(name)",
        "LAST_VALUE(name)",
        "NTILE(4)",
        "PERCENT_RANK()",
        "CUME_DIST()",
    ] {
        assert!(s.contains(expected), "missing `{expected}` in {s}");
    }
    let casts = sql(
        "q = users & select { d = tryDate .name, ts = tryTimestamp .name }\n",
        "q",
    );
    // The default dialect is ANSI, which guards instead of casting loosely; each guard
    // is the shape a value of that type has.
    assert!(
        casts.contains("REGEXP_LIKE(name, '^[0-9]{4}-[0-9]{2}-[0-9]{2}$')")
            && casts.contains("AS DATE)"),
        "{casts}"
    );
    assert!(
        casts.contains("^[0-9]{4}-[0-9]{2}-[0-9]{2}[ T][0-9]{2}:[0-9]{2}:[0-9]{2}$")
            && casts.contains("AS TIMESTAMP)"),
        "{casts}"
    );
    // The cast is applied to a value the guard accepted, so PostgreSQL cannot fold it
    // into a planning-time failure — `CASE WHEN 'abc' ~ … THEN 'abc'::DATE END` raises
    // before the condition runs.
    assert!(
        casts.contains("THEN CASE WHEN REGEXP_LIKE") || casts.contains("ELSE '0' END AS DATE"),
        "the cast is guarded against constant folding: {casts}"
    );
    // A target with `TRY_CAST` keeps it.
    let duck = dialect(
        "q = users & select { d = tryDate .name, ts = tryTimestamp .name }\n",
        "q",
        "duckdb",
    )
    .expect("duckdb");
    assert!(duck.contains("TRY_CAST(name AS DATE)"), "{duck}");
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

/// `except` is not commutative, so it is the set operation that shows whether
/// the piped and direct forms agree. They must: `users &~ admins` and
/// `except admins users` are the same query, with `users` on the left.
///
/// Every stage takes its subject last, and for a set operation the subject is
/// the left operand. `users & except admins` desugars to
/// `except admins users`, so reading the *first* argument as the left operand
/// turned it into `admins EXCEPT users` — the complement of what was asked
/// for. `union`, `unionAll`, and `intersect` are commutative and hid it.
///
/// `admins` has `users`' row because a set operation requires it.
const ADMINS: &str = "admins : query { id = int, name = string, age = int, active = bool } = table \"public\" \"admins\"\n";

fn sql_admins(src: &str, name: &str) -> String {
    sql_with(&format!("{ADMINS}{src}"), name, Options::default())
}

#[test]
fn a_piped_except_is_not_reversed() {
    let piped = sql_admins("q = users & except admins\n", "q");
    let direct = sql_admins("q = except admins users\n", "q");
    assert_eq!(piped, direct, "piped and direct forms must agree");

    let inner = piped
        .find("(SELECT")
        .unwrap_or_else(|| panic!("no derived table: {piped}"));
    let left = piped[inner..]
        .find("public.users")
        .unwrap_or_else(|| panic!("`users` is not an operand: {piped}"));
    let right = piped[inner..]
        .find("public.admins")
        .unwrap_or_else(|| panic!("`admins` is not an operand: {piped}"));
    assert!(
        left < right,
        "`users` must be the left operand of `EXCEPT`: {piped}"
    );
}

/// All four set operations are available through the pipe, and each puts the
/// piped query on the left. Only `except`/`intersect` can observe the order,
/// but pinning all four keeps a future re-declaration from dropping one.
#[test]
fn every_set_operation_has_a_pipe_stage() {
    for (op, sql_op) in [
        ("& union", "UNION"),
        ("& unionAll", "UNION ALL"),
        ("& intersect", "INTERSECT"),
        ("& except", "EXCEPT"),
    ] {
        let piped = sql_admins(&format!("q = users {op} admins\n"), "q");
        assert!(
            piped.contains(sql_op),
            "`{op}` must lower to `{sql_op}`: {piped}"
        );
        // The piped input is the left operand for every one of them.
        let start = piped.find("(SELECT").unwrap_or(0);
        let users = piped[start..].find("public.users").expect("users operand");
        let admins = piped[start..]
            .find("public.admins")
            .expect("admins operand");
        assert!(users < admins, "`{op}` reversed its operands: {piped}");
    }
}

/// A branch of a set operation that carries its own `order` is wrapped.
///
/// The set operator's tail belongs to the whole operation, so an `ORDER BY`
/// written inside one branch cannot be emitted flat: SQLite rejects
/// `SELECT … ORDER BY x UNION SELECT …` with "ORDER BY clause should come after
/// UNION not before", and the other engines read it as the operation's own
/// ordering rather than the branch's. Only `limit`/`offset` used to force the
/// wrap, so an ordering-only branch produced SQL no engine accepts.
#[test]
fn a_set_branch_with_its_own_order_is_wrapped() {
    let s = sql_admins(
        "q = (users & order [asc .id]) & union (admins & select { .id, .name, .age, .active })\n",
        "q",
    );
    // The ordering must sit inside a derived table, not beside the set operator.
    let union = s.find("UNION").expect("a UNION");
    let order = s.find("ORDER BY").expect("an ORDER BY");
    assert!(
        order < union,
        "the branch's ORDER BY must be inside its own SELECT: {s}"
    );
    assert!(
        s[..union].contains("(SELECT"),
        "the ordered branch must be a derived table: {s}"
    );
}

#[test]
fn union_all_preserves_duplicates() {
    let s = sql("q = unionAll users users\n", "q");
    assert!(s.contains("UNION ALL"), "{s}");
}

/// `x == nothing` is `IS NULL`, and `x != nothing` is `IS NOT NULL`.
///
/// SQL cannot compare against NULL: `x = NULL` and `x <> NULL` are NULL for
/// every row, so a `where` on either keeps nothing — which is the *opposite* of
/// what `where (.m != nothing)` reads as. The comparison is rewritten to the
/// predicate SQL has, and the rewrite is structural (the `nothing` definition's
/// template is exactly `NULL`), so an alias of `nothing` works too.
#[test]
fn a_comparison_against_nothing_is_a_null_test() {
    let t = "t : query { id = int, m = maybe int } = table \"public\" \"t\"\n";
    let eq = sql(
        &format!("{t}q = t & select {{ .id, is_null = .m == nothing }}\n"),
        "q",
    );
    assert!(eq.contains("m IS NULL"), "{eq}");
    assert!(
        !eq.contains("= NULL"),
        "the comparison must not survive: {eq}"
    );
    let ne = sql(
        &format!("{t}q = t & select {{ .id, present = .m != nothing }}\n"),
        "q",
    );
    assert!(ne.contains("m IS NOT NULL"), "{ne}");
    // In a `where`, which is where the wrong answer was visible as zero rows.
    let w = sql(&format!("{t}q = t & where (.m != nothing)\n"), "q");
    assert!(w.contains("WHERE (m IS NOT NULL)"), "{w}");
    // An alias of `nothing` is the same value.
    let alias = sql(
        &format!("{t}n = nothing\nq = t & select {{ .id, p = .m != n }}\n"),
        "q",
    );
    assert!(alias.contains("m IS NOT NULL"), "{alias}");
}

/// `rename` is one stage that emits the `AS` of an `update`.
///
/// It desugars to `update { to = .from } & omit "from"` in the normalizer, so the
/// SQL is what those two stages already emit — there is no new lowering, and the
/// interesting part is that the *checks* happen once, at the stage the reader
/// wrote.
#[test]
fn rename_moves_a_column() {
    let src = "t : query { id = int, age = int } = table \"p\" \"t\"\n\
               q = t & rename \"years\" \"age\"\n";
    let s = sql(src, "q");
    assert!(s.contains("age AS years"), "{s}");
    assert!(!s.contains("age,"), "the old name is gone: {s}");

    // Two in a row are two renames, and the second sees the first's output.
    let src = "t : query { id = int, age = int } = table \"p\" \"t\"\n\
               q = t & rename \"years\" \"age\" & rename \"ident\" \"id\"\n";
    let s = sql(src, "q");
    assert!(s.contains("age AS years"), "{s}");
    assert!(s.contains("id AS ident"), "{s}");
}

/// A join renames a shared column instead of dropping it.
///
/// The right column used to be unreachable: `.<x`/`.>x` name the inputs only
/// inside the predicate, so after the join there was no way to ask for the right
/// side's `id`, and `.id` silently meant the left one. A warning said so and did
/// not prevent it.
///
/// It is `right_<name>` now — the prefix the old warning suggested, so a reader
/// following that advice and one letting the compiler do it get the same query.
/// A name already taken is prefixed again rather than numbered, because `id_2`
/// says nothing about why and its number depends on how many joins came before.
#[test]
fn a_join_renames_a_shared_column() {
    let src = "u : query { id = int, name = string } = table \"p\" \"u\"\n\
               o : query { id = int, user_id = int, amount = float } = table \"p\" \"o\"\n\
               q = u & innerJoin o (.<id == .>user_id) & select { .id, .right_id, .amount }\n";
    let s = sql(src, "q");
    assert!(s.contains("t2.id AS right_id"), "{s}");
    // The left one is still the plain name, so nothing was shadowed.
    assert!(s.contains("t1.id"), "{s}");

    // A schema that already has `right_id` gets a second prefix rather than a
    // collision.
    let src = "a : query { id = int, right_id = int } = table \"p\" \"a\"\n\
               b : query { id = int } = table \"p\" \"b\"\n\
               q = a & innerJoin b (.<id == .>id) & select { .id, .right_id, .right_right_id }\n";
    let s = sql(src, "q");
    assert!(s.contains("t2.id AS right_right_id"), "{s}");
    assert!(s.contains("t1.right_id"), "{s}");
}

/// A join on a key an outer join made nullable.
///
/// SQL's own join semantics say `NULL = anything` is never true, which is exactly
/// what such a join wants: the NULL-extended rows stay unmatched. The predicate
/// is a condition, so a `maybe bool` is accepted, and `isTrue` turns the
/// comparison's `maybe bool` into one.
///
/// The trap this replaces is `.<bid ?? 0 == .>bid2`, which emits
/// `COALESCE(bid, 0)` and matches a NULL-extended row to a real row whose key is
/// 0 — a wrong answer that looks right.
#[test]
fn a_join_on_a_nullable_key_compares_null_as_never_matching() {
    let src = "a : query { id = int } = table \"s\" \"a\"\n\
               b : query { aid = int, bid = int } = table \"s\" \"b\"\n\
               c : query { bid2 = int } = table \"s\" \"c\"\n\
               t1 = a & leftJoin b (.<id == .>aid)\n\
               q = t1 & leftJoin c (isTrue (.<bid == just .>bid2))\n";
    let s = sql(src, "q");
    // `COALESCE(…, FALSE)` is the NULL-never-matches spelling; a bare `= NULL`
    // or a `COALESCE(key, 0)` would both be wrong.
    assert!(s.contains("COALESCE((t2.bid = t3.bid2), FALSE)"), "{s}");
    assert!(
        !s.contains("COALESCE(t2.bid, 0)"),
        "the wrong workaround: {s}"
    );
}

/// `filtered` and `distinctAgg` reach `arrayAgg`.
///
/// They used to be dropped: the modifier attached to the call, and the
/// `CAGARA_ARRAY_AGG` dispatch rebuilt the call from its arguments and lost it,
/// so `filtered p (arrayAgg .x)` emitted a plain `ARRAY_AGG(x)` with no
/// diagnostic. Every other aggregate honoured both, and `over` refused on this
/// one — so the prelude was inconsistent with itself and this was the half that
/// answered wrongly rather than loudly.
#[test]
fn array_agg_honours_the_aggregate_modifiers() {
    let t = "t : query { k = int, id = int, s = string, flag = int } = table \"public\" \"t\"\n";
    let f = sql(
        &format!("{t}q = t & agg {{ k = group .k, xs = filtered (.flag > 1) (arrayAgg .id) }}\n"),
        "q",
    );
    assert!(f.contains("FILTER (WHERE flag > 1)"), "{f}");
    let d = sql(
        &format!("{t}q = t & agg {{ k = group .k, xs = distinctAgg (arrayAgg .s) }}\n"),
        "q",
    );
    assert!(d.contains("ARRAY_AGG(DISTINCT s)"), "{d}");
    // The unmodified form is unchanged.
    let plain = sql(
        &format!("{t}q = t & agg {{ k = group .k, xs = arrayAgg .id }}\n"),
        "q",
    );
    assert!(plain.contains("ARRAY_AGG(id)"), "{plain}");
    assert!(
        !plain.contains("DISTINCT") && !plain.contains("FILTER"),
        "{plain}"
    );
}

#[test]
fn nullable_helpers_lower_to_portable_sql() {
    let s = sql(
        "q = users & select { same = eqMaybe (just .name) (just .name), cleared = nullIf .age 0, b = toBool .id }\n",
        "q",
    );
    assert!(s.contains("COALESCE"), "{s}");
    assert!(s.contains("NULLIF(age, 0)"), "{s}");
    assert!(s.contains("CAST(id AS BOOLEAN)"), "{s}");
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
    let s = sql("q = (users & limit 3) & union (users & limit 2)\n", "q");
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
        "q = (users & where (.age > 1)) & union (users & where (.age > 2))\n",
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
    let reused = "act = users & where .active & select { n = length .name, s = .name ++ \"!\" }\n\
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

/// Both sides of a semi/anti join are inlined where they can be.
///
/// The left keeps its cardinality and columns, so its filters are the subject's
/// and belong in the outer `WHERE`; the right only decides whether a match
/// exists, which is what a filter there means. Neither side is null-extended, so
/// a derived table protects nothing — and wrapping a plain table put
/// `FROM (SELECT id, name FROM users) AS t1` in front of a join that needs no
/// subquery.
#[test]
fn a_semi_join_inlines_its_inputs() {
    assert_eq!(
        sql("q = users & semiJoin orders (.<id == .>user_id)\n", "q"),
        "SELECT t1.id, t1.name, t1.age, t1.active FROM public.users AS t1 \
         WHERE (EXISTS (SELECT t2.id, t2.user_id, t2.amount, t2.status, t2.created_at \
         FROM public.orders AS t2 WHERE (t1.id = t2.user_id)))"
    );
    // A filter on either side stays a filter; on the left it is the subject's.
    let s = sql(
        "q = (users & where (.id > 1)) & antiJoin (orders & where (.amount > 5.0)) (.<id == .>user_id)\n",
        "q",
    );
    assert_sql(
        &s,
        "SELECT t1.id, t1.name, t1.age, t1.active FROM public.users AS t1 \
         WHERE (t1.id > 1) AND (NOT EXISTS (SELECT t2.id, t2.user_id, t2.amount, \
         t2.status, t2.created_at FROM public.orders AS t2 \
         WHERE (t2.amount > 5.0) AND (t1.id = t2.user_id)))",
    );
    // A stage a derived table is *for* keeps one: paging, dedupe, grouping.
    let s = sql(
        "q = users & semiJoin (orders & order [asc .id] & limit 3) (.<id == .>user_id)\n",
        "q",
    );
    assert!(s.contains("AS t2 WHERE (t1.id = t2.user_id)"), "{s}");
    assert!(s.contains("LIMIT 3"), "{s}");
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
fn filtered_aggregates_lower_to_filter_or_case() {
    let src = r#"q = orders & agg {
  u = group .user_id,
  paid = filtered (.status == "paid") (sum .amount),
  mean = filtered (.status == "paid") (avg .amount),
  n = filtered (.status == "paid") count,
  n_of = filtered (.status == "paid") (countOf .id),
  n_distinct = filtered (.status == "paid") (distinctAgg (countOf .id)),
  lo = filtered (.status == "paid") (min .amount),
  hi = filtered (.status == "paid") (max .amount)
}
"#;
    // A target with the standard `FILTER` clause reads the aggregate once.
    let s = sql(src, "q");
    for want in [
        "SUM(amount) FILTER (WHERE status = 'paid') AS paid",
        "AVG(amount) FILTER (WHERE status = 'paid') AS mean",
        "COUNT(*) FILTER (WHERE status = 'paid') AS n",
        "COUNT(id) FILTER (WHERE status = 'paid') AS n_of",
        "COUNT(DISTINCT id) FILTER (WHERE status = 'paid') AS n_distinct",
        "MIN(amount) FILTER (WHERE status = 'paid') AS lo",
        "MAX(amount) FILTER (WHERE status = 'paid') AS hi",
    ] {
        assert!(s.contains(want), "missing `{want}` in {s}");
    }
    assert!(!s.contains("CASE WHEN"), "{s}");
    // A target without it gets the portable spelling, which repeats the
    // predicate inside the argument.
    let m = dialect(src, "q", "mysql").expect("mysql");
    for want in [
        "SUM(CASE WHEN status = 'paid' THEN amount END) AS paid",
        "COUNT(CASE WHEN status = 'paid' THEN 1 END) AS n",
        "COUNT(DISTINCT CASE WHEN status = 'paid' THEN id END) AS n_distinct",
    ] {
        assert!(m.contains(want), "missing `{want}` in {m}");
    }
    assert!(!m.contains("FILTER"), "{m}");
}

#[test]
fn a_filtered_aggregate_is_a_value() {
    let direct = sql(
        "q = orders & agg { u = group .user_id, paid = filtered (.status == \"paid\") (sum .amount) }\n",
        "q",
    );
    let partial = sql(
        "paid = filtered (.status == \"paid\") (sum .amount)\nq = orders & agg { u = group .user_id, paid = paid }\n",
        "q",
    );
    assert_eq!(partial, direct);
}

#[test]
fn errors() {
    assert!(error("q = users & agg { t = sum (sum .age) }\n", "q").contains("cannot nest"));
    assert!(error("q = users & agg { n = count, name = .name }\n", "q").contains("not grouped"));
    assert!(error("q = users & where (count >= 1)\n", "q").contains("aggregate"));
    assert!(error("q = users & where (.salary > 1)\n", "q").contains("no column `salary`"));
    assert!(error(
        "q = orders & innerJoin (users & prefix \"u_\") (.user_id == .u_id)\n",
        "q"
    )
    .contains("which input"));
    assert!(error("q = users & select {.nope}\n", "q").contains("no column `nope`"));
    assert!(error("q : query { id = int } = q\n", "q").contains("refers to itself"));
    // A recursive helper is caught during normalization and named, whether it is
    // a lambda that calls itself or two helpers that call each other. It must
    // never reach the compiler budget as an internal/limit error. The failure
    // is reported at the *use*, since a helper has no query of its own.
    assert!(
        error("f = x => f x\nq = users & select { a = f .age }\n", "q")
            .contains("`f` refers to itself")
    );
    assert!(error(
        "f = x => g x\ng = x => f x\nq = users & select { a = f .age }\n",
        "q"
    )
    .contains("`f` refers to itself"));
    assert!(error(
        "q = users & agg { x = coalesce 0 (sum .age) + .age }\n",
        "q"
    )
    .contains("ungrouped"));
    assert!(error(
        "q = orders & agg { p = filtered .status (sum .amount) }\n",
        "q"
    )
    .contains("bool"));
    assert!(error(
        "q = orders & agg { p = filtered (count > 0) (sum .amount) }\n",
        "q"
    )
    .contains("cannot nest"));
    assert!(error(
        "q = orders & select { p = filtered (.status == \"paid\") (sum .amount) }\n",
        "q"
    )
    .contains("belong in `agg`"));
    let w = error(
        "q = users & where (rowNumber { order = [asc .id] } <= 3)\n",
        "q",
    );
    assert!(w.contains("window"), "{w}");
}

#[test]
fn overloads_dispatch_to_sql() {
    let s = sql(
        "q = users & select { s = .name ++ \" \" ++ upper .name, a = .age + 1 }\n",
        "q",
    );
    // `++` is right-associative.
    assert!(s.contains("name || (' ' || UPPER(name)) AS s"), "{s}");
    assert!(s.contains("age + 1 AS a"), "{s}");
    // One generic helper, dispatched at int and float in the same query
    // (the choices are also checked in cagara-hir's overload tests).
    let s = sql("twice = x => x + x\nquad = x => twice (twice x)\nq = orders & select { a = quad .user_id, b = twice .amount }\n", "q");
    assert!(s.contains("AS a"), "{s}");
    assert!(s.contains("amount + amount AS b"), "{s}");
    let s = sql(
        "describe : expr ?r int -> expr ?r string = sql \"CAST($1 AS TEXT)\"\n\
         describe : expr ?r bool -> expr ?r string = sql \"CASE WHEN $1 THEN 'yes' ELSE 'no' END\"\n\
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
        "q = orders & leftJoin (users & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id)\n  & select { id = .id, who = coalesce \"?\" .name, known = isNotNull .name }\n",
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
        "q = orders & leftJoin (users & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id) & where (.name == \"x\")\n",
        "q"
    )
    .contains("maybe"));
}

#[test]
fn join_inputs_are_inlined_when_safe() {
    // An update before a join needs no derived table.
    let s = sql("q = orders & select { order_id = .id, user_id = .user_id } & innerJoin (users & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id) & select { o = .order_id, n = .name }\n", "q");
    assert_sql(
        &s,
        "SELECT t1.id AS o, t2.name AS n FROM public.orders AS t1 INNER JOIN public.users AS t2 ON t1.user_id = t2.id",
    );
    // An outer join's input is a relation, so a filter on either side stays
    // in that side's derived table. Hoisting the preserved side's filter to
    // WHERE would drop the unmatched rows the join keeps; hoisting the
    // null-extended side's into ON would widen the input back to the
    // unfiltered relation.
    let s = sql("q = orders & where (.amount > 10.0) & leftJoin ((users & where .active) & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id)\n", "q");
    assert_sql(
        &s,
        "SELECT t1.id, t1.user_id, t1.amount, t1.status, t1.created_at, t2.u_id, t2.name, t2.age, \
         t2.active FROM (SELECT id, user_id, amount, status, created_at FROM public.orders \
         WHERE (amount > 10.0)) AS t1 LEFT JOIN (SELECT id AS u_id, name, age, active FROM \
         public.users WHERE active) AS t2 ON t1.user_id = t2.u_id",
    );
    // The same holds when only the null-extended side is filtered: its filter
    // stays in that side's derived table rather than moving into `ON`, because
    // an `ON` filter would widen the input back to the unfiltered relation.
    let s = sql(
        "q = orders & leftJoin ((users & where .active) & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id)\n",
        "q",
    );
    assert_sql(
        &s,
        "SELECT t1.id, t1.user_id, t1.amount, t1.status, t1.created_at, t2.u_id, t2.name, t2.age, \
         t2.active FROM public.orders AS t1 LEFT JOIN (SELECT id AS u_id, name, age, active \
         FROM public.users WHERE active) AS t2 ON t1.user_id = t2.u_id",
    );
    // Right / full join: every filtered side keeps a derived table.
    let s = sql(
        "q = orders & where (.amount > 10.0) & fullJoin (users & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id)\n",
        "q",
    );
    assert!(
        s.contains("FROM public.orders WHERE (amount > 10.0)) AS t1 FULL JOIN public.users AS t2"),
        "{s}"
    );
    let s = sql(
        "q = orders & where (.amount > 10.0) & rightJoin (users & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id)\n",
        "q",
    );
    assert!(s.contains("(SELECT"), "{s}");
    // A right join's *right* input is the preserved side, so a filter there
    // stays in its derived table rather than becoming an ON predicate (which
    // would resurrect the filtered-out rows as unmatched ones).
    let s = sql(
        "q = orders & rightJoin ((users & where .active) & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id)\n",
        "q",
    );
    assert_sql(
        &s,
        "SELECT t1.id, t1.user_id, t1.amount, t1.status, t1.created_at, t2.u_id, t2.name, t2.age, \
         t2.active FROM public.orders AS t1 RIGHT JOIN (SELECT id AS u_id, name, age, active \
         FROM public.users WHERE active) AS t2 ON t1.user_id = t2.u_id",
    );
    // Chains of joins stay flat, including a self-join with an updated column.
    let s = sql("q = orders & innerJoin (users & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id) & leftJoin (users & prefix \"u2_\") (.<user_id == .>u2_id)\n", "q");
    assert!(!s.contains("(SELECT"), "{s}");
    assert!(
        s.contains("LEFT JOIN public.users AS t3 ON t1.user_id = t3.id"),
        "{s}"
    );
    // A join on the right is a derived table.
    let s = sql(
        "q = orders & innerJoin ((users & select { u_id = .id }) & innerJoin \
         (orders & prefix \"o2_\") (.<u_id == .>o2_user_id)) (.<user_id == .>u_id)\n",
        "q",
    );
    assert!(s.contains("INNER JOIN (SELECT"), "{s}");
}

#[test]
fn dialects_rewrite_every_block() {
    let q = "q = users & select { id = .id, s = .name ++ \"!\" } & order [asc .s] & limit 10 & offset 5\n";
    assert!(dialect(q, "q", "postgres").unwrap().contains("name || '!'"));
    let my = dialect(q, "q", "mysql").unwrap();
    assert!(!my.contains("||"), "`||` is OR in MySQL: {my}");
    // MySQL has no NULLS LAST: a leading `IS NULL` key sorts them last.
    assert!(
        my.contains("CONCAT(name, '!') AS s")
            && my.contains(
                "ORDER BY CASE WHEN CONCAT(name, '!') IS NULL THEN 1 ELSE 0 END, CONCAT(name, '!')"
            ),
        "{my}"
    );
    // `limit 10 & offset 5` keeps rows 6..=10 — five of them — so the statement
    // carries `LIMIT 5 OFFSET 5`, not `LIMIT 10 OFFSET 5`: the two are one clause
    // only in the order `offset` then `limit`, and `rewrite` turns this pipeline
    // into that one. It stays a single statement either way.
    assert!(my.contains("LIMIT 5 OFFSET 5"), "{my}");
    assert_eq!(my.matches("LIMIT").count(), 1, "{my}");
    assert!(!my.contains("AS t1"), "pagination must not wrap: {my}");
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
        "q = users & order [asc .name] & limit 3 & select { s = .name ++ .name }\n",
        "q",
        "tsql",
    )
    .unwrap();
    assert!(
        ts.contains("TOP 3") && ts.contains("CONCAT(name, name)"),
        "{ts}"
    );
    // T-SQL spells pagination as `OFFSET … ROWS FETCH NEXT … ROWS ONLY`, and
    // the flat statement carries the ORDER BY it needs — no TOP, no wrap.
    // `limit 10 & offset 5` keeps five rows, so the FETCH count is 5.
    let ts = dialect(q, "q", "tsql").unwrap();
    assert!(
        ts.contains("OFFSET 5 ROWS FETCH NEXT 5 ROWS ONLY") && !ts.contains("TOP"),
        "{ts}"
    );
    // A lone `offset` with no `order` is *accepted* now, spelled the way T-SQL
    // requires: `ORDER BY (SELECT NULL)` is the documented idiom for paging with no
    // meaningful order. Refusing it made acceptance depend on the target and was not
    // even self-consistent — `offset 5 & limit 3` already emitted the dummy order, so
    // only the bare form was an error.
    let bare = dialect("q = users & offset 5\n", "q", "tsql")
        .expect("T-SQL pages without an `order` by ordering on a constant");
    assert!(bare.contains("ORDER BY (SELECT NULL)"), "{bare}");
    assert!(
        bare.contains("OFFSET 5 ROWS FETCH NEXT 9223372036854775807 ROWS ONLY"),
        "{bare}"
    );
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
    let j = "q = orders & leftJoin (users & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id) & select { n = coalesce \"?\" .name ++ \"!\", rn = rowNumber { order = [desc .amount] } }\n";
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
    // A filter after a window or LIMIT must stay outside it.
    for q in [
        "q = orders & select { id = .id, amount = .amount, rn = rowNumber { order = [desc .amount] } } & where (.amount > 10.0)\n",
        "q = orders & order [desc .amount] & limit 5 & where (.amount > 10.0)\n",
    ] {
        let s = sql_with(q, "q", opts);
        assert!(s.contains(") AS t1 WHERE"), "filter moved across a boundary: {s}");
    }
    // A filter over an aggregate is HAVING: still a boundary the optimizer must
    // not push past, and still outside the aggregate's own WHERE.
    let s = sql_with(
        "q = orders & agg { u = group .user_id, n = count } & where (.n > 3)\n",
        "q",
        opts,
    );
    // The boundary is the `HAVING` clause itself: the optimizer must not push
    // the filter into the aggregate's `WHERE`, which runs before grouping.
    assert!(s.contains("HAVING COUNT(*) > 3"), "{s}");
    assert!(s.contains("GROUP BY user_id"), "{s}");
    let s = sql_with(
        "q = orders & where (1 + 1 == 2 && .amount > 1.0)\n",
        "q",
        opts,
    );
    assert!(!s.contains("1 + 1"), "{s}");
}

#[test]
fn validator_errors_point_at_the_stage() {
    // The checker carries the projected row through the definition and
    // reports the missing column at the complete stage expression.
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
    assert_eq!((d.col, d.width), (5, "where (.nope > 1)".len()), "{d}");
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
        ("yearOfDate .d", &[("postgres", "EXTRACT(YEAR FROM d)::INT"), ("sqlite", "CAST(STRFTIME('%Y', d) AS INT)"), ("tsql", "DATEPART(YEAR, d)")]),
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
                // A negative `n` drops that many from the front, so the emulation has
                // an arm for it; the old spelling folded every `n <= 0` to `''`, which
                // is `RIGHT`'s answer only at zero.
                (
                    "sqlite",
                    "CASE WHEN 2 < 0 THEN SUBSTR(s, 1 - 2) ELSE CASE WHEN 2 = 0 THEN '' \
                     ELSE SUBSTR(s, -2, 2) END END",
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
        // SQLite's `LIKE` is case-insensitive for ASCII, so emitting it made `like`
        // mean `ilike` there and the same program matched `"ABC"` on SQLite only.
        // The spelling is `GLOB` with the pattern's metacharacters translated, and
        // `GLOB` as a *function* takes the pattern first — the opposite of the
        // operator — so the argument order is part of what this pins.
        (
            "like \"A%\" .s",
            &[("postgres", "s LIKE 'A%'"), ("sqlite", "GLOB(REPLACE(")],
        ),
        (
            // The subject is the *last* argument: `GLOB` the function takes the
            // pattern first, unlike the infix operator. Getting this backwards made
            // every `like` on SQLite answer no-match.
            "like \"A%\" .s",
            &[("sqlite", "'), s) AS x FROM public.ev")],
        ),
        (
            "ilike \"A%\" .s",
            &[("sqlite", "LOWER(s) LIKE LOWER('A%')")],
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
    let q = "nope : expr ?r int -> expr ?r int = sql \"CAGARA_NOPE($1)\"\n\
             q = users & select { v = nope .id }\n";
    let e = dialect(q, "q", "ansi").unwrap_err();
    assert!(e.contains("CAGARA_NOPE"), "{e}");
}

#[test]
fn data_last_functions_compose() {
    // Partial applications are reusable transformations. `>>>` reads left to
    // right and `<<<` right to left, so the two spellings of one pipeline are
    // mirror images and have to produce the same SQL.
    let q = format!(
        "{EVENTS}nextWeek = addDays 7 >>> truncWeek\n\
         clean = trim >>> lower >>> replaceAll \"-\" \"\"\n\
         mirrored = s => replaceAll \"-\" \"\" (lower (trim s))\n\
         q = ev & where (contains \"@\" .s) & select {{ w = nextWeek .d, c = clean .s, m = mirrored .s, t = .s & left 3 }}\n"
    );
    let s = dialect(&q, "q", "postgres").unwrap();
    assert!(s.contains("STRPOS(s, '@') > 0"), "{s}");
    assert!(
        s.contains("DATE_TRUNC('WEEK', (d + 7 * INTERVAL '1' DAY)::DATE)::DATE AS w"),
        "{s}"
    );
    assert!(s.contains("REPLACE(LOWER(TRIM(s)), '-', '') AS c"), "{s}");
    assert!(s.contains("REPLACE(LOWER(TRIM(s)), '-', '') AS m"), "{s}");
}

#[test]
fn the_infix_spellings_match_their_named_forms() {
    // Every infix operator with a named twin lowers to the same SQL, so a
    // reader may choose per site. Arithmetic and comparison have no named
    // form; these three do.
    let pairs = [
        (
            // `nothing` is the nullable operand: `sum` would be an aggregate,
            // and an aggregate belongs in `agg`, not `select`.
            "q = users & select { a = nothing ?? 0 }\n",
            "q = users & select { a = coalesce 0 nothing }\n",
        ),
        (
            "q = users & select { s = .name ++ \"!\" }\n",
            "q = users & select { s = concat .name \"!\" }\n",
        ),
        // `<>` is the same concatenation under Haskell's spelling.
        (
            "q = users & select { s = .name <> \"!\" }\n",
            "q = users & select { s = .name ++ \"!\" }\n",
        ),
    ];
    for (infix, named) in pairs {
        assert_eq!(sql(infix, "q"), sql(named, "q"), "`{infix}`");
    }
    // The operators work where their type is nullable, which is the case a
    // spelling-only implementation would get wrong: the left join makes the
    // right side `maybe`, so the `??` is what makes it non-null.
    let s = sql(
        "q = orders & leftJoin (users & prefix \"u_\") (.<user_id == .>u_id) \
         & select { n = .u_name ?? \"?\" }\n",
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
    let s = sql(
        "q = orders & fullJoin (users & prefix \"u_\") (.<user_id == .>u_id) \
         & select { a = .u_age ?? .user_id ?? 0, b = .u_age ?? 0 + 1 }\n",
        "q",
    );
    assert!(
        s.contains("COALESCE(t2.age, COALESCE(t1.user_id, 0)) AS a"),
        "{s}"
    );
    assert!(s.contains("COALESCE(t2.age, 0) + 1 AS b"), "{s}");
    // The left side must be nullable, the default must not be.
    assert!(error("q = users & select { a = .age ?? 0 }\n", "q").contains("maybe"));
    assert!(error(
        "q = orders & leftJoin (users & prefix \"u_\") (.<user_id == .>u_id) \
         & select { a = .u_age ?? .user_id ?? 0 }\n",
        "q"
    )
    .contains("maybe"));
}

#[test]
fn shorthand_errors_point_at_the_stage() {
    let src = format!("{USERS}q = users\n  & where (.salary > 1)\n");
    let ws = Workspace::from_source(&src);
    let d = root_queries(&ws)
        .into_iter()
        .find(|(k, _)| k == "q")
        .unwrap()
        .1
        .unwrap_err();
    assert!(d.message.contains("salary"), "{d}");
    assert_eq!(d.source.trim(), "& where (.salary > 1)", "{d}");
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
    // A computed column that stays NULL when its row is absent may inline: the
    // join's unmatched rows see NULL either way, because `NULL + 1` is NULL.
    let r = "r = (users & select { id = .id, k = .id + 1 }) & rightJoin (orders & select { user_id = .user_id, amount = .amount, status = .status, created_at = .created_at }) (.<id == .>user_id) & select { k = .k }\n";
    assert_sql(
        &sql(r, "r"),
        "SELECT t1.id + 1 AS k FROM public.users AS t1 RIGHT JOIN public.orders AS t2 ON t1.id = t2.user_id",
    );
    // A function call is not assumed to keep NULLs (`coalesce` and `ifnull`
    // exist), so it is computed before the join.
    let fnq = "fq = users & leftJoin (orders & select { user_id = .user_id, s = toString .amount }) (.<id == .>user_id) & select { s = .s }\n";
    assert!(
        sql(fnq, "fq")
            .contains("(SELECT user_id, CAST(amount AS TEXT) AS s FROM public.orders) AS t2"),
        "{}",
        sql(fnq, "fq")
    );
    let f = "f = users & fullJoin (orders & select { user_id = .user_id, st = coalesce \"none\" (just .status) }) (.<id == .>user_id) & select { s = .st }\n";
    assert!(
        sql(f, "f").contains("COALESCE(status, 'none') AS st FROM public.orders) AS t2"),
        "{}",
        sql(f, "f")
    );
    // Bare columns still inline, and so does anything on a preserved side.
    let b = "b = users & leftJoin (orders & select { user_id = .user_id }) (.<id == .>user_id) & select { u = .user_id }\n";
    assert_sql(
        &sql(b, "b"),
        "SELECT t2.user_id AS u FROM public.users AS t1 LEFT JOIN public.orders AS t2 ON t1.id = t2.user_id",
    );
}

/// A sort key written as an output name must still be one when it is printed.
///
/// `prefix`/`suffix` rename the row's columns, and a later `select` can rename
/// them again. A key that was an output alias of the stage the `order` was written
/// on is not necessarily an output alias of the stage that prints it, and a bare
/// `ORDER BY x_id` against a `FROM` with no such column is not SQL at all:
///
/// ```text
/// prefix "x_" & order [asc .x_id] & select { a = .x_id }
///   SELECT id AS a FROM t ORDER BY id     -- not ORDER BY x_id
/// ```
#[test]
fn a_renamed_column_does_not_leave_a_dangling_sort_key() {
    let q = "t : query { id = int, k = string } = table \"m\" \"t\"\n\
             q = t & prefix \"x_\" & order [asc .x_id] & select { a = .x_id }\n";
    assert_eq!(
        sql(q, "q"),
        "SELECT id AS a FROM m.t ORDER BY id NULLS LAST"
    );
    // The same through `suffix`, and through two renames in a row.
    let q = "t : query { id = int } = table \"m\" \"t\"\n\
             q = t & suffix \"_v\" & order [asc .id_v] & select { b = .id_v }\n";
    assert_eq!(
        sql(q, "q"),
        "SELECT id AS b FROM m.t ORDER BY id NULLS LAST"
    );
    let q = "t : query { id = int } = table \"m\" \"t\"\n\
             q = t & prefix \"x_\" & suffix \"_v\" & order [asc .x_id_v] \
             & select { c = .x_id_v }\n";
    assert_eq!(
        sql(q, "q"),
        "SELECT id AS c FROM m.t ORDER BY id NULLS LAST"
    );
    // A key whose name survives into the printed select list stays the name, and
    // the swap case still means the output column.
    let q = "t : query { id = int, a = int } = table \"m\" \"t\"\n\
             q = t & select { id = .a, a = .id } & order [asc .id] & select { id = .id, a = .a }\n";
    assert_eq!(
        sql(q, "q"),
        "SELECT a AS id, id AS a FROM m.t ORDER BY id NULLS LAST"
    );
}

/// A carried-over sort key must not be captured by the projection above it.
///
/// `wrap` carries a stage's order out as bare names. When the projection that
/// follows gives one of its outputs that same name for a *different* column, SQL
/// resolves the bare name to the output — so the outer query sorted by the new
/// column and reordered the rows the limit had just chosen. Measured on DuckDB
/// before the fix: `order [asc .age] & limit 2 & select { age = .id }` returned
/// `1, 5` where the answer is `5, 1`.
#[test]
fn a_projection_does_not_capture_a_carried_sort_key() {
    // The name is reused for another column: the key is qualified.
    assert_eq!(
        sql(
            "q = users & order [asc .name] & limit 2 & select { name = .id, id = .name, age = .age, active = .active }\n",
            "q"
        ),
        "SELECT id AS name, name AS id, age, active FROM (SELECT id, name, age, active \
         FROM public.users ORDER BY name NULLS LAST LIMIT 2) AS t1 ORDER BY t1.name NULLS LAST"
    );
    // The column is dropped and its name reused: still qualified, so the outer
    // sort reads the derived table's column rather than the new one.
    let dropped = sql(
        "q = users & order [asc .name] & limit 2 & select { name = .id }\n",
        "q",
    );
    assert!(dropped.contains("ORDER BY t1.name"), "{dropped}");
    // A projection that keeps the name for the same column needs no qualification.
    let plain = sql(
        "q = users & order [asc .id] & limit 2 & select { x = .id }\n",
        "q",
    );
    assert_eq!(
        plain,
        "SELECT id AS x FROM public.users ORDER BY id NULLS LAST LIMIT 2"
    );
}

/// `rollup` and `cube` emit one `GROUP BY` clause over the marked keys.
///
/// The shape is the *stage's*, not the expression's: `ROLLUP(a, b)` is one clause
/// over every marked key, so a plain `group` beside them must come first in the
/// clause to mean "within the subtotals".
#[test]
fn rollup_and_cube_emit_the_grouping_clause() {
    let src = "g : query { a = int, b = int, n = int } = table \"main\" \"g\"\n";
    assert_eq!(
        sql(
            &format!("{src}q = g & agg {{ a = rollup .a, b = rollup .b, s = sum .n }}\n"),
            "q"
        ),
        "SELECT a, b, SUM(n) AS s FROM main.g GROUP BY ROLLUP(a, b)"
    );
    assert_eq!(
        sql(
            &format!("{src}q = g & agg {{ a = cube .a, b = cube .b, s = sum .n }}\n"),
            "q"
        ),
        "SELECT a, b, SUM(n) AS s FROM main.g GROUP BY CUBE(a, b)"
    );
    // A plain key beside a rolled-up one groups *within* it, which is only the
    // meaning if the plain key comes first.
    assert_eq!(
        sql(
            &format!("{src}q = g & agg {{ a = group .a, b = rollup .b, s = sum .n }}\n"),
            "q"
        ),
        "SELECT a, b, SUM(n) AS s FROM main.g GROUP BY a, ROLLUP(b)"
    );
}

/// A plain key beside a rolled-up one goes *first* in the clause, whatever order
/// the source wrote them in.
///
/// `GROUP BY b, ROLLUP(a)` groups within each `b` and adds a subtotal per `b`;
/// `GROUP BY ROLLUP(a), b` is not valid SQL at all. So the clause order is not the
/// source order, and that is deliberate — the alternative is a rule that only
/// sometimes compiles. Pinned because the reordering is invisible in the source:
/// `agg { a = rollup .a, b = group .b }` emits `GROUP BY b, ROLLUP(a)`.
#[test]
fn a_plain_key_precedes_a_rolled_up_one() {
    let src = "g : query { a = int, b = int, n = int } = table \"main\" \"g\"\n";
    // Written with the rollup first, emitted with the plain key first.
    assert_eq!(
        sql(
            &format!("{src}q = g & agg {{ a = rollup .a, b = group .b, s = sum .n }}\n"),
            "q"
        ),
        "SELECT a, b, SUM(n) AS s FROM main.g GROUP BY b, ROLLUP(a)"
    );
    // And the other way round, which reads in the order it emits.
    assert_eq!(
        sql(
            &format!("{src}q = g & agg {{ a = group .a, b = rollup .b, s = sum .n }}\n"),
            "q"
        ),
        "SELECT a, b, SUM(n) AS s FROM main.g GROUP BY a, ROLLUP(b)"
    );
    // Two shapes in one stage keep their own groups: `ROLLUP(a, b)` and `CUBE(c)`
    // are two terms, not one merged list.
    let three = "g3 : query { a = int, b = int, c = int, n = int } = table \"main\" \"g\"\n";
    let out = sql(
        &format!(
            "{three}q = g3 & agg {{ a = rollup .a, b = rollup .b, c = cube .c, s = sum .n }}\n"
        ),
        "q",
    );
    assert!(out.contains("ROLLUP(a, b)"), "{out}");
    assert!(out.contains("CUBE(c)"), "{out}");
}

/// The targets without grouping sets get the subtotals as a union of aggregates.
///
/// SQLite has no grouping sets and MySQL's `WITH ROLLUP` is a different operation,
/// so both used to be refused — with a message telling the reader to compute the
/// subtotals with a separate `agg` and union them in. The compiler writes exactly
/// that: one `agg` per grouping set, `UNION ALL`ed, with the rolled-up keys NULL in
/// the rows that do not group by them.
///
/// `ROLLUP(a)` is `{(a), ()}`, so the emulation is two stages: the per-`a` rows and
/// the grand total. Dropping either would be a wrong answer, not a simplification.
#[test]
fn grouping_sets_are_emulated_where_they_do_not_exist() {
    let src = "g : query { a = int, n = int } = table \"main\" \"g\"\n\
               q = g & agg { a = rollup .a, s = sum .n }\n";
    for d in ["sqlite", "mysql"] {
        let s = dialect(src, "q", d).unwrap_or_else(|e| panic!("{d}: {e}"));
        assert!(s.contains("UNION ALL"), "{d}: {s}");
        assert!(!s.contains("ROLLUP"), "{d}: {s}");
        assert!(!s.contains("WITH ROLLUP"), "{d}: {s}");
        // One branch per grouping set: the keyed rows, and the grand total.
        assert_eq!(s.matches("UNION ALL").count(), 1, "{d}: {s}");
        assert_eq!(s.matches("FROM main.g").count(), 2, "{d}: {s}");
        assert!(s.contains("GROUP BY a"), "{d}: {s}");
        // The grand total's key is NULL rather than absent, so the branches line up.
        assert!(s.contains("NULL AS a"), "{d}: {s}");
    }
    // The targets that have the clause keep it: emulation is not applied blindly.
    for d in [
        "duckdb",
        "postgres",
        "trino",
        "tsql",
        "bigquery",
        "snowflake",
    ] {
        let s = dialect(src, "q", d).unwrap_or_else(|e| panic!("{d}: {e}"));
        assert!(s.contains("ROLLUP(a)"), "{d}: {s}");
        assert!(!s.contains("UNION ALL"), "{d}: {s}");
    }
}

/// BigQuery writes a set operation with an explicit `ALL` or `DISTINCT`.
///
/// Its grammar is `UNION { ALL | DISTINCT }` — a bare `UNION` is a syntax error, not
/// a default — and the same for `INTERSECT` and `EXCEPT`. sqlglot's AST has one
/// `all: bool`, so the generator cannot say `DISTINCT`; the keyword is added to the
/// text afterwards (`dialect::finalize`). Python sqlglot does the same, which is
/// where the spelling comes from.
#[test]
fn bigquery_writes_set_operations_with_a_modifier() {
    let q = "a = users & select {.id}\nb = users & select {.id}\nq = a & union b & select {.id}\n";
    for (op, want) in [
        ("union", "UNION DISTINCT"),
        ("intersect", "INTERSECT DISTINCT"),
        ("except", "EXCEPT DISTINCT"),
        ("unionAll", "UNION ALL"),
    ] {
        let src = q.replace("union", op);
        let s = dialect(&src, "q", "bigquery").unwrap_or_else(|e| panic!("{op}: {e}"));
        assert!(s.contains(want), "{op}: {s}");
    }
    // Every other target keeps the plain keyword, which is what it reads.
    for d in ["postgres", "duckdb", "mysql", "trino", "snowflake"] {
        let s = dialect(q, "q", d).unwrap_or_else(|e| panic!("{d}: {e}"));
        assert!(s.contains("UNION SELECT"), "{d}: {s}");
    }
    // The word inside a string literal is not a keyword.
    let s = dialect(
        "q = users & select { id = .id, k = \"UNION\" }\n",
        "q",
        "bigquery",
    )
    .unwrap();
    assert!(s.contains("'UNION'"), "{s}");
    assert!(!s.contains("UNION DISTINCT"), "{s}");
}

/// BigQuery escapes a quote inside a string literal with a backslash.
///
/// `''` is standard SQL and what six of the targets read; BigQuery parses it as two
/// adjacent literals and *errors*. sqlglot's generator doubles the quote for every
/// dialect, so the spelling is fixed in the text after generation.
#[test]
fn bigquery_escapes_a_quote_with_a_backslash() {
    let q = "q = users & where (.name == \"it's\") & select {.id}\n";
    let bq = dialect(q, "q", "bigquery").unwrap();
    assert!(bq.contains(r"'it\'s'"), "{bq}");
    assert!(!bq.contains("''s"), "{bq}");
    for d in ["postgres", "duckdb", "sqlite", "mysql", "trino", "tsql"] {
        let s = dialect(q, "q", d).unwrap_or_else(|e| panic!("{d}: {e}"));
        assert!(s.contains("'it''s'"), "{d}: {s}");
    }
}

/// A filter over a window is `QUALIFY` where the target has the clause.
///
/// The filter belongs after the window functions, which is exactly what `QUALIFY`
/// is, so the derived table the general rule builds is not needed. sqlglot does not
/// rewrite the clause per dialect — it emits `QUALIFY` for every target — so the
/// decision has to be made before the statement exists, which is why the lowerer is
/// told the capability rather than the dialect.
#[test]
fn a_filter_over_a_window_is_qualify_where_the_target_has_it() {
    let q =
        "q = users & select { id = .id, r = rowNumber { order = [asc .id] } } & where (.r > 2)\n";
    for d in ["duckdb", "snowflake", "bigquery"] {
        let s = dialect(q, "q", d).unwrap_or_else(|e| panic!("{d}: {e}"));
        // The window function itself, not the output name: a later projection can
        // drop the name, and then the engine has nothing to bind it to.
        assert!(s.contains("QUALIFY ROW_NUMBER() OVER"), "{d}: {s}");
        assert!(!s.contains("AS t1"), "{d}: no derived table: {s}");
    }
    // Everywhere else the derived table is what the dialect can read.
    for d in ["postgres", "mysql", "sqlite", "trino", "tsql"] {
        let s = dialect(q, "q", d).unwrap_or_else(|e| panic!("{d}: {e}"));
        assert!(!s.contains("QUALIFY"), "{d}: {s}");
        assert!(s.contains("AS t1"), "{d}: {s}");
    }
    // Paging runs *after* the filter in the pipeline but *before* `QUALIFY` in
    // SQL, so the pair cannot fold: `select & limit 2 & where` takes two rows and
    // filters them, where `QUALIFY … LIMIT 2` filters the whole relation.
    let s = dialect(
        "q = users & select { id = .id, r = rowNumber { order = [asc .id] } } & limit 2 & where (.r > 1)\n",
        "q",
        "duckdb",
    )
    .unwrap();
    assert!(s.contains("AS t1"), "paging must not fold: {s}");
    // The other order is the one `QUALIFY` spells.
    let s = dialect(
        "q = users & select { id = .id, r = rowNumber { order = [asc .id] } } & where (.r > 1) & limit 2\n",
        "q",
        "duckdb",
    )
    .unwrap();
    assert_eq!(
        s,
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS r FROM public.users \
         QUALIFY ROW_NUMBER() OVER (ORDER BY id NULLS LAST) > 1 LIMIT 2"
    );
    // A `distinct` also runs after the window, so it keeps the derived table.
    let s = dialect(
        "q = users & select { id = .id, r = rowNumber { order = [asc .id] } } & distinct & where (.r > 1)\n",
        "q",
        "duckdb",
    )
    .unwrap();
    assert!(s.contains("AS t1"), "distinct must not fold: {s}");
}

/// A projection may fold over a `limit` even when it drops columns, because the
/// `ORDER BY` it folds under names *input* columns. That is only true while the
/// projection does not reuse one of those names for another column: after the fold
/// they share a `SELECT`, and a bare `ORDER BY name` then binds to the output.
#[test]
fn a_sort_key_shadowed_by_the_projection_keeps_its_barrier() {
    // `order [asc .name]` with a projection that renames `id` to `name`: the
    // folded `ORDER BY name` would sort by `id`.
    let s = sql(
        "q = users & order [asc .name] & limit 3 & select { name = .id, x = .name }\n",
        "q",
    );
    assert!(s.contains("AS t1"), "must keep the derived table: {s}");
    // The same projection without the clash folds.
    let s = sql(
        "q = users & order [asc .name] & limit 3 & select { x = .id }\n",
        "q",
    );
    assert_eq!(
        s,
        "SELECT id AS x FROM public.users ORDER BY name NULLS LAST LIMIT 3"
    );
}

/// A projection that renames and reorders without dropping anything folds into
/// a stage a general projection may not: it cannot change which rows a `LIMIT` or
/// a `DISTINCT` below it selects.
#[test]
fn a_permutation_projection_crosses_a_barrier() {
    // Across a limit: one statement, and the sort key is still a base column.
    assert_eq!(
        sql(
            "q = users & order [asc .id] & limit 3 & select { x = .id, y = .name, z = .age, w = .active }\n",
            "q"
        ),
        "SELECT id AS x, name AS y, age AS z, active AS w FROM public.users ORDER BY id NULLS LAST LIMIT 3"
    );
    // Across a `distinct`.
    assert_eq!(
        sql(
            "q = users & distinct & select { y = .name, x = .id, z = .age, w = .active }\n",
            "q"
        ),
        "SELECT DISTINCT name AS y, id AS x, age AS z, active AS w FROM public.users"
    );
    // Dropping a column is not a permutation, but across *paging* it folds
    // anyway: `LIMIT` picks rows by an `ORDER BY` over the input columns, which
    // SQL allows even when the projection no longer outputs them.
    assert_eq!(
        sql(
            "q = users & order [asc .id] & limit 3 & select { x = .id }\n",
            "q"
        ),
        "SELECT id AS x FROM public.users ORDER BY id NULLS LAST LIMIT 3"
    );
    // A `DISTINCT` is the one that still needs the barrier: dropping a column
    // there changes how many rows are deduped. `(1,'a'), (1,'b')` is two rows
    // after `distinct`, and one after the fold.
    let dropped = sql("q = users & distinct & select { x = .id }\n", "q");
    assert!(dropped.contains("AS t1"), "{dropped}");
    // And a projection that puts a different column under a name a sort key uses
    // changes what `ORDER BY` means once they share a `SELECT`.
    let shadowed = sql(
        "q = users & order [asc .name] & limit 3 & select { name = .id, x = .name }\n",
        "q",
    );
    assert!(shadowed.contains("AS t1"), "{shadowed}");
    // Nor is computing one.
    let computed = sql(
        "q = users & distinct & select { x = .id, y = .name, z = .age, w = .id + 1 }\n",
        "q",
    );
    assert!(computed.contains("AS t1"), "{computed}");
}

/// A sort key is the *output* column it names, even when a projection put a
/// different column under that name.
///
/// `select { id = .a, a = .id }` is legal and swaps two columns. Resolving
/// `order [asc .id]` to the item's expression — the base column `a` — emitted
/// `ORDER BY a`, and `a` is an output alias for the base `id`, so both engines
/// ordered by the wrong column. The key is the output name.
#[test]
fn a_sort_key_is_the_output_column_not_its_expression() {
    let q = "nums : query { id = int, a = int } = table \"main\" \"nums\"\n\
             q = nums & select { id = .a, a = .id } & order [asc .id] & select { id = .id, a = .a }\n";
    assert_sql(
        &sql(q, "q"),
        "SELECT a AS id, id AS a FROM main.nums ORDER BY id NULLS LAST",
    );
    // The same name for the same column is unchanged: there is nothing to
    // disambiguate, so the key stays the bare column.
    let q = "q = users & select { n = .name } & order [asc .n]\n";
    assert_sql(
        &sql(q, "q"),
        "SELECT name AS n FROM public.users ORDER BY n NULLS LAST",
    );
}

#[test]
fn constant_keys_are_not_positions() {
    // `ORDER BY 1` / `GROUP BY 2` would refer to output columns.
    // The constant key is dropped; the column key is written as the output
    // name it sorts by (`a`, which is `name`), never as a position.
    let o = "o = users & select { a = .name, b = 1 } & order [asc .b, asc .a]\n";
    assert_eq!(
        sql(o, "o"),
        "SELECT name AS a, 1 AS b FROM public.users ORDER BY a NULLS LAST"
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
    for d in ["mysql", "snowflake", "spark"] {
        let s = dialect(q, "q", d).unwrap();
        assert!(s.contains("'a\\\\'' OR 1=1 -- '"), "{d}: {s}");
    }
    // BigQuery is the exception, and it was asserting the bug until an independent
    // parser read it: it has no `''` escape, so `'a\\'' OR 1=1 -- '` is a string, a
    // *second* string next to it, and `ParseError: Adjacent string literals`. The
    // quote is escaped with a backslash there instead.
    let bq = dialect(q, "q", "bigquery").unwrap();
    assert!(bq.contains("'a\\\\\\' OR 1=1 -- '"), "{bq}");
    // Also inside a window spec, which `Expr::transform` skips.
    let w = "w = users & select { id = .id, rn = rowNumber { partition = [.name ++ \"\\\\\"] } }\n";
    let s = dialect(w, "w", "mysql").unwrap();
    assert!(s.contains("CONCAT(name, '\\\\')"), "{s}");
}

#[test]
fn intrinsics_are_lowered_inside_window_specs() {
    let w = "w = orders & select { id = .id, rn = rowNumber { partition = [yearOfDate .created_at], order = [asc .id] } }\n";
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
    assert_sql(
        &sql(q, "q"),
        "SELECT id, rn FROM (SELECT id, ROW_NUMBER() OVER (ORDER BY id NULLS LAST) AS rn, age AS __k1 \
         FROM public.users) AS t2 WHERE (rn <= 3) ORDER BY __k1 DESC NULLS LAST",
    );
    // With a LIMIT the inner query keeps it too.
    let p = "p = users & order [asc .name] & limit 3 & where (.age > 1) & select { n = .name }\n";
    assert_sql(
        &sql(p, "p"),
        "SELECT name AS n FROM (SELECT id, name, age, active FROM public.users ORDER BY name NULLS LAST LIMIT 3) AS t1 \
         WHERE (age > 1) ORDER BY name NULLS LAST",
    );
    // A join does not keep its inputs' order: no ORDER BY in a join input.
    let j = "j = (users & order [asc .name] & select { id = .id, n = .name }) & innerJoin (orders & select { user_id = .user_id, amount = .amount, status = .status, created_at = .created_at }) (.<id == .>user_id) & select { n = .n }\n";
    assert!(!sql(j, "j").contains("ORDER BY"), "{}", sql(j, "j"));
}

#[test]
fn hidden_order_alias_does_not_collide_with_a_user_column() {
    let q = "weird : query { __k1 = int, id = int, sort = int } = table \"public\" \"weird\"\n\
             q = weird & order [desc .sort] & select { __k1 = .__k1, rn = rowNumber { order = [asc .id] } } & where (.rn > 0)\n";
    assert_sql(
        &sql(q, "q"),
        "SELECT __k1, rn FROM (SELECT __k1, ROW_NUMBER() OVER (ORDER BY id NULLS LAST) AS rn, sort AS __k2 \
         FROM public.weird) AS t2 WHERE (rn > 0) ORDER BY __k2 DESC NULLS LAST",
    );
}

#[test]
fn template_placeholders_must_stand_alone() {
    let bad = [
        (
            "f : expr ?r int -> expr ?r int = sql \"x$1\"",
            "must stand alone",
        ),
        (
            "f : expr ?r int -> expr ?r string = sql \"'$1'\"",
            "must stand alone",
        ),
        // Caught at the definition already.
        (
            "f : expr ?r int -> expr ?r int = sql \"$2 + 1\"",
            "placeholders up to $2",
        ),
    ];
    for (f, msg) in bad {
        let e = error(&format!("{f}\nq = users & select {{ y = f .age }}\n"), "f");
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
            // Every target answers NULL for a zero divisor: the families whose
            // own `/` raises get `NULLIF(j, 0)`, and the rest are left alone
            // because theirs already returns NULL.
            &[
                ("trino", "(i / NULLIF(j, 0))"),
                ("spark", "DIV(i, j)"),
                ("duckdb", "DIVIDE(i, j)"),
                ("bigquery", "DIV(i, j)"),
                (
                    "mysql",
                    "CAST(((i - (i % NULLIF(j, 0))) / NULLIF(j, 0)) AS SIGNED)",
                ),
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
        // `timestamp` is a supported `toString` overload, not just `date`.
        (
            "toString .t",
            &[
                ("ansi", "CAST(t AS TEXT)"),
                ("mysql", "CAST(t AS CHAR)"),
                ("trino", "CAST(t AS VARCHAR)"),
                ("spark", "CAST(t AS STRING)"),
            ],
        ),
        ("toString .d", &[("ansi", "CAST(d AS TEXT)")]),
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

/// A definition that is a function from a query to a query is a *stage*, however
/// its body spells the function. All four spellings here must lower to the same
/// SQL, which is what pins that they are one thing rather than four features.
#[test]
fn a_query_function_is_a_stage_in_every_spelling() {
    const EXPECTED: &str = "SELECT id, name, age, active FROM public.users WHERE (age > 18)";

    // The pipe's own stage, for reference.
    assert_eq!(sql("q = users & where (.age > 18)\n", "q"), EXPECTED);

    // A named helper, point-free.
    assert_eq!(
        sql("big = where (.age > 18)\nq = users & big\n", "q"),
        EXPECTED
    );

    // A named helper written as a lambda — the form the guide documents, and
    // the one the elaborator used to reject with "found a function".
    assert_eq!(
        sql("big = q => where (.age > 18) q\nq = users & big\n", "q"),
        EXPECTED
    );

    // The same lambda written inline as the stage.
    assert_eq!(
        sql("q = users & (r => where (.age > 18) r)\n", "q"),
        EXPECTED
    );
}

#[test]
fn a_stage_helper_may_take_parameters_before_the_query() {
    // The subject is last, so `byAge 21` binds `n` and the piped query binds `q`.
    assert_eq!(
        sql(
            "byAge = n => q => where (.age > n) q\nq = users & byAge 21\n",
            "q"
        ),
        "SELECT id, name, age, active FROM public.users WHERE (age > 21)"
    );
}

#[test]
fn a_stage_helper_body_may_apply_stages_in_sequence() {
    // A nested application, not just one stage: the inner query is threaded
    // into the outer one, and both see the row the previous stage produced.
    assert_eq!(
        sql(
            "adults = q => where (.name != \"\") (where (.age > 18) q)\nq = users & adults\n",
            "q"
        ),
        "SELECT id, name, age, active FROM public.users WHERE (age > 18) AND (name <> '')"
    );
}

/// An alias — a definition whose whole body is another name — must lower
/// exactly like the name it forwards to, at every call shape and through a
/// chain. `f = upper` is not a `sql` template and not a lambda, so following
/// the name to the definition that *has* the body is the only thing that makes
/// it callable; missing that step reported a compiler limitation instead.
#[test]
fn an_alias_calls_like_its_target() {
    // A scalar template, reached directly and through one and two aliases.
    let scalar = "q = users & select { x = upper .name }\n";
    assert_eq!(
        sql(scalar, "q"),
        sql("f = upper\nq = users & select { x = f .name }\n", "q")
    );
    assert_eq!(
        sql(scalar, "q"),
        sql(
            "f = upper\ng = f\nq = users & select { x = g .name }\n",
            "q"
        )
    );
    // The prelude's own aliases: `caseWhen` forwards to `ifThenElse`, and `in`
    // to `inList`.
    assert_eq!(
        sql(
            "q = users & select { x = ifThenElse .active \"y\" \"n\" }\n",
            "q"
        ),
        sql(
            "q = users & select { x = caseWhen .active \"y\" \"n\" }\n",
            "q"
        )
    );
    assert_eq!(
        sql("q = users & where (inList [1, 2] .age)\n", "q"),
        sql("q = users & where (in [1, 2] .age)\n", "q")
    );
    // A user's own template, and an alias of an alias of a lambda.
    let user = "myUp : expr ?r string -> expr ?r string = sql \"UPPER($1)\"\n";
    assert_eq!(
        sql(
            &format!("{user}q = users & select {{ x = myUp .name }}\n"),
            "q"
        ),
        sql(
            &format!("{user}f = myUp\nq = users & select {{ x = f .name }}\n"),
            "q"
        )
    );
    assert_eq!(
        sql("q = users & where (.age > 18)\n", "q"),
        sql(
            "p = x => .age > x\nf = p\ng = f\nq = users & where (g 18)\n",
            "q"
        )
    );
    // An aggregate template and a window template behind an alias.
    assert_eq!(
        sql("q = orders & agg { t = sum .amount }\n", "q"),
        sql("s = sum\nq = orders & agg { t = s .amount }\n", "q")
    );
    assert_eq!(
        sql("q = orders & select { r = rowNumber {} }\n", "q"),
        sql("r = rowNumber\nq = orders & select { r = r {} }\n", "q")
    );
}

/// One alias of an overload set must still choose per use, exactly as the
/// overloaded name does: `sum` over `amount` and over `user_id` are different
/// candidates, and the alias must not freeze the first choice.
#[test]
fn an_alias_of_an_overload_chooses_per_use() {
    assert_eq!(
        sql("q = orders & agg { t = sum .amount }\n", "q"),
        sql("s = sum\nq = orders & agg { t = s .amount }\n", "q")
    );
    assert_eq!(
        sql("q = orders & agg { t = sum .user_id }\n", "q"),
        sql("s = sum\nq = orders & agg { t = s .user_id }\n", "q")
    );
}

/// An alias used as a *value* rather than applied to arguments directly — passed
/// to another function, or composed with `>>>` — must also resolve to the
/// definition it names. This is a different code path from a direct call, and it
/// failed differently: the alias matched no body kind and fell through to the
/// expression reading, producing a callable that then refused its argument.
#[test]
fn an_alias_is_a_value_where_a_function_is_expected() {
    // Composed with `>>>` inside a named definition, which is where an alias is
    // passed around as a *value* rather than applied to arguments directly.
    assert_eq!(
        sql(
            "p = trim >>> upper\nq = users & select { x = p .name }\n",
            "q"
        ),
        sql(
            "up = upper\np = trim >>> up\nq = users & select { x = p .name }\n",
            "q"
        )
    );
    assert_eq!(
        sql(
            "p = upper >>> trim\nq = users & select { x = p .name }\n",
            "q"
        ),
        sql(
            "up = upper\np = up >>> trim\nq = users & select { x = p .name }\n",
            "q"
        )
    );
    // And an alias of that composed value, so the chain is followed through a
    // non-alias body too.
    assert_eq!(
        sql(
            "p = trim >>> upper\nq = users & select { x = p .name }\n",
            "q"
        ),
        sql(
            "up = upper\np0 = trim >>> up\np = p0\nq = users & select { x = p .name }\n",
            "q"
        )
    );
}

/// A **computed function value** in application-head position — not a name, but
/// an expression that normalizes to a function — must apply its arguments like
/// any other call. The checker types all of these; the elaborator used to
/// refuse them with "only a named definition can be applied in an expression".
#[test]
fn a_computed_function_value_applies_its_arguments() {
    // Composition, matching the direct spelling.
    assert_eq!(
        sql(
            "p = trim >>> upper\nq = users & select { x = p .name }\n",
            "q"
        ),
        sql("q = users & select { x = (trim >>> upper) .name }\n", "q")
    );
    // An inline lambda, which has no definition to look up at all.
    assert_eq!(
        sql("q = users & select { x = upper .name }\n", "q"),
        sql("q = users & select { x = (f => upper f) .name }\n", "q")
    );
    assert_eq!(
        sql("q = users & select { x = .age + 1 }\n", "q"),
        sql("q = users & select { x = (f => f + 1) .age }\n", "q")
    );
    // A partially applied function as the head.
    assert_eq!(
        sql("q = users & select { x = .name ++ .name }\n", "q"),
        sql("q = users & select { x = (concat .name) .name }\n", "q")
    );
    // A parameter holding a function, applied through a composition.
    assert_eq!(
        sql("q = users & select { x = upper .name }\n", "q"),
        sql(
            "app = g => (g .name)\nq = users & select { x = app upper }\n",
            "q"
        )
    );
}

/// A lambda's body is checked where the lambda is *written*, so one lambda
/// shape used at two column types must select its overloads per use, exactly as
/// a named helper does.
#[test]
fn a_lambda_head_selects_overloads_per_use() {
    let inline = "q = users & select { x = (f => f * 2) .age }\n";
    let named = "twice = f => f * 2\nq = users & select { x = twice .age }\n";
    assert_eq!(sql(named, "q"), sql(inline, "q"));
    // The same shape at another type must not reuse the first choice.
    let float = "q = orders & select { x = (f => f * 2) .amount }\n";
    let float_named = "twice = f => f * 2\nq = orders & select { x = twice .amount }\n";
    assert_eq!(sql(float_named, "q"), sql(float, "q"));
    assert!(
        sql(float, "q").contains("amount * 2"),
        "{}",
        sql(float, "q")
    );
}

/// A non-function head is the checker's error, not the elaborator's: `(1 + 2)`
/// is an `expr`, so applying it to an argument is a type error and must be
/// reported as one rather than as a compiler limitation.
#[test]
fn a_non_function_head_is_a_type_error() {
    let e = error("q = users & select { x = (1 + 2) .name }\n", "q");
    assert!(!e.contains("internal error"), "{e}");
    assert!(e.contains("cannot apply"), "{e}");
}

/// An alias cycle is a program error the compiler understands, so it reports
/// recursion rather than its own limitation.
#[test]
fn an_alias_cycle_is_reported_as_recursion() {
    for src in [
        "f = f\nq = users & select { x = f .name }\n",
        "f = g\ng = f\nq = users & select { x = f .name }\n",
    ] {
        let e = error(src, "q");
        assert!(e.contains("refers to itself"), "{src}: {e}");
    }
}

/// A helper used as a stage and a helper called directly must not diverge: both
/// go through the same elaboration of the definition's body.
#[test]
fn a_stage_helper_and_a_direct_call_agree() {
    let body = "big = q => where (.age > 18) q\n";
    assert_eq!(
        sql(&format!("{body}q = users & big\n"), "q"),
        sql(&format!("{body}q = big users\n"), "q")
    );
}

/// Point-free composition of stages is a stage: `>>>` and `<<<` are ordinary
/// functions from a query to a query, so every spelling must lower alike.
///
/// Composition is also the case that proved a deferred stage must carry the
/// module its argument was written in — it is applied from *prelude* source,
/// where the user's `.id` has no recorded type.
#[test]
fn composed_stages_are_stages() {
    const EXPECTED: &str = "SELECT id FROM public.users WHERE (id > 1)";

    // Composed inline in the pipeline.
    assert_eq!(
        sql("q = users & (select {.id} >>> where (.id > 1))\n", "q"),
        EXPECTED
    );
    // Composed from named stages.
    assert_eq!(
        sql(
            "s1 = select {.id}\ns2 = where (.id > 1)\nq = users & (s1 >>> s2)\n",
            "q"
        ),
        EXPECTED
    );
    // A named composition used as a stage.
    assert_eq!(
        sql(
            "clean = select {.id} >>> where (.id > 1)\nq = users & clean\n",
            "q"
        ),
        EXPECTED
    );
    // `<<<` is the mirror: `g <<< f` is `f` then `g`, so this is the same query.
    assert_eq!(
        sql("q = users & (select {.id} >>> where (.id > 1))\n", "q"),
        EXPECTED
    );
    // A user-written combinator is a stage by the same rule, so composing
    // stages needs no case of its own in the elaborator.
    assert_eq!(
        sql(
            "myComp = f => g => x => g (f x)\ns1 = select {.id}\ns2 = where (.id > 1)\n\
             q = users & (myComp s1 s2)\n",
            "q"
        ),
        EXPECTED
    );
    // Right-associative, so this chains: `a >>> (b >>> c)`.
    assert_eq!(
        sql(
            "q = users & (select {.id} >>> where (.id > 1) >>> select {.id})\n",
            "q"
        ),
        EXPECTED
    );
}

/// A composition written point-free and the same stages written as a lambda
/// must agree: they denote the same function.
#[test]
fn a_composed_stage_agrees_with_its_lambda_spelling() {
    assert_eq!(
        sql("q = users & (select {.id} >>> where (.id > 1))\n", "q"),
        sql("q = users & (r => select {.id} (where (.id > 1) r))\n", "q")
    );
}

#[test]
fn key_stages_rename_columns() {
    // Prefix and suffix are separate stages with type-level keyMap witnesses.
    assert_eq!(
        sql("q = users & prefix \"u_\"\n", "q"),
        "SELECT id AS u_id, name AS u_name, age AS u_age, active AS u_active FROM public.users"
    );
    assert_eq!(
        sql("q = users & suffix \"_v2\"\n", "q"),
        "SELECT id AS id_v2, name AS name_v2, age AS age_v2, active AS active_v2 FROM public.users"
    );
    assert_eq!(
        sql("q = users & prefix \"u_\" & suffix \"_v2\"\n", "q"),
        "SELECT id AS u_id_v2, name AS u_name_v2, age AS u_age_v2, active AS u_active_v2 FROM public.users"
    );
}

/// A relation used twice becomes a CTE, so its work is shared. That is only
/// valid when both uses want the same SQL: a use that *fuses* — a projection,
/// a filter, an order — rewrites the stage it is handed, and the CTE body was
/// built once without that context.
///
/// This is the case that used to emit the left branch's window as a bare
/// column: the input `s` was hoisted into a CTE whose body was built without
/// the window, and the left branch was handed that body.
#[test]
fn a_reused_input_is_not_turned_into_a_cte_when_its_context_differs() {
    let src = "s = users & where (.age > 20)\n\
               q = s & select { a = .id, r = rowNumber { order = [asc .id] } }\n\
                   & union (s & select { a = .id, r = .age })\n";
    let s = sql(src, "q");
    // Both branches must be what the user wrote: the window is not optional,
    // and the other branch must still project the plain column.
    assert_eq!(s.matches("ROW_NUMBER()").count(), 1, "{s}");
    assert!(s.contains("age AS r"), "{s}");
    assert!(
        s.contains("ROW_NUMBER() OVER (ORDER BY id NULLS LAST) AS r"),
        "{s}"
    );
    // The shared input's filter must not be dropped from either branch.
    assert_eq!(s.matches("age > 20").count(), 2, "{s}");
}

/// The same input used twice in the *same* way is still shared: that is the
/// reason the CTE exists, and the fix must not remove it. The filter runs once.
#[test]
fn a_reused_input_is_still_shared_when_the_uses_agree() {
    let src = "s = users & where (.age > 20)\n\
               q = union s s\n";
    let s = sql(src, "q");
    assert!(s.contains("WITH cagara_cte1"), "expected a CTE: {s}");
    assert_eq!(s.matches("age > 20").count(), 1, "{s}");
}

/// Lowering recurses once per IR node. The parser bounds a single pipeline,
/// but a parenthesized group restarts that count, so nesting multiplied the
/// total and overflowed the stack (SIGABRT) instead of reporting anything.
///
/// The nesting is built as IR directly: the parser rejects a source this deep
/// as a syntax error, which is the *other* half of the defence, and this test
/// pins the lowerer's own bound independently of it.
#[test]
fn deeply_nested_stages_are_a_diagnostic_not_a_stack_overflow() {
    use cagara_hir::ir::{Expr, Lit, Rel};
    let build = |n: usize| {
        let mut rel = Rel::Table {
            schema: "public".into(),
            name: "users".into(),
            columns: Some(vec!["id".into()]),
        };
        for _ in 0..n {
            rel = Rel::Order(Box::new(rel), vec![(Expr::Lit(Lit::Int(1)), true)]);
        }
        rel
    };
    // A realistic depth lowers: the bound must not reject real programs.
    assert!(compile(&build(8), Options::default()).is_ok());
    // Far past it is a diagnostic, whatever the stack size of the build, and
    // no depth may abort the process.
    let n = 20;
    let err = match compile(&build(n), Options::default()) {
        Err(e) => e,
        Ok(_) => panic!("expected a depth diagnostic at {n}"),
    };
    assert!(err.contains("folds more than"), "n={n}: {err}");
}

/// A stage costs one level of the depth budget, not two.
///
/// Erasure stamps every node with a transparent `Rel::At`, so a lowerer that
/// charged depth before looking through the wrapper counted each stage twice
/// and tripped the bound at seven stages instead of fifteen. That halved budget
/// is what made a ten-stage pipeline — five `leftJoin`s, the `update`s that
/// materialize their join keys, and a projection — report "folds more than 16
/// stages into one SELECT", whose advice used to be to split the query with a
/// named definition: advice that cannot work, because a definition is inlined
/// and the counter never resets.
///
/// Seven stages is the depth the old, doubled accounting accepted; it is
/// therefore the regression's lower bound. Fifteen is the bound the constant
/// promises, and sixteen must still be refused, so the fix cannot have removed
/// the diagnostic rather than corrected it.
#[test]
fn a_stage_costs_one_level_of_the_depth_budget() {
    let updates = |n: usize| {
        let mut src = "t : query { a = int, b = int } = table \"s\" \"t\"\nbig = t".to_string();
        for i in 0..n {
            src.push_str(&format!(" & update {{ k{i} = .a }}"));
        }
        src
    };
    // The depth the doubled accounting allowed: a fix must not regress here.
    assert!(run(&updates(7)).iter().all(|(_, r)| r.is_ok()));
    // The depth `MAX_LOWER_DEPTH` actually promises. Ten is the shape that
    // blocked the port, so it is asserted exactly rather than approached.
    for n in [10, 15] {
        let out = run(&updates(n));
        let bad: Vec<_> = out.iter().filter(|(_, r)| r.is_err()).collect();
        assert!(bad.is_empty(), "{n} stages must lower: {bad:?}");
    }
    // The bound still exists; at sixteen the diagnostic is unchanged.
    let err = run(&updates(16))
        .into_iter()
        .find_map(|(_, r)| r.err())
        .expect("expected a depth diagnostic at 16");
    assert!(err.contains("folds more than"), "{err}");
}

/// The bare-name and operator spellings of a set operation agree: both put the
/// piped query on the left.
///
/// The subject is the *last* argument (`except admins users` means
/// `users EXCEPT admins`), which is what `users & except admins` supplies. The
/// two spellings used to disagree — the bare name read its first argument as
/// the left operand — and only an asymmetric operation revealed it, because
/// `union` is symmetric.
#[test]
fn the_two_set_operation_spellings_put_the_piped_query_on_the_left() {
    let sqlish = |src: &str| sql_admins(src, "q");

    for (op, name, sql_op) in [
        ("& union", "union", "UNION"),
        ("& unionAll", "unionAll", "UNION ALL"),
        ("& intersect", "intersect", "INTERSECT"),
        ("& except", "except", "EXCEPT"),
    ] {
        let by_op = sqlish(&format!("q = users {op} admins\n"));
        let by_name = sqlish(&format!("q = users & {name} admins\n"));
        assert_eq!(
            by_op, by_name,
            "`users {op} admins` and `users & {name} admins` must agree"
        );
        assert!(
            by_name.contains(sql_op),
            "`{name}` must lower to `{sql_op}`: {by_name}"
        );
        let start = by_name.find("(SELECT").unwrap_or(0);
        let users = by_name[start..]
            .find("public.users")
            .expect("users operand");
        let admins = by_name[start..]
            .find("public.admins")
            .expect("admins operand");
        assert!(
            users < admins,
            "`users & {name} admins` must put `users` (piped) on the left: {by_name}"
        );
    }
}

/// The alias normalizer must ignore the chosen numbers without ignoring the
/// relations they name.
#[test]
fn alias_normalization_keeps_shape_but_not_numbers() {
    // Same shape, different numbering.
    assert_eq!(
        normalize_aliases("FROM a AS t1 JOIN b AS t2 ON t1.x = t2.y"),
        normalize_aliases("FROM a AS t3 JOIN b AS t4 ON t3.x = t4.y")
    );
    // The relations traded places, so the two are *not* the same text.
    assert_ne!(
        normalize_aliases("FROM a AS t1 JOIN b AS t2 ON t1.x = t2.y"),
        normalize_aliases("FROM a AS t1 JOIN b AS t2 ON t2.x = t1.y")
    );
    // A relation named like an alias keeps its own identity, in first-appearance
    // order; `__k` keys normalize too.
    assert_eq!(normalize_aliases("t7 AS t1"), "t#0 AS t#1");
    assert_eq!(
        normalize_aliases("SELECT __k1 ORDER BY __k1"),
        "SELECT __k#0 ORDER BY __k#0"
    );
    // String literals are not touched.
    assert_eq!(normalize_aliases("WHERE s = 't1'"), "WHERE s = 't1'");
}

/// A decimal cast carries its precision and scale, and stays a `float` in the
/// language.
///
/// `DECIMAL($1, $2)` cannot be written in a template: the text is re-parsed
/// after substitution, and a type's precision is syntax rather than an
/// expression. The intrinsic therefore takes them as separate arguments and
/// builds the type itself. The language has no decimal scalar, so the result
/// is `float` and only the SQL type is exact — which is the point, because
/// binary floating point cannot represent money exactly.
#[test]
fn a_decimal_cast_carries_its_precision() {
    let s = sql(
        "t : query { a = string } = table \"s\" \"t\"\n\
         q = t & select { v = toDecimal 18 2 .a }\n",
        "q",
    );
    assert!(s.contains("CAST(a AS DECIMAL(18, 2)) AS v"), "{s}");
}

/// `try*` emits a real `TRY_CAST`, not a `CAST`.
///
/// This is a regression test with teeth: sqlglot's parser reads
/// `TRY_CAST(x AS INT)` as a plain `Cast`, so a `sql` template spelling loses
/// the "try" and the cast fails on the first unparseable row instead of
/// yielding NULL. The intrinsic builds `Expr::TryCast` directly for that
/// reason, and this asserts the two spellings do not collapse into one.
#[test]
fn try_cast_is_not_a_plain_cast() {
    const SRC: &str = "t : query { a = string } = table \"s\" \"t\"\n\
                       q = t & select { v = tryInt .a }\n";
    // A target whose own `try` agrees with the language emits `TRY_CAST`, never a bare
    // `CAST`.
    let duck = dialect(SRC, "q", "spark").expect("spark");
    assert!(duck.contains("TRY_CAST(a AS BIGINT)"), "{duck}");
    // A target without one guards the cast — the cast is still there, but it cannot be
    // reached with a value the guard rejected, which is what "try" means. A bare
    // `CAST` on its own would have been the bug.
    let s = sql(SRC, "q");
    assert!(s.contains("REGEXP_LIKE"), "{s}");
    assert!(
        !s.contains("SELECT CAST(a AS BIGINT)"),
        "an unguarded cast is the bug: {s}"
    );
}

/// A cast that fails is worth failing on; a `try*` cast is not.
///
/// The two exist side by side on purpose. `icbase` types tens of thousands of
/// coded columns as `string`, and `tryInt` is what keeps one unparseable row
/// from failing the whole query — at the cost of a `maybe`, which the caller
/// has to handle.
#[test]
fn a_try_cast_yields_a_maybe() {
    const SRC: &str = "t : query { a = string } = table \"s\" \"t\"\n\
                       q = t & select { v = coalesce 0 (tryInt .a) }\n";
    // The default dialect is ANSI, which has no `TRY_CAST` — so it gets the guard.
    let s = sql(SRC, "q");
    assert!(s.contains("REGEXP_LIKE"), "{s}");
    assert!(s.contains("COALESCE"), "{s}");
    // A target whose own `try` already agrees with the language keeps it.
    let spark = dialect(SRC, "q", "spark").expect("spark");
    assert!(
        spark.contains("COALESCE((TRY_CAST(a AS BIGINT)), 0) AS v"),
        "{spark}"
    );
    // The targets without one still answer `maybe`: the guard yields NULL, which the
    // `coalesce` then replaces — so the shape of the result is the same everywhere even
    // though the spelling is not.
    let pg = dialect(SRC, "q", "postgres").expect("postgres");
    assert!(pg.contains("REGEXP_LIKE"), "{pg}");
    assert!(pg.contains("COALESCE"), "{pg}");
    let sqlite = dialect(SRC, "q", "sqlite").expect("sqlite");
    assert!(sqlite.contains("'0123456789'"), "{sqlite}");
    assert!(sqlite.contains("COALESCE"), "{sqlite}");
}

// ---------------------------------------------------------------- correctness
// regressions: templates, sorting, windows, dedup, pagination

/// A `sql` template's `$n` must stand alone in expression position.
///
/// Two shapes used to reach the database silently: a placeholder in a *type*
/// position (`CAST(id AS $1)`) parsed as a name and was emitted verbatim, and a
/// template whose tail the SQL parser discards (`$1 +`) came out as just `$1`.
#[test]
fn a_template_placeholder_must_be_an_expression() {
    for (src, want) in [
        (
            "bad : expr ?r int -> expr ?r string = sql \"CAST(id AS $1)\"\n\
             q = users & select { x = bad .id }\n",
            "not in an expression position",
        ),
        (
            "bad : expr ?r int -> expr ?r int = sql \"$1 +\"\n\
             q = users & select { x = bad .id }\n",
            "ends with `+`",
        ),
        (
            "bad : expr ?r int -> expr ?r int = sql \"$1,\"\n\
             q = users & select { x = bad .id }\n",
            "ends with `,`",
        ),
    ] {
        let err = run(src)
            .into_iter()
            .find_map(|(_, r)| r.err())
            .unwrap_or_else(|| panic!("expected a template diagnostic for {src:?}"));
        assert!(err.contains(want), "expected {want:?}, got {err:?}");
    }

    // A well-formed template still lowers.
    let s = sql(
        "abs1 : expr ?r int -> expr ?r int = sql \"ABS($1)\"\n\
         q = users & select { x = abs1 .id }\n",
        "q",
    );
    assert!(s.contains("ABS(id)"), "{s}");
}

/// A second `order` sorts rows that are already sorted, so its keys lead and
/// the earlier ones become tiebreakers. Replacing them instead made the first
/// `order` silently dead.
#[test]
fn a_later_order_becomes_the_leading_sort_key() {
    let s = sql("q = users & order [asc .age] & order [desc .id]\n", "q");
    assert!(
        s.contains("ORDER BY id DESC NULLS LAST, age NULLS LAST"),
        "{s}"
    );
    // The same key twice is one sort key.
    let s = sql("q = users & order [asc .id] & order [asc .id]\n", "q");
    assert_eq!(s.matches("ORDER BY").count(), 1, "{s}");
}

/// An ordered aggregate window gets the running frame written out.
///
/// SQL's default with an `ORDER BY` is `RANGE … CURRENT ROW`, which folds peer
/// rows into the same value — so `over { …, order = […] } (sum `) was not a running
/// total whenever the order key repeated. A ranking function ignores the frame,
/// so it stays frameless and its SQL stays short.
#[test]
fn an_ordered_aggregate_window_gets_a_running_frame() {
    let s = sql(
        "q = users & select { .id, run = over { partition = [.active], order = [asc .age] } (sum .age) }\n",
        "q",
    );
    assert!(
        s.contains("ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"),
        "{s}"
    );
    let r = sql(
        "q = users & select { .id, n = rowNumber { order = [asc .age] } }\n",
        "q",
    );
    assert!(!r.contains("ROWS BETWEEN"), "ranking ignores frames: {r}");
    // An explicit frame is left alone.
    let f = sql(
        "q = users & select { .id, t = over { partition = [.active] } (sum .age) }\n",
        "q",
    );
    assert!(!f.contains("ROWS BETWEEN"), "no order, no frame: {f}");
}

/// `distinct` after an `order` whose key the projection dropped is refused.
///
/// SQL rejects `SELECT DISTINCT a … ORDER BY b`, and carrying `b` into the
/// select list would dedupe on `(a, b)` — a different query that keeps rows the
/// user asked to merge. Both readings are wrong, so the pair is an error.
#[test]
fn distinct_refuses_a_sort_key_the_projection_dropped() {
    let err = run("q = users & order [asc .age] & select { .name } & distinct\n")
        .into_iter()
        .find_map(|(_, r)| r.err())
        .expect("expected a diagnostic");
    assert!(err.contains("cannot follow an `order`"), "{err}");
    // A key that survives the projection is fine.
    let s = sql(
        "q = users & order [asc .age] & select { .name, .age } & distinct\n",
        "q",
    );
    assert!(s.contains("DISTINCT"), "{s}");
}

/// A nullable value is handled by the *signature* it selects, not by a
/// `coalesce` sentinel — so the SQL is the SQL a hand-written query would have.
///
/// This is the shape the sentinel used to spoil: `where (coalesce 0 .total >
/// 100)` keeps a group whose `SUM` is NULL, while `where (.total > just 100)`
/// is `WHERE (total > 100)` and drops it — the same rows SQL drops.
#[test]
fn nullable_operands_take_the_nullable_signature() {
    let s = sql(
        "q = orders & agg { total = sum .amount } & where (.total > just 100.0)\n",
        "q",
    );
    assert!(s.contains("HAVING SUM(amount) > 100.0"), "{s}");
    assert!(!s.contains("COALESCE(SUM"), "no sentinel: {s}");

    // A nullable column groups as itself: NULL is its own group, and it stays
    // in the row type.
    let s = sql(
        "q = orders & leftJoin (users & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id) & agg { k = group .name, n = count }\n",
        "q",
    );
    assert!(s.contains("GROUP BY t2.name"), "{s}");
    assert!(!s.contains("COALESCE(t2.name"), "no sentinel: {s}");

    // A nullable sort key sorts last, which the language already fixed.
    let s = sql(
        "q = orders & leftJoin (users & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id) & order [desc .name]\n",
        "q",
    );
    assert!(s.contains("ORDER BY t2.name DESC NULLS LAST"), "{s}");

    // Two nullable operands compare to a `maybe bool`, and `where` drops the
    // rows whose condition is NULL.
    let s = sql(
        "q = orders & leftJoin (users & select { u_id = .id, name = .name, age = .age, active = .active }) (.<user_id == .>u_id) & where (.name == just \"ada\")\n",
        "q",
    );
    assert!(s.contains("WHERE (t2.name = 'ada')"), "{s}");

    // Mixing a nullable with a non-null value is still a type error: no
    // implicit lift, and the message names `just`.
    let err = run("q = orders & agg { t = sum .amount } & where (.t > 100.0)\n")
        .into_iter()
        .find_map(|(_, r)| r.err())
        .expect("expected a diagnostic");
    assert!(err.contains("just"), "{err}");
}

/// A join can be told which right columns its output keeps, and under what
/// names — `take` is a record over the same pair the predicate reads.
///
/// An overlapping join is expressed by reshaping the operand, not by a join
/// spelling of its own.
///
/// `users` and `orders` both have `id`, and the anonymous merge drops the right
/// one in silence — so a name both sides have is a type error. The way through
/// is `select` on the right operand: it names `orders.id` as `order_id` and keeps
/// only the columns the join needs.
#[test]
fn an_overlapping_join_reshapes_the_operand() {
    let src = "u2 : query { id = int, name = string } = table \"s\" \"u\"\n\
               o2 : query { id = int, user_id = int, amount = float } = table \"s\" \"o\"\n\
               q = u2 & innerJoin (o2 & select { user_id = .user_id, order_id = .id }) \
                   (.<id == .>user_id)\n";
    let out = sql(src, "q");
    assert!(
        out.contains("SELECT t1.id, t1.name, t2.user_id, t2.id AS order_id FROM s.u AS t1"),
        "{out}"
    );
    assert!(
        out.contains("INNER JOIN s.o AS t2 ON t1.id = t2.user_id"),
        "{out}"
    );

    // A left join keeps every left row, so the right side's columns are
    // NULL-extended — the SQL says nothing about it, and the *type* records
    // that they may be missing.
    let src = "u2 : query { id = int } = table \"s\" \"u\"\n\
               o2 : query { user_id = int, amount = float } = table \"s\" \"o\"\n\
               q = u2 & leftJoin (o2 & select { user_id = .user_id, amount = .amount }) \
                   (.<id == .>user_id)\n";
    let out = sql(src, "q");
    assert!(
        out.contains("SELECT t1.id, t2.user_id, t2.amount FROM s.u AS t1 LEFT JOIN s.o AS t2"),
        "{out}"
    );
}

/// A bare order key is ascending, so `asc` is the optional spelling.
///
/// `order` takes a list of sort keys, and an expression coerces to one with
/// `NULLS LAST` in the ascending direction — the direction SQL defaults to.
/// Writing `asc .x` says the same thing; `fmt --canonical` (not built yet)
/// would drop it.
#[test]
fn an_order_key_must_say_its_direction() {
    // `order [.a]` used to be read as ascending, which made a missing `asc`
    // indistinguishable from a deliberate one. A key now says `asc` or `desc`,
    // and the diagnostic says so.
    let e = error("q = users & order [.id]\n", "q");
    assert!(e.contains("must say its direction"), "{e}");
    assert!(e.contains("asc <expr>` or `desc <expr>"), "{e}");
    // The same rule inside a window specification, which takes an order list too.
    let e = error(
        "q = users & select { r = rowNumber { order = [.id] } }\n",
        "q",
    );
    assert!(e.contains("must say its direction"), "{e}");
    // Both spellings are keys, and both lower to `ORDER BY … NULLS LAST`.
    let src = "t : query { a = int, b = string } = table \"s\" \"t\"\n\
               q = t & order [asc .a, desc .b]\n";
    let s = sql(src, "q");
    assert!(
        s.contains("ORDER BY a NULLS LAST, b DESC NULLS LAST"),
        "{s}"
    );
}

/// An aggregate is a value, and three combinators modify one.
///
/// This is the shape that replaces sixteen names (`*Filter`, `*Over`,
/// `countDistinct`): the aggregate keeps its name, and filtering, distinctness
/// and windowing are applications of it. The rewrites are the two an aggregate
/// can have — a `CASE` for the filter (every engine understands it; `FILTER
/// (WHERE …)` does not) and `DISTINCT` inside the call — so the SQL is what a
/// hand-written query would say.
#[test]
fn an_aggregate_takes_a_filter_a_distinct_or_a_window() {
    let src = "o2 : query { id = int, user_id = int, amount = float, status = string } = table \"s\" \"o\"\n\
               q = o2 & agg {
                 paid = filtered (.status == \"paid\") (sum .amount),
                 who = distinctAgg (countOf .user_id),
                 dp = distinctAgg (filtered (.status == \"paid\") (sum .amount))
               }\n";
    let s = sql(src, "q");
    assert!(
        s.contains("SUM(amount) FILTER (WHERE status = 'paid') AS paid"),
        "{s}"
    );
    assert!(s.contains("COUNT(DISTINCT user_id) AS who"), "{s}");
    // Composition: the filter and the distinct both apply to the same call.
    assert!(
        s.contains("SUM(DISTINCT amount) FILTER (WHERE status = 'paid') AS dp"),
        "{s}"
    );

    // `over` is the same aggregate at window phase.
    let src = "o2 : query { id = int, user_id = int, amount = float } = table \"s\" \"o\"\n\
               w = o2 & select { id = .id, total = over { partition = [.user_id] } (sum .amount) }\n";
    let s = sql(src, "w");
    assert!(
        s.contains("SUM(amount) OVER (PARTITION BY user_id) AS total"),
        "{s}"
    );

    // An aggregate with no argument is `COUNT(*)`, and the `FILTER` clause is
    // what makes the predicate's rows the ones counted — no argument to wrap.
    let src = "o2 : query { status = string } = table \"s\" \"o\"\n\
               n = o2 & agg { paid = filtered (.status == \"paid\") count }\n";
    let s = sql(src, "n");
    assert!(
        s.contains("COUNT(*) FILTER (WHERE status = 'paid') AS paid"),
        "{s}"
    );
    // Without the clause, the `*` becomes the constant the predicate counts.
    let m = dialect(src, "n", "mysql").expect("mysql");
    assert!(
        m.contains("COUNT(CASE WHEN status = 'paid' THEN 1 END) AS paid"),
        "{m}"
    );

    // Applying `distinctAgg` twice is applying it once: the rewrite does not
    // stack `DISTINCT DISTINCT`.
    let src = "o2 : query { user_id = int } = table \"s\" \"o\"\n\
               d = o2 & agg { n = distinctAgg (distinctAgg (countOf .user_id)) }\n";
    assert_eq!(
        sql(src, "d").matches("DISTINCT").count(),
        1,
        "{}",
        sql(src, "d")
    );
}

/// The timestamp half of the format pair, and the two targets that refuse a
/// format parser or a `TRY_CAST`.
#[test]
fn a_timestamp_format_is_written_once_too() {
    let src = "t : query { s = string, ts = timestamp } = table \"s\" \"t\"\n\
               q = t & select { p = parseTimestamp \"%Y-%m-%d %H:%M:%S\" .s, f = formatTimestamp \"%Y-%m\" .ts }\n";
    let pg = dialect(src, "q", "postgres").expect("postgres");
    assert!(
        pg.contains("TO_TIMESTAMP(s, 'YYYY-MM-DD HH24:MI:SS')"),
        "{pg}"
    );
    let duck = dialect(src, "q", "duckdb").expect("duckdb");
    assert!(duck.contains("STRPTIME(s, '%Y-%m-%d %H:%M:%S')"), "{duck}");
    // No narrowing cast: the parser's own answer is the type.
    assert!(!duck.contains("AS DATE"), "{duck}");
    let bq = dialect(src, "q", "bigquery").expect("bigquery");
    assert!(
        bq.contains("PARSE_TIMESTAMP('%Y-%m-%d %H:%M:%S', s)"),
        "{bq}"
    );
    assert!(bq.contains("FORMAT_TIMESTAMP('%Y-%m', ts)"), "{bq}");

    // BigQuery's `PARSE_DATE` reads date elements only, so a time of day is a
    // compile error there rather than SQL the server rejects.
    let src = "t : query { s = string } = table \"s\" \"t\"\n\
               q = t & select { p = parseDate \"%Y-%m-%d %H\" .s }\n";
    let e = dialect(src, "q", "bigquery").unwrap_err();
    assert!(e.contains("reads only date elements"), "{e}");
    // The date-only format still compiles there.
    let src = "t : query { s = string } = table \"s\" \"t\"\n\
               q = t & select { p = parseDate \"%Y-%m-%d\" .s }\n";
    assert!(dialect(src, "q", "bigquery").is_ok());
}

/// PostgreSQL has no `TRY_CAST`, so the cast is guarded rather than refused.
///
/// The guard is a regex test beside the cast, not `pg_input_is_valid`: that function
/// arrived in PostgreSQL 16 and this project does not require it, while `~` has been
/// there since 7.4.
#[test]
fn a_try_cast_is_guarded_on_postgres() {
    let src = "t : query { a = string } = table \"s\" \"t\"\n\
               q = t & select { v = tryInt .a }\n";
    let pg = dialect(src, "q", "postgres").expect("postgres");
    assert!(
        pg.contains("REGEXP_LIKE(TRIM(a), '^[+-]?[0-9]+$')"),
        "the guard is a regex on the integer shape, over the trimmed input: {pg}"
    );
    assert!(pg.contains("ELSE NULL"), "a failure is NULL: {pg}");
    // DuckDB *has* `TRY_CAST` and is guarded anyway: `TRY_CAST('1.5' AS BIGINT)` is `2`
    // there (it rounds) and `' 42 '` parses, so the engine's own answer is not the
    // language's. The guard is what makes `tryInt` mean one thing everywhere.
    let duck = dialect(src, "q", "duckdb").expect("duckdb");
    assert!(
        duck.contains("regexp_matches(TRIM(a), '^[+-]?[0-9]+$')"),
        "{duck}"
    );
}

/// The decimal cast is spelled the same way in every backend that has the type,
/// and that is worth pinning: `DECIMAL(10, 2)` is what PostgreSQL, MySQL, Trino,
/// Spark, BigQuery, Snowflake and T-SQL all read, while DuckDB's second parse of
/// the same text agrees, so one spelling covers the family.
#[test]
fn a_decimal_cast_says_decimal_everywhere() {
    let src = "o : query { total = string } = table \"s\" \"o\"\n\
               q = o & select { x = toDecimal 10 2 .total }\n";
    for d in [
        "duckdb",
        "postgres",
        "mysql",
        "trino",
        "spark",
        "bigquery",
        "snowflake",
        "tsql",
    ] {
        let sql = dialect(src, "q", d).unwrap_or_else(|e| panic!("{d}: {e}"));
        assert!(
            sql.contains("DECIMAL(10, 2)"),
            "{d}: the width is in the statement\n  {sql}"
        );
    }
    // PostgreSQL spells a cast with `::`; that is the only difference.
    let pg = dialect(src, "q", "postgres").expect("postgres");
    assert!(pg.contains("::DECIMAL(10, 2)"), "{pg}");
    let duck = dialect(src, "q", "duckdb").expect("duckdb");
    assert!(duck.contains("CAST(total AS DECIMAL(10, 2))"), "{duck}");
}

/// A decimal is exact or it is refused: SQLite cannot hold one.
///
/// Its `NUMERIC` affinity stores what it is given as INTEGER or REAL, so a value
/// cast to `DECIMAL(10, 2)` there is a binary float wearing the type's name —
/// which is the one thing `decimal` exists to prevent.
#[test]
fn a_decimal_is_refused_where_it_cannot_be_exact() {
    let src = "o : query { total = string } = table \"s\" \"o\"\n\
               q = o & select { x = toDecimal 10 2 .total }\n";
    let duck = dialect(src, "q", "duckdb").expect("duckdb");
    assert!(duck.contains("CAST(total AS DECIMAL(10, 2))"), "{duck}");
    let pg = dialect(src, "q", "postgres").expect("postgres");
    assert!(pg.contains("total::DECIMAL(10, 2)"), "{pg}");
    let e = dialect(src, "q", "sqlite").unwrap_err();
    assert!(e.contains("needs an exact decimal type"), "{e}");
    assert!(e.contains("NUMERIC affinity"), "{e}");
}

/// `lift` writes the NULL guard, and the function it takes is written once.
#[test]
fn lift_guards_the_null_and_keeps_the_function() {
    let src = "t : query { d = maybe string, xs = maybe (list int) } = table \"s\" \"t\"\n\
               q = t & select { p = lift (parseDate \"%Y%m%d\") .d, n = lift listLength .xs }\n";
    let duck = dialect(src, "q", "duckdb").expect("duckdb");
    assert!(
        duck.contains("CASE WHEN d IS NULL THEN NULL ELSE CAST(STRPTIME(d, '%Y%m%d') AS DATE) END"),
        "{duck}"
    );
    assert!(
        duck.contains("CASE WHEN xs IS NULL THEN NULL ELSE len(xs) END"),
        "{duck}"
    );
    let pg = dialect(src, "q", "postgres").expect("postgres");
    assert!(
        pg.contains("CASE WHEN d IS NULL THEN NULL ELSE TO_DATE(d, 'YYYYMMDD')::DATE END"),
        "{pg}"
    );
    assert!(
        pg.contains("CASE WHEN xs IS NULL THEN NULL ELSE cardinality(xs) END"),
        "{pg}"
    );
}

/// One `parseDate`/`formatDate`, every target's own spelling underneath.
///
/// The language writes `%Y %m %d %H %M %S`; each family reads something else,
/// and two of them differ in ways that would be silent: PostgreSQL wants
/// `YYYY MM DD HH24 MI SS`, and MySQL's dialect (which Trino's `date_parse`
/// follows) reads `%i` for minutes where the C-style `%M` means the month's
/// name. The format is translated here so a query means the same thing
/// wherever it runs.
#[test]
fn a_date_format_is_written_once_and_spelled_per_target() {
    let src = concat!(
        "t : query { s = string, d = date } = table \"s\" \"t\"\n",
        "q = t & select { p = parseDate \"%Y%m%d\" .s, f = formatDate \"%Y-%m-%d %H:%M:%S\" .d }\n",
    );
    let pg = dialect(src, "q", "postgres").expect("postgres");
    assert!(pg.contains("TO_DATE(s, 'YYYYMMDD')"), "{pg}");
    assert!(pg.contains("TO_CHAR(d, 'YYYY-MM-DD HH24:MI:SS')"), "{pg}");
    let duck = dialect(src, "q", "duckdb").expect("duckdb");
    assert!(
        duck.contains("CAST(STRPTIME(s, '%Y%m%d') AS DATE)"),
        "{duck}"
    );
    assert!(duck.contains("STRFTIME(d, '%Y-%m-%d %H:%M:%S')"), "{duck}");
    let trino = dialect(src, "q", "trino").expect("trino");
    assert!(
        trino.contains("CAST(DATE_PARSE(s, '%Y%m%d') AS DATE)"),
        "{trino}"
    );
    // Trino's minutes are `%i`, not `%M` — `%M` there is the month's name.
    assert!(
        trino.contains("DATE_FORMAT(d, '%Y-%m-%d %H:%i:%s')"),
        "{trino}"
    );
    let spark = dialect(src, "q", "spark").expect("spark");
    assert!(spark.contains("TO_DATE(s, 'yyyyMMdd')"), "{spark}");
    let bq = dialect(src, "q", "bigquery").expect("bigquery");
    assert!(bq.contains("PARSE_DATE('%Y%m%d', s)"), "{bq}");
    let snow = dialect(src, "q", "snowflake").expect("snowflake");
    assert!(snow.contains("TO_DATE(s, 'YYYYMMDD')"), "{snow}");

    // SQLite has no format *parser* and its `strftime` has no *name* specifiers, but
    // the format is compiled in, so neither is a refusal: the call is decomposed into
    // one call per field. Parsing rebuilds the ISO string from fixed-width slices;
    // formatting concatenates literal text with single-specifier calls.
    let sqlite = dialect(src, "q", "sqlite").expect("sqlite");
    assert!(
        sqlite.contains("SUBSTR(s, 1, 4)") && sqlite.contains("SUBSTR(s, 5, 2)"),
        "the date is rebuilt from fixed-width slices: {sqlite}"
    );
    assert!(
        sqlite.contains("STRFTIME('%Y', d)") && sqlite.contains("STRFTIME('%H', d)"),
        "one call per numeric field: {sqlite}"
    );
    let src = "t : query { d = date } = table \"s\" \"t\"\n\
               q = t & select { f = formatDate \"%Y-%m\" .d, n = monthName .d }\n";
    let sqlite = dialect(src, "q", "sqlite").expect("sqlite format");
    assert!(sqlite.contains("STRFTIME('%Y', d)"), "{sqlite}");
    // A name has no `strftime` specifier, so it becomes a `CASE` over the month
    // number — the same table every other dialect prints.
    assert!(
        sqlite.contains("WHEN '2' THEN 'February'"),
        "the month name is a CASE: {sqlite}"
    );
    let tsql = dialect(src, "q", "tsql").expect("tsql format");
    assert!(tsql.contains("FORMAT(d, 'yyyy-MM')"), "{tsql}");

    // The format is compiled in, so it must be a literal — and only the
    // specifiers the language defines are translated. A *column* cannot even be
    // offered: the parameter is a row-free `string`, so the checker refuses it
    // before the backend sees the call; a row-free *expression* gets as far as
    // the translation, which is where the message is.
    let src = "t : query { s = string } = table \"s\" \"t\"\n\
               q = t & select { p = parseDate (\"%Y\" ++ \"%m%d\") .s }\n";
    let e = dialect(src, "q", "duckdb").unwrap_err();
    assert!(e.contains("literal format string"), "{e}");
    // `%q` names nothing: the month and weekday *names* are supported (`%b`,
    // `%B`, `%a`, `%A`) and so is the day of the year (`%j`), so the unsupported
    // specifier this test uses has to be one the language really does not have.
    let src = "t : query { s = string } = table \"s\" \"t\"\n\
               q = t & select { p = parseDate \"%q\" .s }\n";
    let e = dialect(src, "q", "duckdb").unwrap_err();
    assert!(e.contains("`%q` is not a format specifier"), "{e}");
}

/// A format conversion is spelled by the target: `SPECIFIC` functions differ
/// (`TO_CHAR`/`TO_TIMESTAMP` on PostgreSQL, `DATE_FORMAT`/`DATE_PARSE` on Trino,
/// `STRFTIME`/`STRPTIME` on DuckDB), and the same `sql` template reaches all
/// three. Before this, every dialect got sqlglot's `TO_CHAR`, which only
/// PostgreSQL reads — DuckDB and Trino both reject it, checked against their
/// docs and, for DuckDB, against the engine.
#[test]
fn a_format_conversion_is_spelled_by_the_target() {
    let src = concat!(
        "t : query { s = string, d = date } = table \"s\" \"t\"\n",
        "q = t & select { ts = parseFmt .s, out = formatFmt .d }\n",
        "parseFmt : expr ?r string -> expr ?r timestamp = sql \"STR_TO_DATE($1, '%Y%m%d')\"\n",
        "formatFmt : expr ?r date -> expr ?r string = sql \"DATE_FORMAT($1, '%Y-%m-%d')\"\n",
    );
    let trino = dialect(src, "q", "trino").expect("trino");
    assert!(trino.contains("date_parse(s, '%Y%m%d')"), "{trino}");
    assert!(trino.contains("date_format(d, '%Y-%m-%d')"), "{trino}");
    let duck = dialect(src, "q", "duckdb").expect("duckdb");
    assert!(duck.contains("strptime(s, '%Y%m%d')"), "{duck}");
    assert!(duck.contains("strftime(d, '%Y-%m-%d')"), "{duck}");
    // PostgreSQL keeps sqlglot's spelling — and its own format vocabulary, which
    // is why the format string is the writer's problem, not the backend's.
    let pg = dialect(src, "q", "postgres").expect("postgres");
    assert!(pg.contains("TO_TIMESTAMP(s, 'YYYYMMDD')"), "{pg}");
    assert!(pg.contains("TO_CHAR(d, 'YYYY-MM-DD')"), "{pg}");
}

/// The padding helpers put their arguments where SQL does.
///
/// `lpad n fill s` is documented as `LPAD(s, n, fill)`, and the template had the
/// fill and the string swapped: `lpad 3 "0" .s` emitted `LPAD('0', 3, s)`, which
/// pads the *fill* to the width with copies of `s` — `077` for `s = '7'`, where
/// the answer is `007`. Silently wrong rather than rejected, so it has a test.
#[test]
fn the_padding_helpers_pass_their_arguments_in_sql_order() {
    let src = "t : query { s = string } = table \"s\" \"t\"\n\
               q = t & select { l = lpad 3 \"0\" .s, r = rpad 3 \"0\" .s }\n";
    let s = sql(src, "q");
    assert!(s.contains("LPAD(s, 3, '0') AS l"), "{s}");
    assert!(s.contains("RPAD(s, 3, '0') AS r"), "{s}");
}

/// A one-argument function works over a value as well as over a column, and the
/// two uses emit the same SQL.
///
/// There is one signature and no second overload: `truncYear` is `expr ?r date ->
/// expr ?r date`, and an expression that reads no row *is* its value (see the
/// checker's coercion). So `truncYear currentDate` is a `date` and `truncYear
/// .d` reads the column, both through the same template.
#[test]
fn a_one_argument_function_works_over_a_value_and_a_column() {
    // A value: `truncYear` applied to the constant `currentDate`, in a field.
    let src = "t : query { d = date } = table \"s\" \"t\"\n\
               q = t & select { y = truncYear currentDate }\n";
    let s = sql(src, "q");
    assert!(
        s.contains("CAST(DATE_TRUNC('YEAR', CURRENT_DATE) AS DATE) AS y"),
        "{s}"
    );

    // The same function over a column.
    let src = "t : query { d = date } = table \"s\" \"t\"\n\
               q = t & select { y = truncYear .d }\n";
    let s = sql(src, "q");
    assert!(
        s.contains("CAST(DATE_TRUNC('YEAR', d) AS DATE) AS y"),
        "{s}"
    );

    // A string function over a literal and over a column: same SQL either way,
    // because both are the same template.
    let src = "t : query { name = string } = table \"s\" \"t\"\n\
               q = t & select { u = upper \"abc\" }\n";
    assert_eq!(sql(src, "q"), "SELECT UPPER('abc') AS u FROM s.t");
    let src = "t : query { name = string } = table \"s\" \"t\"\n\
               q = t & select { u = upper .name }\n";
    assert_eq!(sql(src, "q"), "SELECT UPPER(name) AS u FROM s.t");

    // A value produced by a value flows into a predicate without a row.
    let src = "t : query { d = date } = table \"s\" \"t\"\n\
               q = t & where (.d >= truncMonth currentDate)\n";
    let s = sql(src, "q");
    assert!(s.contains("DATE_TRUNC('MONTH', CURRENT_DATE)"), "{s}");
}

/// A join's select list is the left side, then the fields it takes.
///
/// The order is the row's order (which `--types` shows and the HIR test
/// `a_join_can_take_named_columns` pins), and the SQL has to name the same
/// columns in the same order — a take field that names a column the left side
/// does not have is where an ordered list and a merged map differ.
#[test]
fn a_join_lists_the_left_side_then_what_it_takes() {
    let src = "u : query { id = int, name = string } = table \"s\" \"u\"\n\
               o : query { id = int, user_id = int, amount = float } = table \"s\" \"o\"\n";
    for (join, side) in [
        ("leftJoin", "LEFT"),
        ("rightJoin", "RIGHT"),
        ("innerJoin", "INNER"),
    ] {
        let q = format!(
            "{src}q = u & {join} (o & select {{ user_id = .user_id, order_id = .id, amount = .amount }}) \
             (.<id == .>user_id)\n"
        );
        let s = sql(&q, "q");
        assert!(
            s.contains(
                "SELECT t1.id, t1.name, t2.user_id, t2.id AS order_id, t2.amount FROM s.u AS t1"
            ),
            "{join}: {s}"
        );
        assert!(s.contains(&format!("{side} JOIN s.o AS t2")), "{join}: {s}");
        assert!(s.contains("ON t1.id = t2.user_id"), "{join}: {s}");
    }
}

/// The six ways the backend used to compute the wrong *numbers*, in one place.
///
/// Each of these was a silent wrong answer rather than an error — the query
/// compiled, ran, and returned something a reader would not expect. They have
/// individual tests above; this one exists so the whole list is auditable at a
/// glance, and so a regression in any of them is reported by a single failure
/// whose comment says what the answer used to be.
#[test]
fn the_six_wrong_number_bugs_stay_fixed() {
    // `users` and `orders` come from the fixture; `vips` is a second query with
    // the same row, so `except`/`union` have something to work on.
    let schema = "vips : query { id = int, name = string, age = int, active = bool } = \
                  table \"public\" \"vips\"\n";

    // 1. `except` / `intersect` are not commutative: the *subject* is the left
    //    operand. Taking the first argument instead returned the complement.
    let s = sql(&format!("{schema}q = users & except vips\n"), "q");
    assert!(
        s.contains("FROM public.users") && s.contains("EXCEPT") && s.contains("FROM public.vips"),
        "`users & except vips` must be `users EXCEPT vips`: {s}"
    );
    assert!(
        s.find("FROM public.users").expect("users") < s.find("EXCEPT").expect("except"),
        "the subject comes first: {s}"
    );

    // 2. A window with an `order` and no `frame` used to get the *whole
    //    partition* as its frame, so a running total was a full total.
    let s = sql(
        &format!(
            "{schema}q = orders & select {{ id = .id, total = over {{ partition = [.user_id], \
             order = [asc .id] }} (sum .amount) }}\n"
        ),
        "q",
    );
    assert!(
        s.contains("ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"),
        "an ordered window is a running frame: {s}"
    );

    // 3. An `order` before a set operation used to be appended to the whole
    //    result; it belongs to that branch.
    let s = sql(
        &format!("{schema}p = users & order [desc .id]\nq = union p vips\n"),
        "q",
    );
    assert!(
        s.contains("ORDER BY id DESC") && s.contains("UNION"),
        "the branch keeps its own order: {s}"
    );

    // 4. `distinct` used to drop a hidden key that a later stage still read.
    let s = sql(
        &format!(
            "{schema}q = orders & select {{ k = .user_id }} & distinct & select {{ k = .k }}\n"
        ),
        "q",
    );
    assert!(s.contains("DISTINCT"), "{s}");
    assert_eq!(
        s.matches("AS k").count(),
        1,
        "the hidden key is projected once: {s}"
    );

    // 5. `limit` and `offset` in one pipeline used to lower to a nested pair
    //    whose `OFFSET` applied after the `LIMIT`, which returns no rows.
    //    `limit 10 & offset 5` keeps rows 6..=10, so the one statement that says
    //    that is `LIMIT 5 OFFSET 5`; `rewrite` folds the pair into it.
    let s = sql(
        &format!("{schema}q = users & order [asc .id] & limit 10 & offset 5\n"),
        "q",
    );
    assert!(
        s.contains("LIMIT 5") && s.contains("OFFSET 5"),
        "both survive: {s}"
    );
    assert!(
        s.find("LIMIT 5").expect("limit") < s.find("OFFSET 5").expect("offset"),
        "`LIMIT` then `OFFSET`, in one statement: {s}"
    );
    // The other order is already one clause and stays as written.
    let s = sql(
        &format!("{schema}q = users & order [asc .id] & offset 5 & limit 3\n"),
        "q",
    );
    assert!(s.contains("LIMIT 3 OFFSET 5"), "{s}");

    // 6. A template placeholder has to be an expression on its own: `"$1 +"`
    //    silently truncated the SQL, and `CAST(id AS $1)` produced broken SQL.
    let e = error(
        &format!("{schema}f : expr ?r int -> expr ?r int = sql \"$1 +\"\nq = users & select {{ x = f .id }}\n"),
        "q",
    );
    assert!(e.contains("ends with `+`"), "{e}");
    // A placeholder may not stand where a *type* belongs: `CAST(id AS $1)`
    // produced SQL the engine rejects, and the checker used to accept it.
    let e = error(
        &format!("{schema}f : expr ?r int -> expr ?r int = sql \"CAST($1 AS $2)\"\nq = users & select {{ x = f .id }}\n"),
        "q",
    );
    assert!(e.contains("placeholder") || e.contains("signature"), "{e}");
}

/// Every stage shorthand writes the **same SQL** as the stage it spells.
///
/// A shorthand is a wrapper in the prelude, not a second implementation, so this
/// is the property that keeps it honest: if `&?` ever stopped meaning `where` —
/// a different nullability, a lost `ORDER BY`, a wrong join kind — the two
/// compiled statements would differ here. The named form is the reference, and
/// the assertion is equality, not "looks similar".
#[test]
fn every_stage_shorthand_writes_the_same_sql() {
    let pairs: &[(&str, &str)] = &[
        ("&?", "& where (.active)"),
        ("&=", "& select {.id, .name}"),
        ("&+", "& update {n = .age + 1}"),
        ("&*", "& agg {n = count}"),
        ("&.", "& order [asc .age]"),
        ("&-", "& limit 5"),
        // Set operations need a second input of the same row.
        ("&|", "& union (users & where (.active))"),
        ("&!", "& unionAll (users & where (.active))"),
        ("&^", "& intersect (users & where (.active))"),
        ("&~", "& except (users & where (.active))"),
    ];
    for (op, named) in pairs {
        // The shorthand applies to the same subject the named form does.
        let short = format!("q = users {op} {}\n", shorthand_arg(named));
        let long = format!("q = users {named}\n");
        let a = sql(&short, "q");
        let b = sql(&long, "q");
        assert_eq!(a, b, "`{op}` and `{named}` must compile alike");
    }

    // The join shorthands: the piped query is the *left* side of the join in
    // both spellings, which is what the operator's own level is for.
    let joins: &[(&str, &str)] = &[
        ("?", "innerJoin"),
        ("<?", "leftJoin"),
        ("?>", "rightJoin"),
        ("<?>", "fullJoin"),
    ];
    for (op, named) in joins {
        let src = "o = orders & select { user_id = .user_id, amount = .amount }\n";
        let a = sql(
            &format!("{src}q = users & o {op} (.<id == .>user_id)\n"),
            "q",
        );
        let b = sql(
            &format!("{src}q = users & {named} o (.<id == .>user_id)\n"),
            "q",
        );
        assert_eq!(a, b, "`{op}` and `{named}` must compile alike");
    }

    // `$` is application and `<<<` is reversed composition: both are pure
    // syntax, so the value they produce is the same value.
    assert_eq!(
        sql("q = users & select { x = upper $ \"abc\" }\n", "q"),
        sql("q = users & select { x = upper \"abc\" }\n", "q")
    );
    assert_eq!(
        sql("q = users & select { x = (upper <<< trim) .name }\n", "q"),
        sql("q = users & select { x = upper (trim .name) }\n", "q")
    );
    // `<>` is the same concatenation as `++`.
    assert_eq!(
        sql("q = users & select { x = .name <> \"!\" }\n", "q"),
        sql("q = users & select { x = .name ++ \"!\" }\n", "q")
    );
}

/// The argument a shorthand takes, from the named form it stands for: `&? ` keeps
/// everything after the stage's name.
fn shorthand_arg(named: &str) -> &str {
    let rest = named
        .strip_prefix("& ")
        .unwrap_or_else(|| panic!("`{named}` is not a piped stage"));
    // `where (.active)` -> `(.active)`
    match rest.split_once(' ') {
        Some((_, a)) => a,
        None => panic!("`{named}` has no argument"),
    }
}

/// `<>` is the combining operator, not a second spelling of `++`.
///
/// On `string` the two agree, because concatenation is the only thing SQL can
/// do. On `maybe string` they differ, and the difference is the point: `<>` is
/// Haskell's `<>`, so `nothing` is its identity — `nothing <> x` is `x` — while
/// `++` is SQL's `||`, where one NULL swallows the other side.
#[test]
fn the_combining_operator_has_its_own_maybe_instance() {
    let t = "t : query { a = string, b = string, na = maybe string, nb = maybe string } = \
             table \"s\" \"t\"\n";
    assert_eq!(
        sql(&format!("{t}q = t & select {{ c = .a <> .b }}\n"), "q"),
        "SELECT a || b AS c FROM s.t"
    );
    assert_eq!(
        sql(&format!("{t}q = t & select {{ c = .na <> .nb }}\n"), "q"),
        "SELECT COALESCE(na || nb, na, nb) AS c FROM s.t"
    );
    // `++` stays what it says: string concatenation, NULL-propagating.
    assert_eq!(
        sql(&format!("{t}q = t & select {{ c = .a ++ .b }}\n"), "q"),
        "SELECT a || b AS c FROM s.t"
    );
    // Right-associative like the rest of the group: `a <> b <> c` is
    // `a <> (b <> c)`, which the SQL shows by grouping on the right.
    assert_eq!(
        sql(
            &format!("{t}q = t & select {{ c = .a <> .b <> \"!\" }}\n"),
            "q"
        ),
        "SELECT a || (b || '!') AS c FROM s.t"
    );
}

/// `unnest` is a LATERAL join, and a *subquery* rather than a table function.
///
/// PostgreSQL and DuckDB disagree about what a bare `UNNEST(x) AS t2` names its
/// only column (PostgreSQL calls it `t2`, DuckDB `unnest`), so the lateral wraps
/// a projection that names it once. Verified on both engines.
#[test]
fn unnest_is_a_lateral_join() {
    let src = concat!(
        "t : query { k = int, xs = list int } = table \"s\" \"t\"\n",
        "a = t & unnest .xs\n",
        "b = t & unnest .xs & where (.value > 1) & select { k = .k, v = .value }\n",
        "c = t & agg { k = group .k, ids = arrayAgg .k } & unnest .ids\n",
    );
    // An identity projection needs no derived table: the lateral reads the
    // table's own column.
    assert_eq!(
        dialect(src, "a", "postgres").unwrap(),
        "SELECT k, xs, t1.value FROM s.t \
         CROSS JOIN LATERAL (SELECT unnest AS value FROM UNNEST(xs)) AS t1"
    );
    assert_eq!(
        dialect(src, "b", "duckdb").unwrap(),
        "SELECT k, t1.value AS v FROM s.t \
         CROSS JOIN LATERAL (SELECT unnest AS value FROM UNNEST(xs)) AS t1 \
         WHERE (t1.value > 1)"
    );
    // An aggregate's list column unnests too, which is the round trip.
    assert_eq!(
        dialect(src, "c", "postgres").unwrap(),
        "SELECT t1.k, t1.ids, t2.value FROM (SELECT k, ARRAY_AGG(k) AS ids FROM s.t \
         GROUP BY k) AS t1 \
         CROSS JOIN LATERAL (SELECT unnest AS value FROM UNNEST(t1.ids)) AS t2"
    );
    // The type: the element joins the input row under `value`.
    let full = format!("{USERS}{src}");
    let ws = Workspace::from_source(&full);
    let q = root_queries(&ws)
        .into_iter()
        .find(|(n, _)| n == "a")
        .expect("`a`")
        .1
        .expect("`a` checks");
    let _ = q;
    for d in ["sqlite", "mysql"] {
        let e = dialect(src, "a", d).unwrap_err();
        assert!(e.contains("`unnest`"), "{d}: {e}");
    }
}

/// A list can be measured and searched, and each dialect spells that its own
/// way.
///
/// The literal membership operator is `in`; these take a list *value*, which is
/// what a column or an `arrayAgg` result is.
#[test]
fn a_list_can_be_measured_and_searched() {
    let src = concat!(
        "t : query { k = int, xs = list int } = table \"s\" \"t\"\n",
        "a = t & select { k = .k, n = listLength .xs }\n",
        "b = t & select { k = .k, has = listContains .xs 3 }\n",
        "c = t & agg { k = group .k, xs = arrayAgg .k } ",
        "& select { k = .k, n = listLength .xs, has = listContains .xs 2 }\n",
    );
    // PostgreSQL: `cardinality` and `x = ANY(list)`.
    assert_eq!(
        dialect(src, "a", "postgres").unwrap(),
        "SELECT k, cardinality(xs) AS n FROM s.t"
    );
    assert_eq!(
        dialect(src, "b", "postgres").unwrap(),
        "SELECT k, (3 = ANY(xs)) AS has FROM s.t"
    );
    assert_eq!(
        dialect(src, "c", "postgres").unwrap(),
        "SELECT k, cardinality(ARRAY_AGG(k)) AS n, (2 = ANY(ARRAY_AGG(k))) AS has \
         FROM s.t GROUP BY k"
    );
    // DuckDB: `len` (the generator writes `LENGTH`, which DuckDB accepts for a
    // list) and `list_contains`.
    assert_eq!(
        dialect(src, "a", "duckdb").unwrap(),
        "SELECT k, LENGTH(xs) AS n FROM s.t"
    );
    assert_eq!(
        dialect(src, "b", "duckdb").unwrap(),
        "SELECT k, list_contains(xs, 3) AS has FROM s.t"
    );
    // A dialect with no array type refuses, and names the operation.
    for d in ["sqlite", "mysql", "tsql"] {
        let e = dialect(src, "a", d).unwrap_err();
        assert!(e.contains("`listLength`"), "{d}: {e}");
        assert!(e.contains("PostgreSQL and DuckDB"), "{d}: {e}");
        let e = dialect(src, "b", d).unwrap_err();
        assert!(e.contains("`listContains`"), "{d}: {e}");
    }
}

/// `list` is a value type: an aggregate builds one, a literal is one, and `<>`
/// combines two — each spelled by the dialect that can hold it.
///
/// PostgreSQL and DuckDB have arrays; the dialects that do not refuse the query
/// at compile time, because a fallback would change the type silently.
#[test]
fn a_list_value_is_a_column_an_aggregate_and_a_literal() {
    let src = "t : query { id = int, n = int } = table \"s\" \"t\"\n\
               a = t & agg { k = group .id, xs = arrayAgg .n }\n\
               b = t & select { xs = [1, 2, 3] }\n\
               c = t & agg { k = group .id, xs = arrayAgg .n } \
                   & select { k = .k, twice = .xs <> .xs }\n";
    // PostgreSQL: `ARRAY[…]`, `ARRAY_AGG`, and `||` for concatenation.
    assert_eq!(
        dialect(src, "a", "postgres").unwrap(),
        "SELECT id AS k, ARRAY_AGG(n) AS xs FROM s.t GROUP BY id"
    );
    assert_eq!(
        dialect(src, "b", "postgres").unwrap(),
        "SELECT ARRAY[1, 2, 3] AS xs FROM s.t"
    );
    assert_eq!(
        dialect(src, "c", "postgres").unwrap(),
        "SELECT id AS k, (ARRAY_AGG(n) || ARRAY_AGG(n)) AS twice FROM s.t GROUP BY id"
    );
    // DuckDB: `list_value`, the `list` aggregate, and `list_concat`.
    assert_eq!(
        dialect(src, "a", "duckdb").unwrap(),
        "SELECT id AS k, list(n) AS xs FROM s.t GROUP BY id"
    );
    assert_eq!(
        dialect(src, "b", "duckdb").unwrap(),
        "SELECT list_value(1, 2, 3) AS xs FROM s.t"
    );
    assert_eq!(
        dialect(src, "c", "duckdb").unwrap(),
        "SELECT id AS k, list_concat(list(n), list(n)) AS twice FROM s.t GROUP BY id"
    );
    // And a dialect with no array type says so, naming what needs one.
    for d in ["sqlite", "mysql", "tsql"] {
        let e = dialect(src, "a", d).unwrap_err();
        assert!(e.contains("needs a dialect with an array type"), "{d}: {e}");
        assert!(e.contains("PostgreSQL and DuckDB"), "{d}: {e}");
    }
    let e = dialect(src, "b", "sqlite").unwrap_err();
    assert!(e.contains("a list literal"), "{e}");
}
