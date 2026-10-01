//! Differential tests: the same Cagara queries, compiled per dialect and run
//! on real engines, must return the same rows. Each engine runs through its
//! command-line shell (`sqlite3` 3.46 or later, `duckdb`) and is skipped
//! when that is not installed, unless `CAGARA_REQUIRE_ENGINES` is set (as in
//! CI); `nix develop` provides both.

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
        "nasc = nums & select { id = .id, v = .v } & order [asc .v]",
        &["1|10", "4|20", "3|30", "2|NULL"],
    ),
    (
        "ndesc = nums & select { id = .id, v = .v } & order [desc .v]",
        &["3|30", "4|20", "1|10", "2|NULL"],
    ),
    (
        "win = nums & select { id = .id, r = rowNumber { order = [asc .v] } } & order [asc .id]",
        &["1|1", "2|4", "3|3", "4|2"],
    ),
    // The order survives the derived table that the filter on a window
    // needs, even though its key is no longer a column.
    (
        "kept = nums & order [desc .v] & select { id = .id, rn = rowNumber { order = [asc .id] } } & where (.rn <= 3)",
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
        "ljagg = users & leftJoin orders (.<id == .>user_id) & agg { id = group .id, total = coalesce 0.0 (sum (coalesce 0.0 .amount)), n = coalesce 0 (sum (ifThenElse (isNotNull .user_id) 1 0)) } & order [asc .id]",
        &["1|10.0|2", "2|0.0|0", "3|1.0|1"],
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
        "rj = orders & rightJoin (users & where (.id > 1)) (.<user_id == .>id) & select { oid = .id, amount = .amount, n = .name } & order [asc .n, asc .oid]",
        &["NULL|NULL|bob", "12|1.0|it's a\\b "],
    ),
    // Hoisting a filter to WHERE would drop the null-extended rows. For a
    // right join the *left* input is null-extended, so filtering it must
    // still leave every right row: users 2 and 3 have no matching order and
    // must survive as NULL.
    (
        "rjonleft = (orders & where (.amount > 4.5)) & rightJoin users (.<user_id == .>id) & select { oid = .id, amount = .amount, n = .name } & order [asc .n, asc .oid]",
        &[
            "10|5.5|ann",
            "NULL|NULL|bob",
            "NULL|NULL|it's a\\b ",
        ],
    ),
    // A full join preserves both sides, so a filter on either input keeps a
    // derived table.
    (
        "fj = orders & fullJoin (users & where (.id > 2)) (.<user_id == .>id) & select { oid = .id, amount = .amount, n = .name } & order [asc .n, asc .oid]",
        &[
            "12|1.0|it's a\\b ",
            "10|5.5|NULL",
            "11|4.5|NULL",
        ],
    ),
    (
        "fj2 = (orders & where (.amount > 4.5)) & fullJoin (users & where (.id > 2)) (.<user_id == .>id) & select { oid = .id, amount = .amount, n = .name } & order [asc .n, asc .oid]",
        &["NULL|NULL|it's a\\b ", "10|5.5|NULL"],
    ),
    // The mirror case on the other side: a left join filtered on its
    // preserved (left) input, and one filtered on its null-extended input.
    (
        "ljonleft = (users & where (.id > 2)) & leftJoin orders (.<id == .>user_id) & select { n = .name, amount = .amount } & order [asc .n]",
        &["it's a\\b |1.0"],
    ),
    (
        "ljonright = users & leftJoin (orders & where (.amount > 4.5)) (.<id == .>user_id) & select { n = .name, amount = .amount } & order [asc .n, asc .amount]",
        &["ann|5.5", "bob|NULL", "it's a\\b |NULL"],
    ),
    // An inner join is the exception: both sides may hoist their filters to
    // WHERE, since the filter only decides which rows match. No order is
    // placed by a user above 1, so this returns nothing.
    (
        "ij = (orders & where (.amount > 4.5)) & inner (users & where (.id > 1)) (.<user_id == .>id) & select { oid = .id, n = .name } & order [asc .n]",
        &[],
    ),
    // The same filters without excluding the matching user, so the inner
    // join does return rows.
    (
        "ij2 = (orders & where (.amount > 4.5)) & inner (users & where (.id > 0)) (.<user_id == .>id) & select { oid = .id, n = .name } & order [asc .oid]",
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

/// Rows of the last statement, or `None` when the shell is not installed.
fn run(e: &Engine, sql: &str) -> Option<Result<Vec<String>, String>> {
    let mut child = match Command::new(e.shell)
        .args(e.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return None,
    };
    let script = format!("{SETUP}{sql};\n");
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
        let opts = Options {
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
            let sql =
                compile(rel, opts).unwrap_or_else(|m| panic!("`{name}` / {}: {m}", e.dialect));
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
                Some(Err(m)) => failures.push(format!("`{name}` on {}: {m}", e.shell)),
                Some(Ok(rows)) => {
                    ran += 1;
                    if rows != *want {
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
    eprintln!("{ran} engine runs");
}
