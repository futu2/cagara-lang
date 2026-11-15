//! Differential tests: the same Cagara queries, compiled per dialect and run
//! on real engines, must return the same rows.
//!
//! Each engine runs through a shell and is skipped when that shell is not
//! installed, unless `CAGARA_REQUIRE_ENGINES` is set (as in CI):
//!
//! | engine | shell | notes |
//! |---|---|---|
//! | SQLite 3.46+ | `sqlite3` | `nix develop` provides it |
//! | DuckDB | `duckdb` | `nix develop` provides it |
//! | PostgreSQL | `node` + `tests/pglite.mjs` | PGlite: Postgres compiled to WASM. `npm install --no-save @electric-sql/pglite` in the repository root |
//!
//! Postgres is the only one of the three that can hold a `list`, so it is where
//! `ARRAY[…]`, `ARRAY_AGG` and `||` are checked by a real planner.

use cagara_hir::{root_queries, Workspace};
use cagara_sql::{compile, Dialect, Options};
use std::io::Write;
use std::process::{Command, Stdio};

/// One statement per line (the DuckDB shell stand-in splits on `;\n`).
const SETUP: &str = "\
CREATE TABLE main.nums (id INTEGER, a INTEGER, b INTEGER, v INTEGER);
INSERT INTO main.nums VALUES (1, 7, 2, 10), (2, -7, 2, NULL), (3, 7, -2, 30), (4, -8, 3, 20);
CREATE TABLE main.users (id INTEGER, name TEXT);
INSERT INTO main.users VALUES (1, 'ann'), (2, 'bob'), (3, 'it''s a\\b ');
CREATE TABLE main.orders (id INTEGER, user_id INTEGER, amount DOUBLE);
INSERT INTO main.orders VALUES (10, 1, 5.5), (11, 1, 4.5), (12, 3, 1.0);
CREATE TABLE main.days (id INTEGER, d DATE);
INSERT INTO main.days VALUES (1, '2024-01-31'), (2, '2024-01-07'), (3, '2023-12-25');
CREATE TABLE main.\"select\" (id INTEGER, \"order\" INTEGER, \"userId\" INTEGER);
INSERT INTO main.\"select\" VALUES (1, 5, 9);
CREATE TABLE main.empty (id INTEGER);
CREATE TABLE main.dupes (id INTEGER, k TEXT);
INSERT INTO main.dupes VALUES (1, 'x'), (1, 'x'), (2, 'x'), (3, 'y');
CREATE TABLE main.g (a INTEGER, b INTEGER, n INTEGER);
INSERT INTO main.g VALUES (1, 1, 1), (1, 2, 2), (2, 1, 3);
";

const TABLES: &str = "\
nums : query { id = int, a = int, b = int, v = maybe int } = table \"main\" \"nums\"
users : query { id = int, name = string } = table \"main\" \"users\"
orders : query { id = int, user_id = int, amount = float } = table \"main\" \"orders\"
days : query { id = int, d = date } = table \"main\" \"days\"
kw : query { id = int, order = int, userId = int } = table \"main\" \"select\"
empty : query { id = int } = table \"main\" \"empty\"
dupes : query { id = int, k = string } = table \"main\" \"dupes\"
g : query { a = int, b = int, n = int } = table \"main\" \"g\"
";

