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
";

const TABLES: &str = "\
nums : query { id = int, a = int, b = int, v = maybe int } = table \"main\" \"nums\"
users : query { id = int, name = string } = table \"main\" \"users\"
orders : query { id = int, user_id = int, amount = float } = table \"main\" \"orders\"
days : query { id = int, d = date } = table \"main\" \"days\"
kw : query { id = int, order = int, userId = int } = table \"main\" \"select\"
empty : query { id = int } = table \"main\" \"empty\"
dupes : query { id = int, k = string } = table \"main\" \"dupes\"
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
                let sql =
                    compile(rel, opts).unwrap_or_else(|m| panic!("`{name}` / {}: {m}", e.dialect));
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
    let (x, y): (Vec<&str>, Vec<&str>) = (a.split('|').collect(), b.split('|').collect());
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