/// `(definition, expected rows)`; a row is its columns joined by `|`, NULL
/// as `NULL`, booleans as `1` / `0`.
const CASES: &[(&str, &[&str])] = &[
    // Integer division truncates toward zero; `%` keeps the dividend's sign.
    (
        "div = nums & select { id = .id, d = .a / .b, m = .a % .b } & order [asc .id]",
        &["1|3|1", "2|-3|-1", "3|-3|1", "4|-2|-2"],
    ),
    // `distinct` dedupes the input row: a stage after it sees the deduped
    // rows, and a projection before it defines the row being deduped.
    ("d1 = dupes & distinct & agg { n = count }", &["3"]),
    (
        "d2 = dupes & distinct & select {.k} & order [asc .k]",
        &["x", "x", "y"],
    ),
    (
        "d3 = dupes & select { k = .k } & distinct & order [asc .k]",
        &["x", "y"],
    ),
    // NULLs sort last in both directions.
    (
        "nasc = nums & select { id = .id, v = .v } & order [asc (coalesce 2147483647 .v)]",
        &["1|10", "4|20", "3|30", "2|NULL"],
    ),
    (
        "ndesc = nums & select { id = .id, v = .v } & order [desc (coalesce 0 .v)]",
        &["3|30", "4|20", "1|10", "2|NULL"],
    ),
    (
        "win = nums & select { id = .id, r = rowNumber { order = [asc (coalesce 2147483647 .v)] } } & order [asc .id]",
        &["1|1", "2|4", "3|3", "4|2"],
    ),
    // The order survives the derived table that the filter on a window
    // needs, even though its key is no longer a column.
    (
        "kept = nums & order [desc (coalesce 0 .v)] & select { id = .id, rn = rowNumber { order = [asc .id] } } & where (.rn <= 3)",
        &["3|3", "1|1", "2|2"],
    ),
    (
        "paged = nums & order [asc .id] & limit 3 & where (.id > 1) & select { id = .id }",
        &["2", "3"],
    ),
    // A computed column of a left join's right side is NULL when unmatched.
    (
        "lj = users & leftJoin (orders & select { user_id = .user_id, one = 1 }) (.<id == .>user_id) & select { id = .id, m = isNull .one } & order [asc .id]",
        &["1|0", "1|0", "2|1", "3|0"],
    ),
    // Nullable columns from the missing side are handled explicitly before
    // aggregation; the match count excludes the synthetic unmatched row.
    (
        "ljagg = users & leftJoin (orders & select { user_id = .user_id, amount = .amount }) (.<id == .>user_id) & agg { id = group .id, total = coalesce 0.0 (sum (coalesce 0.0 .amount)), n = coalesce 0 (sum (ifThenElse (isNotNull .user_id) 1 0)) } & order [asc .id]",
        &["1|10.0|2", "2|0.0|0", "3|1.0|1"],
    ),
    // `filtered p count` is the count, and replaces the indicator sum: the synthetic unmatched row
    // fails the predicate, so the count is 0 rather than NULL.
    (
        "ljcount = users & leftJoin (orders & select { user_id = .user_id, amount = .amount }) (.<id == .>user_id) & agg { id = group .id, n = filtered (isNotNull .user_id) count } & order [asc .id]",
        &["1|2", "2|0", "3|1"],
    ),
    // A constant group key groups everything, and nothing from nothing.
    (
        "cg = nums & select { c = 2, x = .id } & agg { c = group .c, n = count }",
        &["2|4"],
    ),
    (
        "cg0 = empty & select { c = 2, x = .id } & agg { c = group .c, n = count }",
        &[],
    ),
    // A filter over an aggregate wraps the grouped `SELECT`, still over the
    // same group; both engines must return the filtered groups.
    (
        "hav = orders & agg { u = group .user_id, total = sum .amount, n = count } & where (.n >= 2) & order [asc .u]",
        &["1|10.0|2"],
    ),
    (
        "havconst = nums & agg { n = count } & where (.n > 0)",
        &["4"],
    ),
    // Strings: trailing spaces count, quotes and backslashes survive.
    (
        "str = users & select { id = .id, l = length .name, r = right 2 .name, p = strpos \"b\" .name, e = endsWith \" \" .name } & order [asc .id]",
        &["1|3|nn|0|0", "2|3|ob|1|0", "3|9|b |8|1"],
    ),
    (
        "esc = users & where (.name == \"it's a\\\\b \") & select { id = .id }",
        &["3"],
    ),
    // Dates: 0 = Sunday; month ends clamp.
    (
        "dt = days & select { id = .id, w = dayOfWeek .d, m = addMonths 1 .d, n = daysBetween .d \"2024-03-01\" } & order [asc .id]",
        &["1|3|2024-02-29|30", "2|0|2024-02-07|54", "3|1|2024-01-25|67"],
    ),
    // Reserved and mixed-case names are quoted.
    (
        "kws = kw & select { order = .order, userId = .userId, id = .id }",
        &["5|9|1"],
    ),
    // A join input is a relation, so a filter on it decides which rows that
    // side contributes, and it must stay in the side's derived table. These
    // cases pin the two ways hoisting goes wrong.
    //
    // The right input of a right join is the *preserved* side. Hoisting its
    // filter into ON would widen the input back to the unfiltered relation,
    // resurrecting user 1 as an unmatched row; here the filter keeps users 2
    // and 3, neither of which has an order, so both survive with NULL order
    // columns and user 1 must not appear.
    (
        "rj = orders & rightJoin ((users & where (.id > 1)) & select { u_id = .id, name = .name }) (.<user_id == .>u_id) & select { oid = .id, amount = .amount, n = .name } & order [asc .n, asc (coalesce 2147483647 .oid)]",
        &["NULL|NULL|bob", "12|1.0|it's a\\b "],
    ),
    // Hoisting a filter to WHERE would drop the null-extended rows. For a
    // right join the *left* input is null-extended, so filtering it must
    // still leave every right row: users 2 and 3 have no matching order and
    // must survive as NULL.
    (
        "rjonleft = (orders & where (.amount > 4.5)) & rightJoin (users & select { u_id = .id, name = .name }) (.<user_id == .>u_id) & select { oid = .id, amount = .amount, n = .name } & order [asc .n, asc (coalesce 2147483647 .oid)]",
        &[
            "10|5.5|ann",
            "NULL|NULL|bob",
            "NULL|NULL|it's a\\b ",
        ],
    ),
    // A full join preserves both sides, so a filter on either input keeps a
    // derived table.
    (
        "fj = orders & fullJoin ((users & where (.id > 2)) & select { u_id = .id, name = .name }) (.<user_id == .>u_id) & select { oid = .id, amount = .amount, n = .name } & order [asc (coalesce \"~\" .n), asc (coalesce 2147483647 .oid)]",
        &[
            "12|1.0|it's a\\b ",
            "10|5.5|NULL",
            "11|4.5|NULL",
        ],
    ),
    (
        "fj2 = (orders & where (.amount > 4.5)) & fullJoin ((users & where (.id > 2)) & select { u_id = .id, name = .name }) (.<user_id == .>u_id) & select { oid = .id, amount = .amount, n = .name } & order [asc (coalesce \"~\" .n), asc (coalesce 2147483647 .oid)]",
        &["NULL|NULL|it's a\\b ", "10|5.5|NULL"],
    ),
    // The mirror case on the other side: a left join filtered on its
    // preserved (left) input, and one filtered on its null-extended input.
    (
        "ljonleft = (users & where (.id > 2)) & leftJoin (orders & select { user_id = .user_id, amount = .amount }) (.<id == .>user_id) & select { n = .name, amount = .amount } & order [asc .n]",
        &["it's a\\b |1.0"],
    ),
    (
        "ljonright = users & leftJoin ((orders & where (.amount > 4.5)) & select { user_id = .user_id, amount = .amount }) (.<id == .>user_id) & select { n = .name, amount = .amount } & order [asc .n, asc (coalesce 2147483647 .amount)]",
        &["ann|5.5", "bob|NULL", "it's a\\b |NULL"],
    ),
    // An inner join is the exception: both sides may hoist their filters to
    // WHERE, since the filter only decides which rows match. No order is
    // placed by a user above 1, so this returns nothing.
    (
        "ij = (orders & where (.amount > 4.5)) & innerJoin ((users & where (.id > 1)) & select { u_id = .id, name = .name }) (.<user_id == .>u_id) & select { oid = .id, n = .name } & order [asc .n]",
        &[],
    ),
    // The same filters without excluding the matching user, so the inner
    // join does return rows.
    (
        "ij2 = (orders & where (.amount > 4.5)) & innerJoin ((users & where (.id > 0)) & select { u_id = .id, name = .name }) (.<user_id == .>u_id) & select { oid = .id, n = .name } & order [asc .oid]",
        &["10|ann"],
    ),
    // A lone `offset` needs a "no limit" that the engine accepts: SQLite
    // rejects a bare OFFSET and the MySQL max-BIGINT spelling (`datatype
    // mismatch`), so it needs `LIMIT -1`.
    (
        "offsolo = nums & order [asc .id] & offset 2 & select { id = .id }",
        &["3", "4"],
    ),
    // A limit and an offset together, which every engine spells the same way.
    (
        "offlim = nums & order [asc .id] & offset 1 & limit 2 & select { id = .id }",
        &["2", "3"],
    ),
    // A set-operation branch with its own LIMIT: the set operator's own tail
    // would otherwise swallow it (`SELECT ... LIMIT 3 UNION SELECT ... LIMIT
    // 2` is a parse error on both engines), so the branch is wrapped. A set
    // operation does not define a row order, so the result is sorted outside
    // it rather than relying on the engine's union order.
    (
        "setlim = (union (nums & select { id = .id } & order [asc .id] & limit 2) (nums & select { id = .id } & order [desc .id] & limit 2)) & order [asc .id]",
        &["1", "2", "3", "4"],
    ),
    // Key stages: `omit` drops one column and `prefix`/`suffix` rewrite names.
    // Both are pure row operations on the query's columns, so every engine must
    // agree with every other one about the resulting names and values.
    (
        "omit_b = nums & omit \"b\" & select {.id, .a} & order [asc .id]",
        &["1|7", "2|-7", "3|7", "4|-8"],
    ),
    (
        "mk_prefix = nums & prefix \"n_\" & select {.n_id, .n_a} & order [asc .n_id]",
        &["1|7", "2|-7", "3|7", "4|-8"],
    ),
    (
        "mk_suffix = nums & suffix \"_v2\" & select {.id_v2, .a_v2} & order [asc .id_v2]",
        &["1|7", "2|-7", "3|7", "4|-8"],
    ),
    (
        "mk_rename = nums & prefix \"n_\" & suffix \"_v2\" & select {.n_id_v2, .n_a_v2} & order [asc .n_id_v2]",
        &["1|7", "2|-7", "3|7", "4|-8"],
    ),
    // A `case` is one searched `CASE`: the first true condition wins, and the
    // trailing `true =>` arm is the `ELSE`. Rows 2 and 4 have `a` negative.
    (
        "case_bucket = nums & select { id = .id, k = case { .a > 0 => \"pos\"; .a < 0 => \"neg\"; true => \"zero\" } } & order [asc .id]",
        &["1|pos", "2|neg", "3|pos", "4|neg"],
    ),
    // A `case` in an `agg` stage: its arms are aggregates, so the whole
    // expression is one. `sum .a` over these rows is 7-7+7-8 = -1, and it is
    // only reached because `count > 3` holds for the single group.
    (
        "case_agg = nums & agg { n = count, k = case { count > 3 => coalesce 0 (sum .a); true => 0 } }",
        &["4|-1"],
    ),
    // Grouping sets: the subtotal rows carry NULL in the key they leave out.
    // `engines_agree` compares rows in order, so each case sorts; a `rollup` key
    // is NULL on its subtotal, and the emulated NULLS LAST puts those rows last
    // on every target.
    (
        "rollup1 = g & agg { a = rollup .a, s = sum .n } & select { a = .a, s = .s } & order [asc .a]",
        &["1|3", "2|3", "NULL|6"],
    ),
    (
        "rollup2 = g & agg { a = rollup .a, b = rollup .b, s = sum .n } & select { a = .a, b = .b, s = .s } & order [asc .a, asc .b]",
        &["1|1|1", "1|2|2", "1|NULL|3", "2|1|3", "2|NULL|3", "NULL|NULL|6"],
    ),
    // Every subset, so the `b` subtotals appear too.
    (
        "cube2 = g & agg { a = cube .a, b = cube .b, s = sum .n } & select { a = .a, b = .b, s = .s } & order [asc .a, asc .b]",
        &[
            "1|1|1", "1|2|2", "1|NULL|3", "2|1|3", "2|NULL|3", "NULL|1|4", "NULL|2|2",
            "NULL|NULL|6",
        ],
    ),
    // A plain key beside a rolled-up one groups within the subtotals.
    (
        "rollup_mixed = g & agg { a = group .a, b = rollup .b, s = sum .n } & select { a = .a, b = .b ?? -1, s = .s } & order [asc .a, asc .b]",
        &["1|-1|3", "1|1|1", "1|2|2", "2|-1|3", "2|1|3"],
    ),
    // Filtered aggregates: the predicate filters each aggregate's input, so
    // the `neg` group has no matching rows. Its value aggregates are NULL
    // while its counts are 0, exactly as `FILTER (WHERE p)` behaves.
    (
        "fagg = nums & select { k = ifThenElse (.a > 0) \"pos\" \"neg\", a = .a } & agg {
           k = group .k,
           total = filtered (.a > 0) (sum .a),
           mean = filtered (.a > 0) (avg .a),
           n = filtered (.a > 0) count,
           n_of = filtered (.a > 0) (countOf .a),
           n_distinct = filtered (.a > 0) (distinctAgg (countOf .a)),
           lo = filtered (.a > 0) (min .a),
           hi = filtered (.a > 0) (max .a)
         } & order [asc .k]",
        &[
            "neg|NULL|NULL|0|0|0|NULL|NULL",
            "pos|14|7.0|2|2|1|7|7",
        ],
    ),
    // More shapes with several barriers in one pipeline.
    (
        "bar_join_offset = (orders & order [asc .id] & offset 1) & innerJoin (users & select { u_id = .id, name = .name }) (.<user_id == .>u_id) & select { oid = .id, n = .name } & order [asc .oid]",
        &["11|ann", "12|it's a\\b "],
    ),
    // The preserved side of a right join is filtered *and* limited: both users
    // without a matching order must still survive.
    (
        "bar_right_limit = (orders & order [asc .id] & limit 1) & rightJoin (users & select { u_id = .id, name = .name }) (.<user_id == .>u_id) & select { oid = .id, n = .name } & order [asc .n, asc (coalesce 2147483647 .oid)]",
        &["10|ann", "NULL|bob", "NULL|it's a\\b "],
    ),
    // An aggregate over a `distinct`, which is a barrier in both directions.
    (
        "bar_agg_distinct_agg = nums & agg { b = group .b, n = count } & distinct & agg { k = count }",
        &["3"],
    ),
    // A filter on a window, then an aggregate: the window is computed first.
    (
        "bar_win_where_agg = nums & select { id = .id, r = rowNumber { order = [asc .id] } } & where (.r <= 2) & agg { n = count }",
        &["2"],
    ),
    // A limit, an aggregate, and a limit again.
    (
        "bar_limit_agg_limit = nums & order [asc .id] & limit 3 & agg { n = count } & limit 1",
        &["3"],
    ),
    // An `omit` between two projections, so the second cannot read the dropped
    // column and the stage has to be wrapped.
    (
        "bar_omit_between = nums & select { id = .id, a = .a, b = .b } & omit \"b\" & select { id = .id, a = .a } & order [asc .id]",
        &["1|7", "2|-7", "3|7", "4|-8"],
    ),
    // Barriers *inside* a join input: the input is a relation, so its limit,
    // its dedupe and its window all decide which rows that side contributes.
    (
        "bar_join_limit = (orders & order [asc .amount] & limit 2) & innerJoin (users & select { u_id = .id, name = .name }) (.<user_id == .>u_id) & select { oid = .id, amount = .amount, n = .name } & order [asc .oid]",
        &["11|4.5|ann", "12|1.0|it's a\\b "],
    ),
    (
        "bar_join_distinct = (dupes & select { id = .id } & distinct) & innerJoin (nums & select { nid = .id }) (.<id == .>nid) & select { id = .id } & order [asc .id]",
        &["1", "2", "3"],
    ),
    (
        "bar_join_agg = (nums & agg { b = group .b, n = count }) & innerJoin (nums & select { bb = .b, id = .id }) (.<b == .>bb) & select { b = .b, n = .n, id = .id } & order [asc .b, asc .id]",
        &["-2|1|3", "2|2|1", "2|2|2", "3|1|4"],
    ),
    (
        "bar_join_window = (nums & select { id = .id, r = rowNumber { order = [asc .id] } }) & innerJoin (nums & select { nid = .id }) (.<id == .>nid) & select { id = .id, r = .r } & order [asc .id]",
        &["1|1", "2|2", "3|3", "4|4"],
    ),
    // A chain of two joins, so the second one joins a derived table to a
    // derived table.
    (
        "bar_join_chain = orders & innerJoin (users & select { u_id = .id, name = .name }) (.<user_id == .>u_id) & innerJoin (nums & select { nn = .id, b = .b }) (.<user_id == .>nn) & select { oid = .id, n = .name, b = .b } & order [asc .oid]",
        &["10|ann|2", "11|ann|2", "12|it's a\\b |-2"],
    ),
    (
        "bar_leftjoin_window = users & leftJoin (nums & select { nid = .id, b = .b, r = rowNumber { order = [asc .id] } }) (.<id == .>nid) & select { id = .id, r = .r } & order [asc .id]",
        &["1|1", "2|2", "3|3"],
    ),
    // A set operation of two aggregates: both branches produce the same row, so
    // `union` leaves one.
    (
        "bar_union_agg = (union (nums & agg { n = count }) (nums & agg { n = count })) & order [asc .n]",
        &["4"],
    ),
    // A filter after a `distinct`, and a limit between them.
    (
        "bar_where_distinct = dupes & select {.k} & distinct & where (.k == \"x\")",
        &["x"],
    ),
    (
        "bar_dist_limit_proj = dupes & select {.k} & distinct & order [asc .k] & limit 1 & select { k = .k }",
        &["x"],
    ),
    // A filter after a limit that already ran over an aggregate.
    (
        "bar_agg_limit_where = nums & agg { b = group .b, n = count } & order [asc .b] & limit 2 & where (.n > 1)",
        &["2|2"],
    ),
    // A null-extended join side inlines a computed column only when the value
    // stays NULL for an unmatched row. `amount * 2` does; a literal does not, and
    // inlining it would print a number where the join promises NULL.
    (
        "rw_null_ext_prop = users & leftJoin (orders & select { user_id = .user_id, dbl = .amount * 2 }) (.<id == .>user_id) & select { id = .id, dbl = .dbl } & order [asc .id, asc .dbl]",
        &["1|9.0", "1|11.0", "2|NULL", "3|2.0"],
    ),
    (
        "rw_null_ext_lit = users & leftJoin (orders & select { user_id = .user_id, one = 1 }) (.<id == .>user_id) & select { id = .id, one = .one } & order [asc .id]",
        &["1|1", "1|1", "2|NULL", "3|1"],
    ),
    // A function call is not assumed to keep NULLs, so it is computed before the
    // join and is NULL for an unmatched row all the same.
    (
        "rw_null_ext_fn = users & leftJoin (orders & select { user_id = .user_id, s = toString .amount }) (.<id == .>user_id) & select { id = .id, s = .s } & order [asc .id, asc .s]",
        &["1|4.5", "1|5.5", "2|NULL", "3|1.0"],
    ),
    // Every NULL-skipping aggregate takes a nullable column, and counts the
    // non-null values: `nums.v` is 10, NULL, 30, 20.
    (
        "null_aggs = nums & agg { s = sum .v, a = avg .v, lo = min .v, hi = max .v, n = countOf .v, nd = countDistinct .v }",
        &["60|20.0|10|30|3|3"],
    ),
    // A filter over a window folds into `QUALIFY` on DuckDB and into a derived
    // table on PostgreSQL. Same query, same rows — which is the whole claim.
    (
        "win_filter = nums & select { id = .id, r = rowNumber { order = [asc .id] } } & where (.r > 2) & select {.id} & order [asc .id]",
        &["3", "4"],
    ),
    // The paging order matters and must not fold: `limit 2 & where` takes two
    // rows and filters them, where `QUALIFY … LIMIT 2` filters the relation.
    (
        "win_filter_paged = nums & select { id = .id, r = rowNumber { order = [asc .id] } } & limit 2 & where (.r > 1) & select {.id} & order [asc .id]",
        &["2"],
    ),
    (
        "win_filter_then_paged = nums & select { id = .id, r = rowNumber { order = [asc .id] } } & where (.r > 1) & limit 2 & select {.id} & order [asc .id]",
        &["2", "3"],
    ),
    // A filter after a bare `distinct` folds into it, which is only right
    // because deduping whole rows and filtering on them commute.
    (
        "rw_distinct_where = dupes & select {.k} & distinct & where (.k != \"x\")",
        &["y"],
    ),
    // A renamed column must not leave a dangling sort key: `prefix` renames the
    // row, and a later `select` renames it again, so the name the `order` was
    // written with is not a column of the printed `FROM`.
    (
        "rw_renamed_key = nums & prefix \"x_\" & order [asc .x_id] & limit 2 & select { a = .x_id }",
        &["1", "2"],
    ),
    (
        "rw_renamed_key2 = nums & suffix \"_v\" & order [desc .id_v] & limit 2 & select { a = .id_v }",
        &["4", "3"],
    ),
    // A carried-over sort key must not be captured by the projection above it.
    // `nums.a` is 7, -7, 7, -8, so `order [asc .a] & limit 2` keeps ids 4 and 2;
    // before the fix the outer `ORDER BY a` bound to the *output* `a` (which the
    // projection set to `id`) and returned them as 2, 4.
    (
        "rw_carried_key = nums & order [asc .a] & limit 2 & select { a = .id, id = .a }",
        &["4|-8", "2|-7"],
    ),
    (
        "rw_carried_key_dropped = nums & order [asc .a] & limit 2 & select { a = .id }",
        &["4", "2"],
    ),
    // A projection that renames and reorders without dropping anything folds
    // into the stage below it, so it must not change the rows: the limit and the
    // sort key are the same ones, and only the names move.
    (
        "rw_perm_limit = nums & order [asc .id] & limit 2 & select { b = .b, id = .id, a = .a, v = .v }",
        &["2|1|7|10", "2|2|-7|NULL"],
    ),
    (
        "rw_perm_distinct = dupes & distinct & select { k = .k, id = .id } & order [asc .id, asc .k]",
        &["x|1", "x|2", "y|3"],
    ),
    // Dropping a column across *paging* folds, so it must not move the rows
    // either: the sort key is a base column, which SQL lets an `ORDER BY` name
    // even when the projection no longer outputs it.
    (
        "rw_page_drop = nums & order [asc .id] & limit 2 & select { a = .a }",
        &["7", "-7"],
    ),
    (
        "rw_page_drop_desc = nums & order [desc .id] & limit 2 & select { a = .a }",
        &["-8", "7"],
    ),
    // Sorting by a column the projection drops: the order is still the input's.
    (
        "rw_page_sort_dropped = nums & order [asc .a] & limit 2 & select { id = .id }",
        &["4", "2"],
    ),
    // A projection that reuses a sort key's name for a different column changes
    // what `ORDER BY` means, so it keeps its derived table.
    // The rows the barrier produces, in the order the limit chose: ids 4 and 2
    // (the two smallest `a`), then swapped names. The earlier expectation here
    // was `2|-7, 4|-8` — recorded from the output rather than reasoned from the
    // query — and it was the bug: the outer `ORDER BY a` bound to the output `a`.
    (
        "rw_page_shadow = nums & order [asc .a] & limit 2 & select { a = .id, id = .a }",
        &["4|-8", "2|-7"],
    ),
    // Dropping a column is not a permutation and keeps its derived table, so it
    // still has to return the rows the drop implies.
    (
        "rw_perm_dropped = dupes & distinct & select { id = .id } & order [asc .id]",
        &["1", "2", "3"],
    ),
    // A semi/anti join is an existence test, so inlining its inputs must not
    // change which rows exist: the left keeps every row that has (or has not) a
    // match, with its own filters applied, and the right's filter decides what
    // counts as a match.
    (
        "rw_semi = nums & semiJoin (nums & select { nid = .id }) (.<id == .>nid) & select {.id} & order [asc .id]",
        &["1", "2", "3", "4"],
    ),
    (
        "rw_anti = nums & antiJoin (nums & where (.a > 0) & select { nid = .id }) (.<id == .>nid) & select {.id} & order [asc .id]",
        &["2", "4"],
    ),
    (
        "rw_anti_filtered_left = (nums & where (.a > 0)) & antiJoin (nums & where (.b > 2) & select { nid = .id }) (.<id == .>nid) & select {.id} & order [asc .id]",
        &["1", "3"],
    ),
    // The right side needs its own derived table when it pages: only the first
    // two ids count as matches.
    (
        "rw_semi_paged = nums & semiJoin (nums & order [asc .id] & limit 2 & select { nid = .id }) (.<id == .>nid) & select {.id} & order [asc .id]",
        &["1", "2"],
    ),
    // An identity projection is dropped, so it must not change the rows when it
    // sits between a barrier and a later stage — the case where the lowerer
    // would otherwise have to wrap.
    (
        "rw_ident_limit = nums & order [asc .id] & limit 2 & select { id = .id, a = .a, b = .b, v = .v }",
        &["1|7|2|10", "2|-7|2|NULL"],
    ),
    (
        "rw_ident_distinct = dupes & select { id = .id, k = .k } & distinct & order [asc .id, asc .k]",
        &["1|x", "2|x", "3|y"],
    ),
    // A projection that renames is *not* the identity, and a later stage reads
    // the new name: dropping it would break the query rather than the plan.
    (
        "rw_ident_renamed = nums & select { x = .id, a = .a } & order [asc .x] & select { x = .x }",
        &["1", "2", "3", "4"],
    ),
    // A projection that swaps two columns under their own names is not the
    // identity: the values move. It is also where a sort key resolved to its
    // expression instead of its output name ordered by the wrong column —
    // `ORDER BY a` binds to the *output* `a`, which is the base `id`.
    (
        "rw_ident_swapped = nums & select { id = .a, a = .id } & order [asc .id] & select { id = .id, a = .a }",
        &["-8|4", "-7|2", "7|1", "7|3"],
    ),
    // The `Rel -> Rel` rules, measured on rows. Each of these used to keep a
    // derived table (or, for pagination, return the wrong rows), so a wrong rule
    // here is a wrong answer rather than a slower query.
    //
    // `limit 3 & limit 2` is the first two rows of the first three.
    (
        "rw_limit_limit = nums & order [asc .id] & limit 3 & limit 2 & select {.id}",
        &["1", "2"],
    ),
    // Two offsets add: skipping 1 twice is skipping 2.
    (
        "rw_offset_offset = nums & order [asc .id] & offset 1 & offset 1 & select {.id}",
        &["3", "4"],
    ),
    // `limit 3 & offset 1` keeps rows 2..=3 — three rows, starting at the
    // second — which is `LIMIT 2 OFFSET 1`, not `LIMIT 3 OFFSET 1`.
    (
        "rw_limit_offset = nums & order [asc .id] & limit 3 & offset 1 & select {.id}",
        &["2", "3"],
    ),
    // An offset at or past the limit keeps nothing.
    (
        "rw_limit_offset_empty = nums & order [asc .id] & limit 2 & offset 2 & select {.id}",
        &[],
    ),
    (
        "rw_limit_offset_past = nums & order [asc .id] & limit 2 & offset 9 & select {.id}",
        &[],
    ),
    // The other order is already one clause, and must keep its meaning.
    (
        "rw_offset_limit = nums & order [asc .id] & offset 1 & limit 2 & select {.id}",
        &["2", "3"],
    ),
    // Deduping twice is deduping once.
    (
        "rw_distinct_distinct = dupes & select {.k} & distinct & distinct & order [asc .k]",
        &["x", "y"],
    ),
    // A chain of all three, which the pass has to reach a fixed point on.
    (
        "rw_chain = nums & order [asc .id] & limit 4 & offset 1 & offset 1 & limit 1 & select {.id}",
        &["3"],
    ),
    // The barrier table, one case per row of it. Each of these folds the wrong
    // way unless the stage is wrapped first, and folding changes *which rows*
    // come back rather than producing invalid SQL — so the differential harness
    // is the only thing that can catch it.
    //
    // A projection after a limit sees only the kept rows, not all of them.
    (
        "bar_proj_limit = nums & order [asc .id] & limit 2 & select { id = .id, a = .a }",
        &["1|7", "2|-7"],
    ),
    // An aggregate after a limit counts the kept rows.
    (
        "bar_agg_limit = nums & order [asc .id] & limit 2 & agg { n = count }",
        &["2"],
    ),
    // `distinct` after a limit dedupes the kept rows.
    (
        "bar_dist_limit = nums & order [asc .id] & limit 2 & distinct & select {.id} & order [asc .id]",
        &["1", "2"],
    ),
    // `omit` after a `distinct` keeps the dedupe: the row being deduped is the
    // three-column one, so dropping `k` afterwards leaves three ids.
    (
        "bar_omit_distinct = dupes & distinct & omit \"k\" & order [asc .id]",
        &["1", "2", "3"],
    ),
    // Two limits: the second applies to the first's rows.
    (
        "bar_limit_limit = nums & order [asc .id] & limit 3 & limit 2 & select {.id}",
        &["1", "2"],
    ),
    // Two offsets, likewise.
    (
        "bar_offset_offset = nums & order [asc .id] & offset 1 & offset 1 & select {.id}",
        &["3", "4"],
    ),
    // A projection reading a column the previous one computed.
    (
        "bar_proj_proj = nums & select { x = .a + 1 } & select { y = .x * 2 } & order [asc .y]",
        &["-14", "-12", "16", "16"],
    ),
    // A filter on a computed column: it must run after the projection that
    // names it, not against the base table.
    (
        "bar_where_proj = nums & select { x = .a + 1 } & where (.x > 0) & select { x = .x } & order [asc .x]",
        &["8", "8"],
    ),
    // An aggregate grouped by a projected column.
    (
        "bar_agg_proj = nums & select { k = .b, a = .a } & agg { k = group .k, n = count } & order [asc .k]",
        &["-2|1", "2|2", "3|1"],
    ),
    // A window over an aggregate's result.
    (
        "bar_win_agg = nums & agg { n = count } & select { n = .n, r = rowNumber { order = [asc .n] } }",
        &["4|1"],
    ),
    // `distinct` over an aggregate's rows.
    (
        "bar_dist_agg = nums & agg { b = group .b, n = count } & distinct & order [asc .b]",
        &["-2|1", "2|2", "3|1"],
    ),
    // A sort after a `distinct` is a barrier too: sorting the pre-dedupe rows
    // would order a different multiset.
    (
        "bar_sort_distinct = dupes & select {.k} & distinct & order [desc .k]",
        &["y", "x"],
    ),
    // On an empty input a global filtered aggregate still has one row:
    // `filtered p count` is 0 and a filtered `sum` is NULL.
    (
        "fagg_empty = empty & agg { n = filtered true count, total = filtered true (sum .id) }",
        &["0|NULL"],
    ),
];

struct Engine {
    shell: &'static str,
    args: &'static [&'static str],
    dialect: &'static str,
}

const ENGINES: &[Engine] = &[
    Engine {
        shell: "sqlite3",
        args: &[
            "-batch",
            "-list",
            "-noheader",
            "-nullvalue",
            "NULL",
            ":memory:",
        ],
        dialect: "sqlite",
    },
    Engine {
        shell: "duckdb",
        args: &["-batch", "-list", "-noheader", "-nullvalue", "NULL"],
        dialect: "duckdb",
    },
];

/// Rows of the last statement in `sql`, under this engine's own `setup`.
fn run_setup(e: &Engine, setup: &str, sql: &str) -> Option<Result<Vec<String>, String>> {
    let args: Vec<&std::ffi::OsStr> = e.args.iter().map(std::ffi::OsStr::new).collect();
    run_script(e.shell, &args, setup, sql)
}

/// Rows of the last statement under the shared [`SETUP`], or `None` when the
/// shell is not installed.
fn run(e: &Engine, sql: &str) -> Option<Result<Vec<String>, String>> {
    let args: Vec<&std::ffi::OsStr> = e.args.iter().map(std::ffi::OsStr::new).collect();
    run_script(e.shell, &args, SETUP, sql)
}

/// Rows of the last statement in `sql`, after `setup` has been run. `None`
/// when the shell is not installed.
fn run_script(
    shell: &str,
    args: &[&std::ffi::OsStr],
    setup: &str,
    sql: &str,
) -> Option<Result<Vec<String>, String>> {
    let mut child = match Command::new(shell)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return None,
    };
    let script = format!("{setup}{sql};\n");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(script.as_bytes())
        .expect("write script");
    let out = child.wait_with_output().expect("run shell");
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || stderr.contains("Error") {
        return Some(Err(format!("{stderr}\n{sql}")));
    }
    let rows = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            l.split('|')
                .map(|v| match v {
                    "true" => "1",
                    "false" => "0",
                    v => v,
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    Some(Ok(rows))
}

/// A list compares as a value on the engines that have the type.
///
/// `[1, 2] == [1, 2]` was listed as missing in the plan; it is not — the generic
/// comparison takes `a` on both sides and a `list a` is an `a`, and both engines
/// compare arrays element-wise. Pinned here because the plan said otherwise and a
/// claim like that is worth a measurement either way.
#[test]
fn a_list_compares_as_a_value_on_the_engines() {
    let src = "nums2 : query { id = int } = table \"main\" \"nums\"\n\
               q = nums2 & where (.id == 1) & select { same = [1, 2] == [1, 2], \
               diff = [1, 2] == [2, 1], ne = [1] != [2], ord = [1] < [2] }\n";
    let ws = Workspace::from_source(src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);
    let (_, rel) = rels
        .iter()
        .find(|(n, _)| n == "q")
        .unwrap_or_else(|| panic!("no `q`"));
    let rel = rel
        .as_ref()
        .unwrap_or_else(|d| panic!("`q`: {}", d.message));
    // DuckDB prints a boolean as 1/0 and PostgreSQL as `true`/`false`, so the
    // assertion compares rows the way the shared cases do.
    let want = ["1|0|1|1"];

    // DuckDB, through its shell.
    let duck = Engine {
        shell: "duckdb",
        args: &["-batch", "-list", "-noheader", "-nullvalue", "NULL"],
        dialect: "duckdb",
    };
    let sql = compile(
        rel,
        Options {
            dialect: Dialect::DuckDb,
            ..Options::default()
        },
    )
    .expect("duckdb");
    match run_setup(&duck, SETUP, &sql) {
        Some(Ok(rows)) => assert_eq!(rows, want, "{sql}"),
        Some(Err(m)) => panic!("duckdb: {m}\n  {sql}"),
        None if std::env::var_os("CAGARA_REQUIRE_ENGINES").is_some() => {
            panic!("`duckdb` is not installed (CAGARA_REQUIRE_ENGINES is set)")
        }
        None => eprintln!("skipping duckdb: not installed"),
    }

    // PostgreSQL, through PGlite.
    let harness = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pglite.mjs");
    let ready = Command::new("node")
        .args([harness, "--check"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ready {
        assert!(
            std::env::var_os("CAGARA_REQUIRE_ENGINES").is_none(),
            "`node` with @electric-sql/pglite is not installed (CAGARA_REQUIRE_ENGINES is set)"
        );
        return;
    }
    let pg = compile(
        rel,
        Options {
            dialect: Dialect::Postgres,
            ..Options::default()
        },
    )
    .expect("postgres");
    let blocks = pg_batch(harness, &pg_setup(SETUP), std::slice::from_ref(&pg));
    assert_eq!(blocks[0].len(), want.len(), "{pg}");
    assert!(
        blocks[0].iter().zip(want).all(|(a, b)| same_row(a, b)),
        "{:?} vs {want:?}\n  {pg}",
        blocks[0]
    );
}

/// Every statement is well-formed SQL.
///
/// Only three engines can be started here, so the other seven dialects — ANSI,
/// MySQL, Trino, Spark, BigQuery, Snowflake, T-SQL — have no execution check. The
/// strongest thing available without a server is to read the text back, and this
/// does that for every shared case in every dialect.
///
/// It is a **weaker check than it looks**, and the reason is measured rather than
/// assumed: sqlglot's parser is dialect-*agnostic* for this purpose. It reads
/// `SELECT TOP 3 id` under PostgreSQL and `SELECT \`id\`` under PostgreSQL just as
/// happily as under T-SQL and MySQL, so a statement being readable says it is
/// well-formed SQL, not that the target accepts it. What it does catch is the
/// generator producing malformed text — an unbalanced parenthesis, a stray
/// placeholder, an empty clause — which no differential run can see for a dialect
/// it cannot execute.
///
/// The per-dialect rules that *are* checkable are in
/// [`each_dialect_avoids_what_it_cannot_read`].
#[test]
fn every_statement_is_well_formed_sql() {
    let extra = "\
                ex_concat = users & select { s = .name ++ \"!\" }\n\
                ex_paged = users & order [asc .id] & limit 3 & offset 1 & select {.id}\n\
                ex_window = nums & select { id = .id, r = rowNumber { order = [asc .id] } } & where (.r > 1)\n\
                ex_agg = orders & agg { u = group .user_id, t = sum .amount, n = count } & where (.n > 1)\n\
                ex_join = users & innerJoin (orders & select { user_id = .user_id, amount = .amount }) (.<id == .>user_id) & select { n = .name, a = .amount }\n\
                ex_semi = users & semiJoin orders (.<id == .>user_id) & select {.name}\n\
                ex_set = (union (users & select {.id}) (orders & select { id = .user_id })) & order [asc .id]\n\
                ex_distinct = dupes & select {.k} & distinct & order [asc .k]\n\
                ex_case = nums & select { id = .id, k = case { .a > 0 => \"pos\"; true => \"neg\" } }\n\
                ex_ident = kw & select { order = .order, userId = .userId }\n";
    let src = format!(
        "{TABLES}{}\n{extra}",
        CASES.iter().map(|(d, _)| *d).collect::<Vec<_>>().join("\n")
    );
    let ws = Workspace::from_source(&src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);
    let names: Vec<&str> = CASES
        .iter()
        .map(|(d, _)| d.split_whitespace().next().expect("name"))
        .chain([
            "ex_concat",
            "ex_paged",
            "ex_window",
            "ex_agg",
            "ex_join",
            "ex_semi",
            "ex_set",
            "ex_distinct",
            "ex_case",
            "ex_ident",
        ])
        .collect();

    // Every dialect this backend accepts, not only the three that can run.
    const DIALECTS: &[&str] = &[
        "ansi",
        "postgres",
        "duckdb",
        "sqlite",
        "mysql",
        "trino",
        "spark",
        "bigquery",
        "snowflake",
        "tsql",
    ];
    let mut failures = Vec::new();
    let mut checked = 0;
    for d in DIALECTS {
        let dialect = Dialect::from_str(d).expect("dialect");
        for name in &names {
            let (_, rel) = rels
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("no query `{name}`"));
            let rel = rel
                .as_ref()
                .unwrap_or_else(|e| panic!("`{name}`: {}", e.message));
            let sql = match compile(
                rel,
                Options {
                    dialect,
                    ..Options::default()
                },
            ) {
                Ok(sql) => sql,
                // A refusal is a decision (`fullJoin` on MySQL, a `list` or a
                // `try*` on a dialect without one), not a failure to read back.
                Err(_) => continue,
            };
            match sqlglot_rust::parse(&sql, dialect) {
                Ok(_) => checked += 1,
                Err(e) => failures.push(format!("{d} / `{name}`: {e}\n  {sql}")),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(checked > 500, "only {checked} statements were read back");
}

/// Each dialect avoids the spellings it cannot read.
///
/// Parsing cannot decide this — the parser accepts every dialect's syntax under
/// every dialect — so the rules are stated as text invariants instead, one list
/// per target, and checked over every shared case. They are exactly the
/// constructs the dialect pass exists to rewrite: a `LIMIT` in T-SQL, a `||` in
/// MySQL (where it is boolean OR), a `NULLS LAST` where there is no such clause,
/// a `::` cast outside PostgreSQL, backticks outside BigQuery and MySQL.
#[test]
fn each_dialect_avoids_what_it_cannot_read() {
    let src = format!(
        "{TABLES}{}\n",
        CASES.iter().map(|(d, _)| *d).collect::<Vec<_>>().join("\n")
    );
    let ws = Workspace::from_source(&src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);
    let names: Vec<&str> = CASES
        .iter()
        .map(|(d, _)| d.split_whitespace().next().expect("name"))
        .collect();

    // `(dialect, forbidden text, why)`. A construct is listed only when the
    // target genuinely cannot read it, so a hit is a bug in the dialect pass.
    const FORBIDDEN: &[(&str, &[&str])] = &[
        // T-SQL paginates with `OFFSET … ROWS FETCH NEXT`, never `LIMIT`, and
        // has no `NULLS LAST` (the emulation is a leading `CASE` key).
        ("tsql", &["LIMIT ", "NULLS LAST", "NULLS FIRST"]),
        // `||` is boolean OR in MySQL, and there is no `NULLS LAST`.
        (
            "mysql",
            &["||", "NULLS LAST", "NULLS FIRST", "FULL JOIN", "::"],
        ),
        // A `::` cast is PostgreSQL's spelling; every other target takes CAST.
        ("trino", &["::"]),
        ("spark", &["::"]),
        ("bigquery", &["::"]),
        ("snowflake", &["::"]),
        ("duckdb", &["::"]),
        ("sqlite", &["::"]),
        ("ansi", &["::", "`"]),
    ];
    let mut failures = Vec::new();
    let mut checked = 0;
    for (d, forbidden) in FORBIDDEN {
        let dialect = Dialect::from_str(d).expect("dialect");
        for name in &names {
            let (_, rel) = rels
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("no query `{name}`"));
            let rel = rel
                .as_ref()
                .unwrap_or_else(|e| panic!("`{name}`: {}", e.message));
            let Ok(sql) = compile(
                rel,
                Options {
                    dialect,
                    ..Options::default()
                },
            ) else {
                continue;
            };
            checked += 1;
            // The string-literal escapes are not the construct: `'it''s'` and
            // `'a\b'` are data, so a match inside quotes is not a hit.
            let bare = strip_literals(&sql);
            for f in *forbidden {
                if bare.contains(f) {
                    failures.push(format!("{d} / `{name}`: {f:?} in\n  {sql}"));
                }
            }
            assert!(
                !bare.contains("CAGARA_"),
                "{d} / `{name}`: an intrinsic was not lowered:\n  {sql}"
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(checked > 300, "only {checked} statements were checked");
}

/// Each dialect uses the spelling it *does* have, not merely avoiding the ones it
/// does not.
///
/// [`each_dialect_avoids_what_it_cannot_read`] checks the absence of the wrong
/// spelling; this checks the presence of the right one. Both are needed: `CONCAT`
/// missing from a MySQL statement and `||` missing from it are the same fact from
/// two sides, but a generator that dropped the concatenation altogether would pass
/// the first and fail this one.
///
/// Every expectation here was read off the emitted SQL and confirmed against the
/// engines that can run — see `engines_agree` for the value-level half.
#[test]
fn each_dialect_uses_the_spelling_it_has() {
    let src = format!(
        "{TABLES}concat = users & select {{ s = .name ++ \"!\" }} & limit 3\n\
                       paged = users & order [asc .id] & limit 3 & offset 1 & select {{.id}}\n\
                       bools = nums & select {{ b = .a > 1, n = isNull .v }}\n"
    );
    let ws = Workspace::from_source(&src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);
    let compile_in = |name: &str, d: &str| {
        let (_, rel) = rels
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no query `{name}`"));
        let rel = rel
            .as_ref()
            .unwrap_or_else(|e| panic!("`{name}`: {}", e.message));
        compile(
            rel,
            Options {
                dialect: Dialect::from_str(d).expect("dialect"),
                ..Options::default()
            },
        )
        .unwrap_or_else(|e| panic!("{name} / {d}: {e}"))
    };

    // MySQL and T-SQL have no `||`: the concatenation is a function call.
    for d in ["mysql", "tsql"] {
        let sql = compile_in("concat", d);
        assert!(sql.contains("CONCAT("), "{d}: {sql}");
    }
    // The rest keep the operator, which is what `||` means there.
    for d in ["postgres", "duckdb", "sqlite", "trino", "spark", "bigquery"] {
        let sql = compile_in("concat", d);
        assert!(sql.contains("||"), "{d}: {sql}");
    }
    // Paging: T-SQL has no LIMIT at all, and spells the pair as one clause.
    let ts = compile_in("paged", "tsql");
    assert!(
        ts.contains("OFFSET 1 ROWS FETCH NEXT 2 ROWS ONLY"),
        "T-SQL paging: {ts}"
    );
    let my = compile_in("paged", "mysql");
    assert!(my.contains("LIMIT 2 OFFSET 1"), "MySQL paging: {my}");
    // A limit with no offset is `TOP` on T-SQL, where a FETCH would need an ORDER BY.
    let ts = compile_in("concat", "tsql");
    assert!(ts.contains("TOP 3"), "T-SQL limit: {ts}");
    // T-SQL has no boolean values, so a condition as a column becomes 1 / 0 with a
    // third branch left out — an unknown comparison is NULL, not false.
    let ts = compile_in("bools", "tsql");
    assert!(
        ts.contains("CASE WHEN a > 1 THEN 1 WHEN NOT (a > 1) THEN 0 END"),
        "T-SQL boolean column: {ts}"
    );
    // Everywhere else a boolean column is the condition itself.
    let pg = compile_in("bools", "postgres");
    assert!(pg.contains("a > 1 AS b"), "{pg}");
}

/// The SQL with every quoted string replaced by `''`, so a check for a construct
/// cannot be fooled by data that happens to contain it.
fn strip_literals(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut in_str = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_str => {
                in_str = true;
                out.push('\'');
            }
            '\'' if in_str => {
                // `''` is an escaped quote inside a literal.
                if chars.peek() == Some(&'\'') {
                    chars.next();
                } else {
                    in_str = false;
                }
                out.push('\'');
            }
            _ if in_str => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[test]
fn engines_agree() {
    let src = format!(
        "{TABLES}{}\n",
        CASES.iter().map(|(d, _)| *d).collect::<Vec<_>>().join("\n")
    );
    let ws = Workspace::from_source(&src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);
    let mut ran = 0;
    let mut failures = Vec::new();
    for e in ENGINES {
        let base = Options {
            dialect: Dialect::from_str(e.dialect).expect("dialect"),
            ..Options::default()
        };
        for (def, want) in CASES {
            let name = def.split_whitespace().next().expect("name");
            let (_, rel) = rels
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("no query `{name}`"));
            let rel = rel
                .as_ref()
                .unwrap_or_else(|d| panic!("`{name}`: {}", d.message));
            // Both lowerings: the optimizer rewrites the statement after
            // lowering, so a case where it changes an answer is exactly what
            // this suite exists to catch.
            for optimize in [false, true] {
                let opts = Options { optimize, ..base };
                // A refusal is a decision about the target — `rollup` on SQLite
                // and MySQL, `fullJoin` on MySQL — not a failure: nothing is
                // emitted, so there is nothing to run. Every case that compiles is
                // still checked, and `refusals_are_the_ones_we_mean` pins which.
                let Ok(sql) = compile(rel, opts) else {
                    break;
                };
                let label = if optimize {
                    format!("{} (optimized)", e.shell)
                } else {
                    e.shell.to_string()
                };
                match run(e, &sql) {
                    None if std::env::var_os("CAGARA_REQUIRE_ENGINES").is_some() => {
                        panic!(
                            "`{}` is not installed (CAGARA_REQUIRE_ENGINES is set)",
                            e.shell
                        )
                    }
                    None => {
                        eprintln!("skipping {}: `{}` is not installed", e.dialect, e.shell);
                        break;
                    }
                    Some(Err(m)) => failures.push(format!("`{name}` on {label}: {m}")),
                    Some(Ok(rows)) => {
                        ran += 1;
                        if rows != *want {
                            failures.push(format!(
                                "`{name}` on {label}: got {rows:?}, want {want:?}\n  {sql}",
                            ));
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    eprintln!("{ran} engine runs");
}

/// `parseDate`/`formatDate` mean the same thing on the engines that run here.
///
/// One Cagara query, two spellings underneath — `strftime` and `TO_CHAR` — and
/// the same rows back. SQLite is absent because it has no format *parser*;
/// `a_date_format_is_written_once_and_spelled_per_target` covers what it emits
/// instead.
#[test]
fn a_parsed_and_printed_date_agrees_on_the_engines() {
    let src = format!(
        "{TABLES}\
         fmt = days & select {{ s = formatDate \"%Y-%m-%d %H:%M:%S\" .d, ym = formatDate \"%Y-%m\" .d }}\n"
    );
    let ws = Workspace::from_source(&src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);
    let (_, rel) = rels
        .iter()
        .find(|(n, _)| n == "fmt")
        .unwrap_or_else(|| panic!("no `fmt`"));
    let rel = rel
        .as_ref()
        .unwrap_or_else(|d| panic!("`fmt`: {}", d.message));
    let want = [
        "2024-01-31 00:00:00|2024-01",
        "2024-01-07 00:00:00|2024-01",
        "2023-12-25 00:00:00|2023-12",
    ];

    // DuckDB, through its shell.
    let duck = Engine {
        shell: "duckdb",
        args: &["-batch", "-list", "-noheader", "-nullvalue", "NULL"],
        dialect: "duckdb",
    };
    let opts = Options {
        dialect: Dialect::DuckDb,
        ..Options::default()
    };
    let sql = compile(rel, opts).expect("duckdb");
    match run(&duck, &sql) {
        Some(Ok(rows)) => assert_eq!(rows, want, "{sql}"),
        Some(Err(m)) => panic!("duckdb: {m}\n  {sql}"),
        None if std::env::var_os("CAGARA_REQUIRE_ENGINES").is_some() => {
            panic!("`duckdb` is not installed (CAGARA_REQUIRE_ENGINES is set)")
        }
        None => eprintln!("skipping duckdb: not installed"),
    }

    // PostgreSQL, through PGlite.
    let harness = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pglite.mjs");
    let ready = Command::new("node")
        .args([harness, "--check"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ready {
        assert!(
            std::env::var_os("CAGARA_REQUIRE_ENGINES").is_none(),
            "`node` with @electric-sql/pglite is not installed (CAGARA_REQUIRE_ENGINES is set)"
        );
        eprintln!("skipping postgres: node or @electric-sql/pglite is not installed");
        return;
    }
    // `days.d` is a `date` in both setups; only the schema and the `DOUBLE`
    // spelling differ, so the SQL is the same shape.
    let setup = format!(
        "CREATE SCHEMA main;\n{}",
        SETUP.replace("DOUBLE", "DOUBLE PRECISION")
    );
    let opts = Options {
        dialect: Dialect::Postgres,
        ..Options::default()
    };
    let sql = compile(rel, opts).expect("postgres");
    let blocks = pg_batch(harness, &setup, std::slice::from_ref(&sql));
    assert_eq!(blocks[0], want, "{sql}");
}

/// Money on the engines: the sum of three 19.99s is 59.97 exactly, and
/// `0.1 + 0.2` is `0.3` — the two places a binary float shows itself.
///
/// One Cagara query, two spellings underneath (`DECIMAL(10, 2)` and `NUMERIC`),
/// the same rows back.
#[test]
fn money_is_exact_on_the_engines() {
    let setup = "CREATE TABLE main.money (id INTEGER, total DECIMAL(10, 2), part DECIMAL(10, 1));\n\
                 INSERT INTO main.money VALUES (1, 19.99, 0.1), (2, 19.99, 0.2), (3, 19.99, 0.3);\n";
    let src = "money : query { id = int, total = decimal 10 2, part = decimal 10 1 } = \
               table \"main\" \"money\"\n\
               q = money & agg { revenue = sum .total, parts = sum .part }\n\
               prod = money & select { id = .id, gross = .total * .total }\n\
               sum2 = money & select { id = .id, both = .total + .total }\n";
    let ws = Workspace::from_source(src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);
    let (_, rel) = rels
        .iter()
        .find(|(n, _)| n == "q")
        .unwrap_or_else(|| panic!("no `q`"));
    let rel = rel
        .as_ref()
        .unwrap_or_else(|d| panic!("`q`: {}", d.message));
    let want = ["59.97|0.6"];
    let (_, prod) = rels
        .iter()
        .find(|(n, _)| n == "prod")
        .unwrap_or_else(|| panic!("no `prod`"));
    let prod = prod
        .as_ref()
        .unwrap_or_else(|d| panic!("`prod`: {}", d.message));
    // The addition is the other case where the value may need more digits than
    // the type states (10 2 + 10 1 is 11 1 by SQL's rule, 10 2 by the language's).
    let (_, sum2) = rels
        .iter()
        .find(|(n, _)| n == "sum2")
        .unwrap_or_else(|| panic!("no `sum2`"));
    let sum2 = sum2
        .as_ref()
        .unwrap_or_else(|d| panic!("`sum2`: {}", d.message));
    // 19.99 * 19.99 is 399.6001 exactly, and both engines render it the same way
    // even though their decimal *types* differ (DuckDB `DECIMAL(18,4)`,
    // PostgreSQL `numeric`): the product keeps the operand type in Cagara and the
    // exact value in the engine.
    let prod_want = ["1|399.6001", "2|399.6001", "3|399.6001"];
    // `total + total` is `decimal 10 2` in Cagara and both engines add two
    // `DECIMAL(10,2)` exactly: 19.99 + 19.99 is 39.98. (Adding `part`, a
    // `decimal 10 1`, is a compile error — one type everywhere.)
    let sum2_want = ["1|39.98", "2|39.98", "3|39.98"];

    let duck = Engine {
        shell: "duckdb",
        args: &["-batch", "-list", "-noheader", "-nullvalue", "NULL"],
        dialect: "duckdb",
    };
    let sql = compile(
        rel,
        Options {
            dialect: Dialect::DuckDb,
            ..Options::default()
        },
    )
    .expect("duckdb");
    let out = run_setup(&duck, setup, &sql);
    match out {
        Some(Ok(rows)) => assert_eq!(rows, want, "{sql}"),
        Some(Err(m)) => panic!("duckdb: {m}\n  {sql}"),
        None if std::env::var_os("CAGARA_REQUIRE_ENGINES").is_some() => {
            panic!("`duckdb` is not installed (CAGARA_REQUIRE_ENGINES is set)")
        }
        None => eprintln!("skipping duckdb: not installed"),
    }
    let prod_sql = compile(
        prod,
        Options {
            dialect: Dialect::DuckDb,
            ..Options::default()
        },
    )
    .expect("duckdb product");
    if let Some(rows) = run_setup(&duck, setup, &prod_sql) {
        match rows {
            Ok(rows) => assert_eq!(rows, prod_want, "{prod_sql}"),
            Err(m) => panic!("duckdb: {m}\n  {prod_sql}"),
        }
    }

    let harness = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pglite.mjs");
    let ready = Command::new("node")
        .args([harness, "--check"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ready {
        assert!(
            std::env::var_os("CAGARA_REQUIRE_ENGINES").is_none(),
            "`node` with @electric-sql/pglite is not installed (CAGARA_REQUIRE_ENGINES is set)"
        );
        return;
    }
    let pg = compile(
        rel,
        Options {
            dialect: Dialect::Postgres,
            ..Options::default()
        },
    )
    .expect("postgres");
    let pg_setup = format!("CREATE SCHEMA main;\n{setup}");
    let blocks = pg_batch(harness, &pg_setup, std::slice::from_ref(&pg));
    assert_eq!(blocks[0], want, "{pg}");
    let prod_pg = compile(
        prod,
        Options {
            dialect: Dialect::Postgres,
            ..Options::default()
        },
    )
    .expect("postgres product");
    let blocks = pg_batch(harness, &pg_setup, std::slice::from_ref(&prod_pg));
    assert_eq!(blocks[0], prod_want, "{prod_pg}");
    let sum2_pg = compile(
        sum2,
        Options {
            dialect: Dialect::Postgres,
            ..Options::default()
        },
    )
    .expect("postgres sum");
    let blocks = pg_batch(harness, &pg_setup, std::slice::from_ref(&sum2_pg));
    assert_eq!(blocks[0], sum2_want, "{sum2_pg}");
}

/// `lift` on the engines: one row has a value and one does not, so the NULL
/// branch of the guard is what the second row measures.
#[test]
fn a_lifted_value_keeps_its_nulls_on_the_engines() {
    // DuckDB and SQLite have no schemas unless one is made; both engines get the
    // same `main` schema, which is where the other cases live too.
    let setup = "CREATE SCHEMA IF NOT EXISTS main;\n\
                 CREATE TABLE main.d (id INTEGER, packed TEXT);\n\
                 INSERT INTO main.d VALUES (1, '20240131'), (2, NULL);\n";
    let src = "d : query { id = int, packed = maybe string } = table \"main\" \"d\"\n\
               q = d & order [asc .id] & select { id = .id, p = lift (parseDate \"%Y%m%d\") .packed }\n";
    let ws = Workspace::from_source(src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);
    let (_, rel) = rels
        .iter()
        .find(|(n, _)| n == "q")
        .unwrap_or_else(|| panic!("no `q`"));
    let rel = rel
        .as_ref()
        .unwrap_or_else(|d| panic!("`q`: {}", d.message));
    let want = ["1|2024-01-31", "2|NULL"];

    let duck = Engine {
        shell: "duckdb",
        args: &["-batch", "-list", "-noheader", "-nullvalue", "NULL"],
        dialect: "duckdb",
    };
    let sql = compile(
        rel,
        Options {
            dialect: Dialect::DuckDb,
            ..Options::default()
        },
    )
    .expect("duckdb");
    let Some(rows) = run_setup(&duck, setup, &sql) else {
        panic!("`duckdb` is not installed");
    };
    match rows {
        Ok(rows) => assert_eq!(rows, want, "{sql}"),
        Err(m) => panic!("duckdb: {m}\n  {sql}"),
    }

    let harness = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pglite.mjs");
    let pg = compile(
        rel,
        Options {
            dialect: Dialect::Postgres,
            ..Options::default()
        },
    )
    .expect("postgres");
    let pg_setup = String::from("CREATE SCHEMA main;\n").clone()
        + &setup.replace("CREATE SCHEMA IF NOT EXISTS main;\n", "");
    let blocks = pg_batch(harness, &pg_setup, std::slice::from_ref(&pg));
    assert_eq!(blocks[0], want, "{pg}");
}

/// The tables `examples/report.cagara` queries, with rows chosen so every
/// clause of the report has something to say.
///
/// SQLite has no `CREATE SCHEMA`, and rejects `public.users` outright, so the
/// two engines get their own setup. DuckDB needs the schema spelled out
/// because the example names one.
const REPORT_SETUP_SQLITE: &str = "\
CREATE TABLE users (id INTEGER, name TEXT, age INTEGER, active BOOLEAN);
INSERT INTO users VALUES (1, 'ann', 34, 1), (2, 'bob', 17, 1), (3, 'cy', 29, 0), (4, 'dee', 41, 1);
CREATE TABLE orders (id INTEGER, user_id INTEGER, amount DOUBLE, status TEXT, created_at DATE);
INSERT INTO orders VALUES
  (1, 1, 10.0, 'paid', '2024-01-01'), (2, 1, 20.0, 'paid', '2024-01-02'),
  (3, 1, 30.0, 'paid', '2024-01-03'), (4, 1, 40.0, 'paid', '2024-01-04'),
  (5, 1, 50.0, 'paid', '2024-01-05'), (6, 1, 60.0, 'paid', '2024-01-06'),
  (7, 3, 70.0, 'paid', '2024-02-01'), (8, 3, 80.0, 'pending', '2024-02-02'),
  (9, 4, 90.0, 'paid', '2024-02-03');
";

const REPORT_SETUP_DUCKDB: &str = "\
CREATE SCHEMA IF NOT EXISTS public;
CREATE TABLE public.users (id INTEGER, name TEXT, age INTEGER, active BOOLEAN);
INSERT INTO public.users VALUES (1, 'ann', 34, TRUE), (2, 'bob', 17, TRUE), (3, 'cy', 29, FALSE), (4, 'dee', 41, TRUE);
CREATE TABLE public.orders (id INTEGER, user_id INTEGER, amount DOUBLE, status TEXT, created_at DATE);
INSERT INTO public.orders VALUES
  (1, 1, 10.0, 'paid', '2024-01-01'), (2, 1, 20.0, 'paid', '2024-01-02'),
  (3, 1, 30.0, 'paid', '2024-01-03'), (4, 1, 40.0, 'paid', '2024-01-04'),
  (5, 1, 50.0, 'paid', '2024-01-05'), (6, 1, 60.0, 'paid', '2024-01-06'),
  (7, 3, 70.0, 'paid', '2024-02-01'), (8, 3, 80.0, 'pending', '2024-02-02'),
  (9, 4, 90.0, 'paid', '2024-02-03');
";

/// `(definition in `report.cagara`, expected rows, order-independent?)`.
const REPORT_CASES: &[(&str, &[&str], bool)] = &[
    // Adult, active users, ordered by the uppercased label, capped at 10.
    ("adults", &["1|ANN|35", "4|DEE|42"], true),
    // Only users with at least five paid orders; `ann` has six, `cy` one.
    ("revenue", &["1|210.0|6"], true),
    // The latest three orders per user, newest first.
    (
        "latest",
        &["6|1|1", "5|1|2", "4|1|3", "8|3|1", "7|3|2", "9|4|1"],
        true,
    ),
    // A running total per user, in order of `created_at`.
    (
        "running",
        &[
            "1|10.0", "2|30.0", "3|60.0", "4|100.0", "5|150.0", "6|210.0", "7|70.0", "8|150.0",
            "9|90.0",
        ],
        true,
    ),
    // Every order joined to its user.
    (
        "order_names",
        &[
            "1|ann|10.0",
            "2|ann|20.0",
            "3|ann|30.0",
            "4|ann|40.0",
            "5|ann|50.0",
            "6|ann|60.0",
            "7|cy|70.0",
            "8|cy|80.0",
            "9|dee|90.0",
        ],
        true,
    ),
    // Every user, including those with no orders.
    (
        "user_totals",
        &["1|210.0|6", "2|0.0|0", "3|150.0|2", "4|90.0|1"],
        true,
    ),
    // The shorthand spelling of the same, restricted to active users and
    // ordered by the descending total.
    (
        "active_totals",
        &["1|ann|210.0", "4|dee|90.0", "2|bob|0.0"],
        false,
    ),
];

/// Run the real `examples/report.cagara` against every engine. The focused
/// cases above pin individual rules; this pins that a whole, realistic file
/// compiles and runs, which is what a user actually does.
#[test]
fn the_report_example_runs_on_every_engine() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/report.cagara");
    let src = std::fs::read_to_string(path).expect("read examples/report.cagara");
    let ws = Workspace::from_source(&src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);
    let mut ran = 0;
    let mut failures = Vec::new();
    for e in ENGINES {
        let setup = if e.shell == "sqlite3" {
            REPORT_SETUP_SQLITE
        } else {
            REPORT_SETUP_DUCKDB
        };
        let opts = Options {
            dialect: Dialect::from_str(e.dialect).expect("dialect"),
            ..Options::default()
        };
        for (name, want, unordered) in REPORT_CASES {
            let (_, rel) = rels
                .iter()
                .find(|(n, _)| n == *name)
                .unwrap_or_else(|| panic!("report.cagara has no `{name}`"));
            let rel = rel
                .as_ref()
                .unwrap_or_else(|d| panic!("`{name}`: {}", d.message));
            let sql =
                compile(rel, opts).unwrap_or_else(|m| panic!("`{name}` / {}: {m}", e.dialect));
            // SQLite has no schemas at all, so the example's `public.` has to
            // come off before its shell can resolve the names. The rewrite is
            // confined to this test; what is verified is the rest of the SQL.
            let sql = if e.shell == "sqlite3" {
                sql.replace("public.", "")
            } else {
                sql
            };
            let args: Vec<&std::ffi::OsStr> = e.args.iter().map(std::ffi::OsStr::new).collect();
            match run_script(e.shell, &args, setup, &sql) {
                None if std::env::var_os("CAGARA_REQUIRE_ENGINES").is_some() => {
                    panic!(
                        "`{}` is not installed (CAGARA_REQUIRE_ENGINES is set)",
                        e.shell
                    )
                }
                None => {
                    eprintln!("skipping {}: `{}` is not installed", e.dialect, e.shell);
                    break;
                }
                Some(Err(m)) => failures.push(format!("`{name}` on {}: {m}", e.shell)),
                Some(Ok(rows)) => {
                    ran += 1;
                    let ok = if *unordered {
                        // Compare as sorted multisets: this test is about the
                        // SQL compiling and computing the right rows, not
                        // about a report's own row order.
                        let mut got = rows.clone();
                        let mut expected: Vec<String> =
                            want.iter().map(|s| s.to_string()).collect();
                        got.sort_unstable();
                        expected.sort_unstable();
                        got == expected
                    } else {
                        rows == want.iter().map(|s| s.to_string()).collect::<Vec<_>>()
                    };
                    if !ok {
                        failures.push(format!(
                            "`{name}` on {}: got {rows:?}, want {want:?}\n  {sql}",
                            e.shell
                        ));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    eprintln!("{ran} report runs");
}

/// A `list` query runs on an engine that has arrays.
///
/// SQLite has no array type, so the differential cases above cannot carry one;
/// this is the DuckDB half, run when the shell is installed. The rows are
/// compared as text, and DuckDB prints a list as `[1, 2]`, so the expected
/// values are written the way the shell renders them.
#[test]
fn a_list_query_runs_on_duckdb() {
    let engine = ENGINES
        .iter()
        .find(|e| e.dialect == "duckdb")
        .expect("duckdb engine");
    let src = format!(
        "{TABLES}\
         list_agg = orders & agg {{ k = group .user_id, amounts = arrayAgg .amount }}\n\
         list_lit = orders & select {{ one = [1, 2, 3] }}\n\
         list_ops = orders & where (.id == 10) \
                   & agg {{ k = group .user_id, xs = arrayAgg .id }} \
                   & select {{ n = listLength .xs, has = listContains .xs 10 }}\n\
         list_round = orders & agg {{ xs = arrayAgg .id }} & unnest .xs \
                    & select {{ id = .value }}\n"
    );
    let ws = Workspace::from_source(&src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);
    let opts = Options {
        dialect: Dialect::from_str("duckdb").expect("dialect"),
        ..Options::default()
    };
    for name in ["list_agg", "list_lit", "list_ops", "list_round"] {
        let (_, rel) = rels
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no query `{name}`"));
        let rel = rel
            .as_ref()
            .unwrap_or_else(|d| panic!("`{name}`: {}", d.message));
        let sql = compile(rel, opts).unwrap_or_else(|m| panic!("`{name}`: {m}"));
        match run(engine, &sql) {
            None if std::env::var_os("CAGARA_REQUIRE_ENGINES").is_some() => {
                panic!("`duckdb` is not installed (CAGARA_REQUIRE_ENGINES is set)")
            }
            None => {
                eprintln!("skipping duckdb: not installed");
                return;
            }
            Some(Err(m)) => panic!("`{name}` on duckdb: {m}\n  {sql}"),
            Some(Ok(rows)) => assert!(!rows.is_empty(), "`{name}` returned no rows\n  {sql}"),
        }
    }
}

/// Two rows are the same when every column is: text for text, number for
/// number.
///
/// The shells print numbers their own way — sqlite3 writes a `DOUBLE` as
/// `99.0`, Postgres as `99` — so a textual comparison would report a difference
/// where the values agree.
fn same_row(a: &str, b: &str) -> bool {
    // Each shell prints a value its own way: `true` (PGlite, and `True` on Trino)
    // against `1` (SQLite, DuckDB), and a float with however many digits the
    // engine keeps. A cell compares equal when it is the same text, the same
    // number, or the same boolean.
    let cell = |s: &str| -> String {
        match s {
            "true" | "True" | "TRUE" => "1".to_string(),
            "false" | "False" | "FALSE" => "0".to_string(),
            other => other.to_string(),
        }
    };
    let (x, y): (Vec<String>, Vec<String>) = (
        a.split('|').map(cell).collect(),
        b.split('|').map(cell).collect(),
    );
    x.len() == y.len()
        && x.iter().zip(&y).all(|(p, q)| {
            p == q
                || match (p.parse::<f64>(), q.parse::<f64>()) {
                    (Ok(p), Ok(q)) => p == q,
                    _ => false,
                }
        })
}

/// The `main` schema the shared setup writes into, in Postgres's spelling.
fn pg_setup(setup: &str) -> String {
    format!(
        "CREATE SCHEMA main;\n{}",
        setup.replace("DOUBLE", "DOUBLE PRECISION")
    )
}

/// Run `queries` against one fresh Postgres and return each query's rows.
///
/// One process for the whole batch: booting a WASM Postgres costs a few seconds
/// and a suite of twenty queries would pay it twenty times. The batch format is
/// the setup, then the queries, separated by a record separator
/// (`crates/cagara-sql/tests/pglite.mjs`).
fn pg_batch(harness: &str, setup: &str, queries: &[String]) -> Vec<Vec<String>> {
    let mut script = setup.to_string();
    for q in queries {
        script.push('\u{1e}');
        script.push_str(q);
    }
    let args = [std::ffi::OsStr::new(harness)];
    let lines = run_script("node", &args, "", &script)
        .expect("node ran")
        .unwrap_or_else(|m| panic!("postgres: {m}"));
    let mut out: Vec<Vec<String>> = vec![Vec::new()];
    for line in lines {
        if line == "\u{1e}" {
            out.push(Vec::new());
        } else {
            out.last_mut().expect("a block").push(line);
        }
    }
    // A trailing separator would leave an empty block; the harness never writes
    // one, so a length mismatch is a harness bug worth seeing.
    assert_eq!(out.len(), queries.len(), "one block per query: {out:?}");
    out
}

/// A batch through the Trino harness, which speaks the same protocol as
/// `pglite.mjs`: `setup \x1e query \x1e query…`, blocks separated by the same
/// record separator in the output.
fn trino_batch(harness: &str, setup: &str, queries: &[String]) -> Vec<Vec<String>> {
    let mut script = setup.to_string();
    for q in queries {
        script.push('\u{1e}');
        script.push_str(q);
    }
    let args = [std::ffi::OsStr::new(harness)];
    let lines = run_script("bash", &args, "", &script)
        .expect("bash ran")
        .unwrap_or_else(|m| panic!("trino: {m}"));
    let mut out: Vec<Vec<String>> = vec![Vec::new()];
    for line in lines {
        if line == "\u{1e}" {
            out.push(Vec::new());
        } else {
            out.last_mut().expect("a block").push(line);
        }
    }
    assert_eq!(out.len(), queries.len(), "one block per query: {out:?}");
    out
}

/// Trino, through a server this machine starts by hand (`tests/trino.sh`).
///
/// Trino is the strictest target in the family: no implicit casts between
/// numeric types, `||` for concatenation, `OFFSET … FETCH` and no `LIMIT` in the
/// old releases, and its own spelling for half the functions. It is also the one
/// dialect where a wrong type in a `CASE` or a `UNION` is an *error* rather than
/// a coercion, which is exactly what a text check cannot see.
///
/// The server is not part of `nix develop` — it is 680 MiB of jars — so the test
/// skips unless one answers. `docs/ENGINE-TESTS.md` says how to start it.
#[test]
fn trino_agrees_when_a_server_is_running() {
    let harness = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/trino.sh");
    let ready = Command::new("bash")
        .args([harness, "--check"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ready {
        assert!(
            std::env::var_os("CAGARA_REQUIRE_ENGINES").is_none(),
            "no Trino at ${{TRINO_URL:-http://localhost:18080}} (CAGARA_REQUIRE_ENGINES is set)"
        );
        eprintln!("skipping trino: no server answers");
        return;
    }
    // The tables, in Trino's `memory` connector. It has no `DELETE`, so the
    // setup drops what a previous run may have made.
    // The shared [`SETUP`], in Trino's `memory` connector: no `DELETE`, so each
    // table is dropped and made again, and `VARCHAR` where the shared script says
    // `TEXT`. One statement per line, because Trino takes one per request.
    let setup = "DROP TABLE IF EXISTS main.nums\n\
                 CREATE TABLE main.nums (id INTEGER, a INTEGER, b INTEGER, v INTEGER)\n\
                 INSERT INTO main.nums VALUES (1, 7, 2, 10), (2, -7, 2, NULL), \
                 (3, 7, -2, 30), (4, -8, 3, 20)\n\
                 DROP TABLE IF EXISTS main.users\n\
                 CREATE TABLE main.users (id INTEGER, name VARCHAR)\n\
                 INSERT INTO main.users VALUES (1, 'ann'), (2, 'bob'), \
                 (3, 'it''s a\\b ')\n\
                 DROP TABLE IF EXISTS main.orders\n\
                 CREATE TABLE main.orders (id INTEGER, user_id INTEGER, amount DOUBLE)\n\
                 INSERT INTO main.orders VALUES (10, 1, 5.5), (11, 1, 4.5), (12, 3, 1.0)\n\
                 DROP TABLE IF EXISTS main.days\n\
                 CREATE TABLE main.days (id INTEGER, d DATE)\n\
                 INSERT INTO main.days VALUES (1, DATE '2024-01-31'), \
                 (2, DATE '2024-01-07'), (3, DATE '2023-12-25')\n\
                 DROP TABLE IF EXISTS main.\"select\"\n\
                 CREATE TABLE main.\"select\" (id INTEGER, \"order\" INTEGER, \"userId\" INTEGER)\n\
                 INSERT INTO main.\"select\" VALUES (1, 5, 9)\n\
                 DROP TABLE IF EXISTS main.empty\n\
                 CREATE TABLE main.empty (id INTEGER)\n\
                 DROP TABLE IF EXISTS main.dupes\n\
                 CREATE TABLE main.dupes (id INTEGER, k VARCHAR)\n\
                 INSERT INTO main.dupes VALUES (1, 'x'), (1, 'x'), (2, 'x'), (3, 'y')\n\
                 DROP TABLE IF EXISTS main.g\n\
                 CREATE TABLE main.g (a INTEGER, b INTEGER, n INTEGER)\n\
                 INSERT INTO main.g VALUES (1, 1, 1), (1, 2, 2), (2, 1, 3)\n";
    let trino = Options {
        dialect: Dialect::Trino,
        ..Options::default()
    };
    let src = format!(
        "{TABLES}{}\n",
        CASES.iter().map(|(d, _)| *d).collect::<Vec<_>>().join("\n")
    );
    let ws = Workspace::from_source(&src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);

    let mut wanted: Vec<(&str, &[&str])> = Vec::new();
    let mut queries = Vec::new();
    for (def, want) in CASES {
        let name = def.split_whitespace().next().expect("name");
        let (_, rel) = rels
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no query `{name}`"));
        let rel = rel
            .as_ref()
            .unwrap_or_else(|d| panic!("`{name}`: {}", d.message));
        // A refusal is a decision about the target, not a failure: Trino has no
        // `list` type in this connector's spelling and no `fullJoin` gap of its
        // own, so whatever it refuses simply does not run here.
        let Ok(sql) = compile(rel, trino) else {
            continue;
        };
        wanted.push((name, *want));
        queries.push(sql);
    }
    assert!(!queries.is_empty(), "nothing compiled for Trino");
    let blocks = trino_batch(harness, setup, &queries);
    let mut failures = Vec::new();
    for ((name, want), got) in wanted.iter().zip(&blocks) {
        let mut got = got.clone();
        let mut want: Vec<&str> = want.to_vec();
        got.sort();
        want.sort();
        if got.len() != want.len() || !got.iter().zip(&want).all(|(a, b)| same_row(a, b)) {
            failures.push(format!("`{name}` on trino: got {got:?}, want {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// MySQL, through a server this machine starts by hand (`tests/mysql.sh`).
///
/// The MySQL family is the one whose *semantics* differ most, not just its
/// spelling: `||` is boolean OR, integer division and `%` round differently,
/// `FULL JOIN` does not exist, `CAST` to `CHAR` rather than `TEXT`, and a boolean
/// column is `1`/`0` rather than `true`/`false`. MariaDB is used because it ships
/// as a self-contained binary tarball — no root, no container — and is a
/// drop-in for the dialect.
///
/// Skips unless `MYSQL_SOCKET` and `MYSQL` name a running server.
#[test]
fn mysql_agrees_when_a_server_is_running() {
    let harness = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mysql.sh");
    let ready = Command::new("bash")
        .args([harness, "--check"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ready {
        assert!(
            std::env::var_os("CAGARA_REQUIRE_ENGINES").is_none(),
            "no MySQL server (set MYSQL_SOCKET and MYSQL; CAGARA_REQUIRE_ENGINES is set)"
        );
        eprintln!("skipping mysql: no server answers");
        return;
    }
    // The shared [`SETUP`], in MySQL's spelling: `TEXT` is fine here (the *cast*
    // spells `CHAR`), and `DELETE` rather than a drop-and-create keeps the
    // connection cheap.
    let setup = "DROP TABLE IF EXISTS nums\n\
                 CREATE TABLE nums (id INT, a INT, b INT, v INT)\n\
                 INSERT INTO nums VALUES (1, 7, 2, 10), (2, -7, 2, NULL), \
                 (3, 7, -2, 30), (4, -8, 3, 20)\n\
                 DROP TABLE IF EXISTS users\n\
                 CREATE TABLE users (id INT, name TEXT)\n\
                 INSERT INTO users VALUES (1, 'ann'), (2, 'bob'), \
                 (3, 'it''s a\\\\b ')\n\
                 DROP TABLE IF EXISTS orders\n\
                 CREATE TABLE orders (id INT, user_id INT, amount DOUBLE)\n\
                 INSERT INTO orders VALUES (10, 1, 5.5), (11, 1, 4.5), (12, 3, 1.0)\n\
                 DROP TABLE IF EXISTS days\n\
                 CREATE TABLE days (id INT, d DATE)\n\
                 INSERT INTO days VALUES (1, '2024-01-31'), (2, '2024-01-07'), \
                 (3, '2023-12-25')\n\
                 DROP TABLE IF EXISTS `select`\n\
                 CREATE TABLE `select` (id INT, `order` INT, userId INT)\n\
                 INSERT INTO `select` VALUES (1, 5, 9)\n\
                 DROP TABLE IF EXISTS empty\n\
                 CREATE TABLE empty (id INT)\n\
                 DROP TABLE IF EXISTS dupes\n\
                 CREATE TABLE dupes (id INT, k TEXT)\n\
                 INSERT INTO dupes VALUES (1, 'x'), (1, 'x'), (2, 'x'), (3, 'y')\n\
                 DROP TABLE IF EXISTS g\n\
                 CREATE TABLE g (a INT, b INT, n INT)\n\
                 INSERT INTO g VALUES (1, 1, 1), (1, 2, 2), (2, 1, 3)\n";
    let mysql = Options {
        dialect: Dialect::Mysql,
        ..Options::default()
    };
    let src = format!(
        "{TABLES}{}\n",
        CASES.iter().map(|(d, _)| *d).collect::<Vec<_>>().join("\n")
    );
    let ws = Workspace::from_source(&src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);

    let mut wanted: Vec<(&str, &[&str])> = Vec::new();
    let mut queries = Vec::new();
    for (def, want) in CASES {
        let name = def.split_whitespace().next().expect("name");
        let (_, rel) = rels
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no query `{name}`"));
        let rel = rel
            .as_ref()
            .unwrap_or_else(|d| panic!("`{name}`: {}", d.message));
        // A refusal is a decision about the target (`fullJoin` on this family),
        // not a failure to compile.
        let Ok(sql) = compile(rel, mysql) else {
            continue;
        };
        wanted.push((name, *want));
        queries.push(sql);
    }
    assert!(!queries.is_empty(), "nothing compiled for MySQL");
    let blocks = mysql_batch(harness, setup, &queries);
    let mut failures = Vec::new();
    for ((name, want), got) in wanted.iter().zip(&blocks) {
        let mut got = got.clone();
        let mut want: Vec<&str> = want.to_vec();
        got.sort();
        want.sort();
        if got.len() != want.len() || !got.iter().zip(&want).all(|(a, b)| same_row(a, b)) {
            failures.push(format!("`{name}` on mysql: got {got:?}, want {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A batch through a shell harness that speaks `pglite.mjs`'s protocol.
fn mysql_batch(harness: &str, setup: &str, queries: &[String]) -> Vec<Vec<String>> {
    let mut script = setup.to_string();
    for q in queries {
        script.push('\u{1e}');
        script.push_str(q);
    }
    let args = [std::ffi::OsStr::new(harness)];
    let lines = run_script("bash", &args, "", &script)
        .expect("bash ran")
        .unwrap_or_else(|m| panic!("mysql: {m}"));
    let mut out: Vec<Vec<String>> = vec![Vec::new()];
    for line in lines {
        if line == "\u{1e}" {
            out.push(Vec::new());
        } else {
            out.last_mut().expect("a block").push(line);
        }
    }
    assert_eq!(out.len(), queries.len(), "one block per query: {out:?}");
    out
}

/// Cases the emulation cannot judge, and why. Every one of them is *also* run on
/// the five real engines, so nothing here is unchecked — it is unchecked *by this
/// route*.
///
/// `div` is `a / b` on two integer columns. Integer division is what ANSI and T-SQL
/// mean by it, and DuckDB's `/` is float division for every type; the emulator
/// cannot translate it faithfully because it does not know the operand types, and
/// teaching it means passing a schema Python sqlglot does not use for arithmetic
/// (checked: `annotate_types` with the real column types still emits `a / b`).
const EMULATION_BLIND: &[(&str, &str)] = &[("div", "ansi"), ("div", "tsql")];

/// The statements the emulation cannot *run*, pinned so that a statement which used
/// to run and stops running is a failure rather than a silent loss of coverage.
///
/// All three are T-SQL, and all three are limits of the *second opinion*, not of our
/// output. Each was read by hand before it was listed here:
///
/// - `str` — `LEN(name + 'x') - 1` is right: `+` concatenates in T-SQL, and `LEN`
///   ignores trailing spaces, hence the `- 1`. Python sqlglot passes the `+` through
///   and DuckDB refuses to add two strings.
/// - `rw_null_ext_fn` — `CAST(amount AS VARCHAR(MAX))` is right. Python sqlglot turns
///   it into `CAST(amount AS TEXT(MAX))`, which DuckDB does not parse.
/// - `dt` — `@@DATEFIRST` is a T-SQL global, and the expression is the documented way
///   to make `DATEPART(WEEKDAY, …)` start on Monday. Python sqlglot rewrites it to
///   `$$DATEFIRST`, a DuckDB parameter reference, which is a parse error.
const EMULATION_UNREADABLE: &[(&str, &str)] =
    &[("str", "tsql"), ("rw_null_ext_fn", "tsql"), ("dt", "tsql")];

/// The emulated dialects: our SQL, read by an independent parser, run on DuckDB.
///
/// T-SQL, BigQuery, Snowflake and ANSI have no server here. `tests/emulate.py`
/// transpiles their SQL to DuckDB with Python's `sqlglot` — a different
/// implementation from the Rust one this backend generates with — and the rows are
/// compared with the ones the real engines produce for the same case. That answers
/// the question the text checks cannot: do these dialects *mean* the same thing.
///
/// What it does not prove, and the test's doc says so: a construct both
/// implementations misunderstand passes, and DuckDB is doing the computing for every
/// dialect. It is a second opinion, not the server.
#[test]
fn the_emulated_dialects_mean_the_same_thing() {
    let harness = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/emulate.py");
    let python = std::env::var("CAGARA_PYTHON").unwrap_or_else(|_| "python3".to_string());
    let ready = Command::new(&python)
        .args([harness, "--check"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ready {
        assert!(
            std::env::var_os("CAGARA_REQUIRE_ENGINES").is_none(),
            "no `sqlglot` for {python} (set CAGARA_PYTHON; CAGARA_REQUIRE_ENGINES is set)"
        );
        eprintln!("skipping the emulated dialects: sqlglot is not importable");
        return;
    }
    let duck = Engine {
        shell: "duckdb",
        args: &["-batch", "-list", "-noheader", "-nullvalue", "NULL"],
        dialect: "duckdb",
    };
    let src = format!(
        "{TABLES}{}\n",
        CASES.iter().map(|(d, _)| *d).collect::<Vec<_>>().join("\n")
    );
    let ws = Workspace::from_source(&src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);

    let mut failures: Vec<String> = Vec::new();
    let mut ran = 0;
    let mut unreadable: Vec<(String, String)> = Vec::new();
    for dialect_name in ["tsql", "bigquery", "snowflake", "ansi"] {
        let dialect = Dialect::from_str(dialect_name).expect("dialect");
        let mut names: Vec<(&str, &[&str])> = Vec::new();
        let mut statements: Vec<String> = Vec::new();
        for (def, want) in CASES {
            let name = def.split_whitespace().next().expect("name");
            let (_, rel) = rels
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("no query `{name}`"));
            let rel = rel
                .as_ref()
                .unwrap_or_else(|d| panic!("`{name}`: {}", d.message));
            let Ok(sql) = compile(
                rel,
                Options {
                    dialect,
                    ..Options::default()
                },
            ) else {
                continue;
            };
            names.push((name, *want));
            statements.push(sql);
        }
        let translated = emulate(&python, harness, dialect_name, &statements);
        for ((name, want), got) in names.iter().zip(&translated) {
            if EMULATION_BLIND.contains(&(name, dialect_name)) {
                continue;
            }
            let Ok(sql) = got else {
                // Either Python sqlglot cannot parse our output in this dialect, or
                // the DuckDB SQL it produces does not run. Both are worth knowing and
                // neither is automatically our bug, so the *set* is pinned rather
                // than the count: a statement that used to run and stops running is a
                // regression, and it fails the test instead of quietly shrinking the
                // coverage.
                unreadable.push((name.to_string(), dialect_name.to_string()));
                continue;
            };
            match run_setup(&duck, SETUP, sql) {
                Some(Ok(rows)) => {
                    ran += 1;
                    let mut rows = rows.clone();
                    let mut want: Vec<&str> = want.to_vec();
                    rows.sort();
                    want.sort();
                    if rows.len() != want.len()
                        || !rows.iter().zip(&want).all(|(a, b)| same_instant(a, b))
                    {
                        failures.push(format!(
                            "`{name}` as {dialect_name}: got {rows:?}, want {want:?}\n  {sql}"
                        ));
                    }
                }
                Some(Err(m)) => {
                    if std::env::var_os("CAGARA_DEBUG_EMU").is_some() {
                        eprintln!("SKIP {name} as {dialect_name}: {m}");
                    }
                    unreadable.push((name.to_string(), dialect_name.to_string()));
                }
                None => panic!("`duckdb` is not installed"),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    let mut seen = unreadable.clone();
    seen.sort();
    let mut expected: Vec<(String, String)> = EMULATION_UNREADABLE
        .iter()
        .map(|(case, dialect)| (case.to_string(), dialect.to_string()))
        .collect();
    expected.sort();
    assert_eq!(
        seen, expected,
        "the set of statements the emulator cannot run changed: {ran} ran"
    );
}

/// `same_row`, plus one rendering difference the emulation introduces: DuckDB's
/// interval arithmetic returns a `TIMESTAMP` where BigQuery and Snowflake return a
/// `DATE`, so `2024-02-29` comes back as `2024-02-29 00:00:00`. That is the same
/// instant and the same value; it is not the same type, and a type check is
/// something this route cannot do (DuckDB computes every dialect's answer).
fn same_instant(a: &str, b: &str) -> bool {
    let (x, y): (Vec<&str>, Vec<&str>) = (a.split('|').collect(), b.split('|').collect());
    x.len() == y.len()
        && x.iter().zip(&y).all(|(p, q)| {
            same_row(p, q) || {
                let midnight = |s: &str| s.strip_suffix(" 00:00:00").map(str::to_string);
                midnight(p).as_deref() == Some(q) || midnight(q).as_deref() == Some(p)
            }
        })
}

/// Transpile a batch of statements through `tests/emulate.py`, which speaks the same
/// record-separator protocol as the shells: `dialect \x1e statement \x1e …` in, one
/// `OK|…` / `ERR|…` block per statement out.
fn emulate(
    python: &str,
    harness: &str,
    dialect: &str,
    statements: &[String],
) -> Vec<Result<String, String>> {
    let mut script = dialect.to_string();
    for sql in statements {
        script.push('\u{1e}');
        script.push_str(sql);
    }
    let mut child = Command::new(python)
        .arg(harness)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(script.as_bytes())
        .expect("write request");
    let out = child.wait_with_output().expect("run python");
    let text = String::from_utf8_lossy(&out.stdout);
    text.split('\u{1e}')
        .map(|block| match block.split_once('|') {
            Some(("OK", sql)) => Ok(sql.to_string()),
            Some((_, message)) => Err(message.to_string()),
            None => Err(format!("no result for a statement: {block:?}")),
        })
        .collect()
}

/// Every construct we emit *conditionally* must be one the target can actually read.
///
/// The dialect tables — `supports_qualify`, `supports_filter`, `supports_full_join`,
/// `null_order`, the paging order, `concat_as_function` — are written by hand, and a
/// wrong entry produces SQL that looks right. `each_dialect_avoids_what_it_cannot_read`
/// catches the negative half by searching the text; this is the positive half, settled
/// by the engine: whatever we decided to emit for a target, run it there.
///
/// The assertion is deliberately not "the output contains X": a capability whose
/// spelling differs per target is *two* valid answers (`QUALIFY …` or a derived table,
/// `LIMIT … OFFSET …` or `OFFSET … FETCH`, `FILTER (WHERE …)` or `SUM(CASE WHEN …)`),
/// and pinning one of them would just re-record the table. What must hold is that
/// whichever one we chose runs.
#[test]
fn every_construct_we_emit_is_one_the_engine_reads() {
    // `(name, cagara query)` — one query per conditional rule, so the rule decides.
    const PROBES: &[(&str, &str)] = &[
        // A filter over a window: `QUALIFY` on four targets, a derived table on the
        // rest.
        (
            "qualify",
            "q = nums & select { id = .id, r = rowNumber { order = [asc .id] } } \
             & where (.r > 2) & select {.id} & order [asc .id]",
        ),
        // An aggregate with a filter: `FILTER (WHERE …)` where the target has it,
        // `SUM(CASE WHEN …)` where it does not.
        (
            "filter",
            "q = nums & agg { s = filtered (.a > 0) (sum .a) }",
        ),
        // A full join: refused on the MySQL family, emitted everywhere else.
        (
            "full_join",
            "q = nums & fullJoin (dupes & select { id2 = .id }) (.<id == .>id2) \
             & select { id = .id } & order [asc .id]",
        ),
        // Paging: `LIMIT … OFFSET …` everywhere except the Trino family, which needs
        // `OFFSET … FETCH FIRST`.
        (
            "paging",
            "q = nums & order [asc .id] & limit 2 & offset 1 & select {.id}",
        ),
        // NULL ordering: `NULLS LAST` where it is not the default, a leading `CASE`
        // key where there is no such clause.
        (
            "null_order",
            "q = nums & order [asc .v] & select { id = .id }",
        ),
        // Concatenation: `||` except on the targets where it is boolean OR.
        (
            "concat",
            "q = users & select { id = .id, k = .name ++ \"!\" }",
        ),
    ];
    let engines = [
        (
            Engine {
                shell: "sqlite3",
                args: &[
                    "-batch",
                    "-list",
                    "-noheader",
                    "-nullvalue",
                    "NULL",
                    ":memory:",
                ],
                dialect: "sqlite",
            },
            SETUP,
        ),
        (
            Engine {
                shell: "duckdb",
                args: &["-batch", "-list", "-noheader", "-nullvalue", "NULL"],
                dialect: "duckdb",
            },
            SETUP,
        ),
    ];
    let mut failures = Vec::new();
    let mut ran = 0;
    for (engine, setup) in &engines {
        let opts = Options {
            dialect: Dialect::from_str(engine.dialect).expect("dialect"),
            ..Options::default()
        };
        for (name, def) in PROBES {
            let ws = Workspace::from_source(&format!("{TABLES}{def}\n"));
            assert!(ws.diags.is_empty(), "`{name}`: {:?}", ws.diags);
            // `q`, not the first root: `TABLES` defines the tables as queries too, so
            // `next()` would hand back `nums` itself.
            let (_, rel) = root_queries(&ws)
                .into_iter()
                .find(|(n, _)| n == "q")
                .unwrap_or_else(|| panic!("`{name}`: no query `q`"));
            let rel = rel.unwrap_or_else(|d| panic!("`{name}`: {}", d.message));
            let Ok(sql) = compile(&rel, opts) else {
                // A refusal is a decision about the target — `fullJoin` on the MySQL
                // family — and nothing is emitted, so there is nothing to run.
                continue;
            };
            ran += 1;
            match run_setup(engine, setup, &sql) {
                Some(Ok(_)) => {}
                Some(Err(m)) => failures.push(format!(
                    "`{name}` on {}: the engine refused what we emitted\n  {sql}\n  {m}",
                    engine.dialect
                )),
                None => panic!("{} is not installed", engine.shell),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(ran >= 10, "only {ran} constructs ran");
}

/// Every correlated pattern is reachable with the join spellings, and the rows
/// prove it against a hand-written correlated subquery.
///
/// `docs/PLAN.md` listed "general correlated subqueries" as the largest remaining
/// change and the one with the least evidence that it is wanted. This is the
/// evidence, and it says the language is already expressive: each case is a Cagara
/// query, and beside it the SQL a user *would* have written with a correlated
/// subquery. The two must agree on the engine, so the claim is checked rather than
/// asserted.
///
/// The gap that is real is ergonomic, not semantic: `semiJoin` and `antiJoin` are
/// *stages*, so an `EXISTS` cannot be a column or sit inside `&&`, and every case
/// below needs an explicit aggregate on the joined side. Forgetting that aggregate
/// multiplies rows, which is the footgun — not a missing capability.
#[test]
fn the_join_spellings_match_a_correlated_subquery() {
    // `(name, cagara query, the same question as a correlated subquery)`.
    const PAIRS: &[(&str, &str, &str)] = &[
        // "does this user have an order" — the boolean column.
        (
            "exists_as_a_column",
            "q = users & leftJoin (orders & agg { user_id = group .user_id, n = count }) \
             (.<id == .>user_id) & select { id = .id, has = (.n ?? 0) > 0 } \
             & order [asc .id]",
            "SELECT id, (SELECT COUNT(*) FROM main.orders AS o WHERE o.user_id = u.id) > 0 AS has \
             FROM main.users AS u ORDER BY id",
        ),
        // "how many orders" — the correlated aggregate.
        (
            "count_per_row",
            "q = users & leftJoin (orders & agg { user_id = group .user_id, n = count }) \
             (.<id == .>user_id) & select { id = .id, n = .n ?? 0 } & order [asc .id]",
            "SELECT id, (SELECT COUNT(*) FROM main.orders AS o WHERE o.user_id = u.id) AS n \
             FROM main.users AS u ORDER BY id",
        ),
        // "each row against its group's maximum" — the self-correlated scalar.
        (
            "against_the_group_max",
            "q = orders & leftJoin (orders & agg { user_id = group .user_id, m = max .amount }) \
             (.<user_id == .>user_id) & select { id = .id, amount = .amount, m = .m } \
             & order [asc .id]",
            "SELECT id, amount, (SELECT MAX(amount) FROM main.orders AS o2 \
             WHERE o2.user_id = o.user_id) AS m FROM main.orders AS o ORDER BY id",
        ),
    ];
    let engine = Engine {
        shell: "duckdb",
        args: &["-batch", "-list", "-noheader", "-nullvalue", "NULL"],
        dialect: "duckdb",
    };
    let mut failures = Vec::new();
    for (name, def, correlated) in PAIRS {
        let ws = Workspace::from_source(&format!("{TABLES}{def}\n"));
        assert!(ws.diags.is_empty(), "`{name}`: {:?}", ws.diags);
        let (_, rel) = root_queries(&ws)
            .into_iter()
            .find(|(n, _)| n == "q")
            .unwrap_or_else(|| panic!("`{name}`: no query `q`"));
        let rel = rel.unwrap_or_else(|d| panic!("`{name}`: {}", d.message));
        let ours = compile(&rel, Options::default()).unwrap_or_else(|e| panic!("`{name}`: {e}"));
        let Some(Ok(ours_rows)) = run_setup(&engine, SETUP, &ours) else {
            panic!("`{name}`: `duckdb` refused our SQL or is not installed\n  {ours}");
        };
        let Some(Ok(their_rows)) = run_setup(&engine, SETUP, correlated) else {
            panic!("`{name}`: `duckdb` refused the correlated subquery\n  {correlated}");
        };
        // Two empty results would agree for the wrong reason.
        assert!(!ours_rows.is_empty(), "`{name}` returned no rows: {ours}");
        if ours_rows != their_rows {
            failures.push(format!(
                "`{name}`: the join spelling and the correlated subquery disagree\n  \
                 ours: {ours_rows:?}\n  theirs: {their_rows:?}\n  {ours}\n  {correlated}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Chaining two existence checks must not leave a dangling alias.
///
/// `exists p && exists q` is two `semiJoin`s in a row, which is how the plan's
/// "correlated subqueries" item is answered without a correlated subquery. The
/// second one's input is the first one's output, and that output is a stage whose
/// `FROM` already carries an alias — so re-aliasing it produced
/// `SELECT t1.id FROM main.users AS t3`, which DuckDB refuses with
/// `Referenced table "t1" not found`. `qualify` only rewrites *unqualified*
/// columns, so the alias has to be kept, not replaced.
#[test]
fn chained_existence_checks_keep_their_alias() {
    const DEFS: &str = "\
q_and = users & semiJoin orders (.<id == .>user_id) \
        & semiJoin orders (.<id == .>user_id && .>amount > 4.5) & select {.id} & order [asc .id]
q_or = users & semiJoin orders (.<id == .>user_id) \
       & union (users & semiJoin orders (.<id == .>user_id && .>amount > 4.5)) \
       & select {.id} & order [asc .id]
q_not = users & antiJoin orders (.<id == .>user_id) & select {.id} & order [asc .id]
";
    let engine = Engine {
        shell: "duckdb",
        args: &["-batch", "-list", "-noheader", "-nullvalue", "NULL"],
        dialect: "duckdb",
    };
    // `users` is ann(1), bob(2), "it's a\b "(3); `orders` has 1→5.5, 1→4.5, 3→1.0.
    // So: an order over 4.5 is user 1 only, and the three answers are distinct.
    for (name, want) in [
        ("q_and", vec!["1".to_string()]),
        ("q_or", vec!["1".to_string(), "3".to_string()]),
        ("q_not", vec!["2".to_string()]),
    ] {
        let ws = Workspace::from_source(&format!("{TABLES}{DEFS}"));
        assert!(ws.diags.is_empty(), "{:?}", ws.diags);
        let (_, rel) = root_queries(&ws)
            .into_iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no query `{name}`"));
        let rel = rel.unwrap_or_else(|d| panic!("`{name}`: {}", d.message));
        let sql = compile(&rel, Options::default()).unwrap_or_else(|e| panic!("`{name}`: {e}"));
        match run_setup(&engine, SETUP, &sql) {
            Some(Ok(rows)) => assert_eq!(rows, want, "`{name}`: {sql}"),
            Some(Err(m)) => panic!("`{name}`: {m}\n  {sql}"),
            None => panic!("duckdb is not installed"),
        }
    }
}

/// `markExists` is `EXISTS` as a value, so it composes where `semiJoin` cannot.
///
/// `semiJoin` is a stage: its answer is which rows survive. That is the right tool
/// when the answer *is* the question, and the wrong one when it is one column among
/// others — `select { has = .exists }` — or when it has to be `||`-ed with a
/// condition that is not an existence test. `markExists` is the same `EXISTS`, in a
/// column.
#[test]
fn mark_exists_is_exists_as_a_value() {
    const DEFS: &str = "\
q_column = users & markExists orders (.<id == .>user_id) \
           & select { id = .id, has = .exists } & order [asc .id]
q_or = users & markExists orders (.<id == .>user_id) \
       & where (.exists || .id > 2) & select {.id} & order [asc .id]
q_and = users & markExists orders (.<id == .>user_id && .>amount > 4.5) \
        & where .exists & select {.id} & order [asc .id]
";
    // `users` is ann(1), bob(2), "it's a\b "(3); `orders` has 1→5.5, 1→4.5, 3→1.0.
    // So only user 1 has an order over 4.5, and `exists || id > 2` is users 1 and 3.
    let engine = Engine {
        shell: "duckdb",
        args: &["-batch", "-list", "-noheader", "-nullvalue", "NULL"],
        dialect: "duckdb",
    };
    let ws = Workspace::from_source(&format!("{TABLES}{DEFS}"));
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    // DuckDB prints a boolean as `true`; `same_row` folds that to `1`, because
    // every engine prints a bool its own way and this compares values.
    for (name, want) in [
        (
            "q_column",
            vec!["1|1".to_string(), "2|0".into(), "3|1".into()],
        ),
        ("q_or", vec!["1".to_string(), "3".into()]),
        ("q_and", vec!["1".to_string()]),
    ] {
        let (_, rel) = root_queries(&ws)
            .into_iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no query `{name}`"));
        let rel = rel.unwrap_or_else(|d| panic!("`{name}`: {}", d.message));
        let sql = compile(&rel, Options::default()).unwrap_or_else(|e| panic!("`{name}`: {e}"));
        match run_setup(&engine, SETUP, &sql) {
            Some(Ok(rows)) => {
                assert_eq!(
                    rows.len(),
                    want.len(),
                    "`{name}`: {rows:?} against {want:?}"
                );
                assert!(
                    rows.iter().zip(&want).all(|(a, b)| same_row(a, b)),
                    "`{name}`: {rows:?} against {want:?}\n  {sql}"
                );
            }
            Some(Err(m)) => panic!("`{name}`: {m}\n  {sql}"),
            None => panic!("duckdb is not installed"),
        }
    }
}

/// A decimal sum really does need a wider precision — which is why `+` does not
/// widen today, and what a widening would have to be right about.
///
/// `DECIMAL(10,2)` holds `99999999.99`; its sum with itself is `199999999.98`, which
/// does not fit. Both engines answer with a one-digit-wider type, and casting that sum
/// back is an *error*, not a rounding:
///
/// ```text
/// Conversion Error: Casting value "199999999.98" to type DECIMAL(10,2) failed:
/// value is out of range!
/// ```
///
/// So the same-type rule is a real limitation and not a formality. This test pins the
/// engine behaviour that makes it one, so a future widening has the measurement to
/// check itself against — see `docs/PLAN.md` for what blocked it.
#[test]
fn a_decimal_sum_outgrows_its_operand_type() {
    let duck = Engine {
        shell: "duckdb",
        args: &["-batch", "-list", "-noheader", "-nullvalue", "NULL"],
        dialect: "duckdb",
    };
    let setup = "CREATE TABLE t (v DECIMAL(10, 2));\n\
                 INSERT INTO t VALUES (99999999.99);\n";
    // The sum, as the engine types it.
    match run_setup(&duck, setup, "SELECT typeof(v + v) AS t FROM t") {
        Some(Ok(rows)) => assert_eq!(rows, vec!["DECIMAL(11,2)".to_string()]),
        Some(Err(m)) => panic!("duckdb: {m}"),
        None => panic!("duckdb is not installed"),
    }
    // And the value, which the declared type cannot hold.
    match run_setup(&duck, setup, "SELECT v + v AS s FROM t") {
        Some(Ok(rows)) => assert_eq!(rows, vec!["199999999.98".to_string()]),
        Some(Err(m)) => panic!("duckdb: {m}"),
        None => panic!("duckdb is not installed"),
    }
    // Casting it back to the operand type is the error, and it is an error rather
    // than a silent wrap: that is what makes the wider type necessary.
    let narrowed = run_setup(
        &duck,
        setup,
        "SELECT CAST(v + v AS DECIMAL(10, 2)) AS s FROM t",
    );
    match narrowed {
        Some(Ok(rows)) => panic!("duckdb accepted an out-of-range cast: {rows:?}"),
        Some(Err(_)) => {}
        None => panic!("duckdb is not installed"),
    }
}

/// Postgres, through PGlite — Postgres compiled to WebAssembly and driven by
/// `node`, so no server is needed (`crates/cagara-sql/tests/pglite.mjs`).
///
/// The shared cases and `examples/report.cagara` run here as they do on the
/// other engines, plus the `list` cases, which no other engine in this file can
/// hold: PostgreSQL is where `ARRAY[…]`, `ARRAY_AGG` and `||` are checked by a
/// real planner rather than against the text we expect.
#[test]
fn postgres_agrees_via_pglite() {
    let harness = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pglite.mjs");
    let ready = Command::new("node")
        .args([harness, "--check"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ready {
        assert!(
            std::env::var_os("CAGARA_REQUIRE_ENGINES").is_none(),
            "`node` with @electric-sql/pglite is not installed (CAGARA_REQUIRE_ENGINES is set)"
        );
        eprintln!("skipping postgres: node or @electric-sql/pglite is not installed");
        return;
    }
    let postgres = Options {
        dialect: Dialect::Postgres,
        ..Options::default()
    };
    let mut failures: Vec<String> = Vec::new();
    let mut ran = 0;

    // The shared cases and the three `list` cases, on one database.
    let list_src = format!(
        "{TABLES}\
         l1 = nums & select {{ id = .id, xs = [10, 20] }}\n\
         l2 = nums & select {{ id = .id, both = [1] <> [2] }}\n\
         l3 = nums & where (.id == 1) & agg {{ xs = arrayAgg .id }}\n\
         l4 = nums & where (.id == 1) & agg {{ xs = arrayAgg .id }} \
              & select {{ n = listLength .xs, has = listContains .xs 1 }}\n\
         l5 = nums & agg {{ xs = arrayAgg .id }} & unnest .xs & select {{ id = .value }}\n"
    );
    let shared = format!(
        "{TABLES}{}\n{}",
        CASES.iter().map(|(d, _)| *d).collect::<Vec<_>>().join("\n"),
        list_src.strip_prefix(TABLES).expect("prefix")
    );
    let ws = Workspace::from_source(&shared);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);
    let list_cases: &[(&str, &[&str], bool)] = &[
        (
            "l1",
            &["1|{10,20}", "2|{10,20}", "3|{10,20}", "4|{10,20}"],
            false,
        ),
        ("l2", &["1|{1,2}", "2|{1,2}", "3|{1,2}", "4|{1,2}"], false),
        ("l3", &["{1}"], false),
    ];
    let wanted: Vec<(&str, &[&str], bool)> = CASES
        .iter()
        .map(|(d, w)| (d.split_whitespace().next().expect("name"), *w, false))
        .chain(list_cases.iter().copied())
        .collect();
    let mut queries = Vec::with_capacity(wanted.len());
    for (name, _, _) in &wanted {
        let (_, rel) = rels
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no query `{name}`"));
        let rel = rel
            .as_ref()
            .unwrap_or_else(|d| panic!("`{name}`: {}", d.message));
        queries.push(compile(rel, postgres).unwrap_or_else(|m| panic!("`{name}`: {m}")));
    }
    let blocks = pg_batch(harness, &pg_setup(SETUP), &queries);
    for ((name, want, unordered), rows) in wanted.iter().zip(&blocks) {
        ran += 1;
        let mut got = rows.clone();
        let mut want: Vec<&str> = want.to_vec();
        if *unordered {
            got.sort();
            want.sort();
        }
        if got.len() != want.len() || !got.iter().zip(&want).all(|(a, b)| same_row(a, b)) {
            failures.push(format!("`{name}` on postgres: got {got:?}, want {want:?}"));
        }
    }

    // `examples/report.cagara`, whose tables are `public.` — which Postgres has.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/report.cagara");
    let source = std::fs::read_to_string(path).expect("read examples/report.cagara");
    let ws = Workspace::from_source(&source);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let rels = root_queries(&ws);
    let mut queries = Vec::with_capacity(REPORT_CASES.len());
    for (name, _, _) in REPORT_CASES {
        let (_, rel) = rels
            .iter()
            .find(|(n, _)| n == *name)
            .unwrap_or_else(|| panic!("report.cagara has no `{name}`"));
        let rel = rel
            .as_ref()
            .unwrap_or_else(|d| panic!("`{name}`: {}", d.message));
        queries.push(compile(rel, postgres).unwrap_or_else(|m| panic!("`{name}`: {m}")));
    }
    let setup = REPORT_SETUP_DUCKDB.replace("DOUBLE", "DOUBLE PRECISION");
    let blocks = pg_batch(harness, &setup, &queries);
    for ((name, want, unordered), rows) in REPORT_CASES.iter().zip(&blocks) {
        ran += 1;
        let mut got = rows.clone();
        let mut want: Vec<&str> = want.to_vec();
        if *unordered {
            got.sort();
            want.sort();
        }
        if got.len() != want.len() || !got.iter().zip(&want).all(|(a, b)| same_row(a, b)) {
            failures.push(format!(
                "report `{name}` on postgres: got {got:?}, want {want:?}"
            ));
        }
    }

    assert!(ran > 0, "no Postgres query ran");
    assert!(
        failures.is_empty(),
        "{} of {ran} Postgres queries disagreed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
