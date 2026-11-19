# SQL dialect support

Cagara builds a SQL AST and asks `sqlglot-rust` to rewrite and validate it for
the selected dialect. A dialect accepted by the library is a code-generation
target; that alone does not mean Cagara's custom date and string operations
have been checked against that database engine.

| Coverage | Dialects |
|---|---|
| A real server runs the query and the rows match | SQLite 3.46+, DuckDB, PostgreSQL (through [PGlite](https://pglite.dev/), Postgres compiled to WASM and driven by `node`), Trino, MariaDB |
| Translated to DuckDB by Python sqlglot, then run | ANSI, BigQuery, Snowflake, T-SQL |
| Text assertions only — the spelling is intended, nothing more is known | Spark, and the aliases: Presto/Athena, Hive/Databricks, Doris/SingleStore/StarRocks, Fabric, ClickHouse, Teradata, Redshift, Oracle |
| Cagara-specific date, string, and integer arithmetic rewrites | ANSI/Postgres fallback; MySQL family (MySQL, Doris, SingleStore, StarRocks); SQLite; DuckDB; T-SQL/Fabric; BigQuery; Snowflake; Trino/Presto/Athena; Spark/Databricks |
| SQLGlot Rust can recognize and validate | Other dialects exposed by the pinned `sqlglot-rust` version; their Cagara-specific intrinsic spelling uses the ANSI/Postgres fallback unless listed above |

The first three rows are the `VERIFIED` table in
[`crates/cagara-sql/tests/engines.rs`](../crates/cagara-sql/tests/engines.rs), which a
test keeps in step with the dialects the crate accepts: a new target has to say which of
the three it is. The distinction matters — an emulation proves the SQL *means* the same
thing, not that the target *accepts* it. MariaDB rejects a `GROUPS` frame outright and
refuses a `range` frame with two sort keys, and no amount of translation could have shown
either; see [`ENGINE-TESTS.md`](ENGINE-TESTS.md).

## The capability table

Whether a target can say a construct is **data**, not a family test. `sqlglot-rust`
groups dialects by grammar, and capabilities do not follow grammar: T-SQL shares no
family with MySQL, yet neither has a `GROUPS` window frame, and Doris shares MySQL's
family yet the two are classified separately. So every question of the form "does this
dialect have X" goes through one table in
[`crates/cagara-sql/src/dialect.rs`](../crates/cagara-sql/src/dialect.rs), which answers
with one of four values:

| answer | meaning | what the compiler does |
|---|---|---|
| `Native` | the target has it | writes it out |
| `Emulated` | the target lacks it, an equivalent exists | rewrites it (see [Rewritten instead of refused](#rewritten-instead-of-refused)) |
| `Unverified` | the compiler writes it and no run has shown the target accepts it | writes it out — refusing would be indistinguishable from a real refusal — and this file says so |
| `Unsupported` | the target lacks it and there is no equivalent | refuses, naming what to write instead |

The table matches on `Dialect` exhaustively, with no wildcard arm, so a dialect added
to `sqlglot-rust` is a compile error here rather than a new target that silently
inherits whichever family happened to claim it. That is not hypothetical: the
`GROUPS` bug was `fam(d) != Fam::Mysql`, which accepted T-SQL and emitted
`GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW` for a server whose frame syntax is
`{ ROWS | RANGE }`.

| capability | native | emulated | unsupported |
|---|---|---|---|
| `FULL [OUTER] JOIN` | everything else | MySQL, Doris, SingleStore, StarRocks | — |
| `ROLLUP` / `CUBE` | everything else | SQLite, MySQL, Doris, SingleStore, StarRocks | — |
| `GROUPS` frame | ANSI, PostgreSQL, DuckDB, SQLite, Trino, Presto, Athena, Oracle, Spark, Databricks | — | MySQL family, T-SQL, Fabric, BigQuery, Snowflake, Redshift, Teradata, Hive |
| `RANGE` frame | every target whose `NULLS LAST` is native | — | every target that emulates `NULLS LAST` with a second sort key |
| `QUALIFY` | DuckDB, Snowflake, BigQuery, Teradata | everywhere else (a derived table) | — |
| `FILTER (WHERE …)` | ANSI, PostgreSQL, DuckDB, SQLite, Trino, Presto, Athena, Snowflake, Redshift | everywhere else (`SUM(CASE WHEN …)`) | — |
| `list` / arrays | ANSI, PostgreSQL, DuckDB | — | everywhere else |
| exact `DECIMAL(p, s)` | everything else | — | SQLite |

The `RANGE` row is derived rather than listed twice: a dialect needs the second sort
key exactly when its entry in `null_order` says there is no `NULLS LAST` syntax, so
`supports_range_frame` reads that entry instead of keeping a second list that could
disagree with it.

`Unverified` is not a fifth kind of support. It is the honest name for "the spelling
is what this compiler intends and nothing more is known", and the coverage table at the
top of this file is where those targets are listed.

Rust unit tests check generated SQL for selected constructs across these
dialects. Those tests catch regressions in the emitted text, while the differential
tests check query results for the cases in
[`crates/cagara-sql/tests/engines.rs`](../crates/cagara-sql/tests/engines.rs).

CI runs the SQLite and DuckDB half of that suite on every push, from pinned
versions of both shells; the other three engines (PostgreSQL through PGlite, Trino,
MariaDB) are a local run. `CAGARA_ENGINES` picks the engines a run may use, so the
CI job requires the two it installs without failing on the three it does not — see
[`ENGINE-TESTS.md`](ENGINE-TESTS.md).

`list` is the one value type that is not portable. `arrayAgg`, a list literal, `<>`, `listLength`,
`listContains` and `unnest` are spelled for PostgreSQL (`ARRAY[…]`, `ARRAY_AGG`, `||`,
`cardinality`, `x = ANY(xs)`, a `LATERAL` subquery over `UNNEST`) and DuckDB (`list_value`, `list`,
`list_concat`, `len`, `list_contains`, the same `LATERAL`); every other dialect refuses the query at
compile time with a message naming what needs an array. A list is never lowered to JSON text — that would change the column's type without
saying so.

Filtered aggregates are `FILTER (WHERE …)` where the target has the clause —
the ANSI family, SQLite, DuckDB, PostgreSQL, Trino and Snowflake — and the
portable `SUM(CASE WHEN p THEN x END)` where it does not (the MySQL family,
T-SQL, BigQuery, Spark). The choice is made in the dialect pass, from the `Expr`
the aggregate already lowered to, so the two spellings cannot drift apart.

A **format conversion** is the same idea in one place: `parseDate`/`formatDate`
write the format once, in strftime's vocabulary (`%Y %m %d %H %M %S`, `%%`), and
the backend spells both the function and the format per family —

| family | parse | format | format style |
|---|---|---|---|
| PostgreSQL, ANSI, Snowflake | `TO_DATE(s, fmt)` / `TO_TIMESTAMP(s, fmt)` | `TO_CHAR(d, fmt)` | `YYYY MM DD HH24 MI SS` |
| DuckDB, BigQuery | `STRPTIME` / `PARSE_DATE` | `STRFTIME` / `FORMAT_DATE` | `%Y %m %d %H %M %S` |
| Trino, MySQL | `DATE_PARSE` / `STR_TO_DATE` | `DATE_FORMAT` | `%Y %m %d %H %i %s` |
| Spark | `TO_DATE(s, fmt)` | `DATE_FORMAT` | `yyyy MM dd HH mm ss` |
| SQLite, T-SQL | `DATE()`/`CAST` on a rebuilt ISO string | `STRFTIME` / `FORMAT` | `%Y…` / `yyyy…` |

The middle column differs for a reason worth stating: MySQL's format language
(which Trino's `date_parse` follows) reads `%i` for minutes and `%M` for the
month's *name*, so passing a C-style format through unchanged would silently
parse a minute as a month name. Translating is what makes one `formatDate`
mean one thing everywhere. A specifier the language does not define is a compile
error, and the format must be a literal — it is compiled in.

SQLite and T-SQL have no format *parser* (SQLite reads ISO strings only, T-SQL's
`PARSE` is culture-driven) — and the language does not follow them, because a function
that compiles on one `--dialect` and not another makes the language's surface depend on
the target. The format is a compile-time literal, so it is **decomposed** instead of
refused:

- **Parsing** rebuilds the ISO string from fixed-width slices of the input. `%Y%m%d`
  becomes `SUBSTR(s,1,4) || '-' || SUBSTR(s,5,2) || '-' || SUBSTR(s,7,2)`, handed to
  `DATE()`/`DATETIME()` on SQLite and to `CAST(… AS DATE)` on T-SQL. That covers the
  numeric vocabulary, which is what a machine-written date uses; a *name* has no fixed
  width and is the one thing still refused, with a message saying so.
- **Formatting** on SQLite concatenates literal text with one single-specifier
  `strftime` per numeric field, and a `CASE` per name — SQLite's `strftime` has no
  `%b %B %a %A` and answers NULL rather than failing, so `monthName`/`dayName`/
  `monthAbbrev`/`dayAbbrev` are a `CASE` over the month or weekday number. Every other
  target prints the name directly.

PostgreSQL's `TO_CHAR` is the other direction of the same problem: `Month`/`Day`
blank-pad to nine characters, so the translation uses the unpadded `FMMonth`/`FMDay` —
otherwise `formatDate "%B %d"` would be `February  29` on PostgreSQL alone.

Other functions that hide an engine rule behind one spelling, none of which the language
follows:

| function | the divergence | what the language does |
|---|---|---|
| `like` | SQLite's `LIKE` is case-insensitive for ASCII unless the connection sets `PRAGMA case_sensitive_like` | a `GLOB` with the pattern translated (`%`→`*`, `_`→`?`, literal `*`/`?`/`[` escaped), because `GLOB` is the case-sensitive matcher SQLite has |
| `lpad`, `rpad` | SQLite has neither `LPAD` nor `REPEAT` | `HEX(ZEROBLOB(n))` is `0` repeated `2n` times, so replacing each `0` with the fill gives the padding; `SUBSTR` takes the missing part |
| `random` | SQLite's `random()` is a signed 64-bit integer, so dividing by `2^63` is negative half the time | `RANDOM() & 2147483647` masked and scaled, which is exactly `[0, 1)` |
| `reverse` | SQLite has no `REVERSE` | a scalar subquery carrying its own `WITH RECURSIVE`, walking the characters from the end |
| `try*` | SQLite's `CAST('abc' AS INT)` is `0`, not an error; PostgreSQL has no `TRY_CAST` and raises | a validation before the cast — `TRIM(x,'0123456789')` on SQLite, a regex on PostgreSQL |
| `listAt` | PostgreSQL needs `(ARRAY_AGG(id))[2]`, and unparenthesised `[2]` is a syntax error there | the subscript is built around a parenthesised inner expression |

## Rewritten instead of refused

A construct the target cannot spell is **rewritten into equivalent SQL** wherever an
equivalent exists, so the language does not change with `--dialect`. Two are:

- `fullJoin` on the MySQL family. MySQL has only `LEFT`/`RIGHT` joins, so the join
  becomes `(l LEFT JOIN r ON p) UNION ALL (r` with no match`)`, and the second branch
  is an anti-join on the same predicate with the sides exchanged rather than
  `RIGHT JOIN … WHERE <some left column> IS NULL`. The anti-join is what makes it
  exact. The `IS NULL` test has to name a column, and is only correct if that column
  is non-NULL on every matched row — which no general predicate promises. Measured on
  SQLite with a matched pair whose left `name` is NULL, the `IS NULL` form returned
  that row twice; the anti-join returned it once, exactly as `FULL JOIN` did.
- `rollup`/`cube` on SQLite and the MySQL family. Grouping sets are a family of key
  lists — `ROLLUP(a, b)` is `{(a, b), (a), ()}` — so the rewrite enumerates them: one
  `agg` per set over the same input, `UNION ALL`ed, with the excluded keys written as
  NULL. Two shapes in one clause are their cross product, which is what the clause
  means. MySQL's `WITH ROLLUP` is *not* used: it rolls up the whole key list and
  cannot be combined with `ORDER BY`.

Both rewrites are in [`crates/cagara-sql/src/emulate.rs`](../crates/cagara-sql/src/emulate.rs)
and produce ordinary relational algebra, so the lowerer sees nothing new.

**Nothing is refused.** Every construct Cagara can write compiles for every target,
because a function that works on one `--dialect` and not another makes the language's
surface depend on the target — which is the thing this whole file exists to prevent.
Where a target cannot spell something directly, the spelling is rewritten:

- An empty `inList []` lowers to a constant `FALSE` (or `TRUE` when negated), because
  SQL has no empty `IN ()` anywhere.
- A `decimal` on SQLite, which has no exact numeric type, is the one construct still
  refused rather than rewritten — a binary float wearing the type's name would be the
  exact failure the type exists to prevent, and there is no fixed-point representation to
  emit instead. That refusal names the target and the alternative.

The remaining gap in the *emulation* is a format *name* in `parseDate` on SQLite and
T-SQL, where the decomposition has no width to slice; that is a limitation of the
technique, stated in the message, rather than a target being unable.

`toInt`, `toFloat`, `tryInt`, and `tryFloat` cast to the language's widths
rather than the engine's default `INT`/`FLOAT`: the integer target is `BIGINT`
and the float target is a 64-bit double in the target's spelling
(`DOUBLE PRECISION`, `DOUBLE`, `FLOAT`, or `FLOAT64`). `toInt` of a float also
truncates toward zero in every dialect, rather than inheriting PostgreSQL or
MySQL rounding from a plain cast.

When targeting another engine, inspect the generated SQL and run representative
queries against that engine, especially if they use dates, string functions,
integer division, null ordering, or T-SQL-specific boolean handling. Add a
dialect rewrite and an expected-output or engine test when Cagara needs a
different intrinsic spelling or semantic adjustment.

## Why the date and string intrinsics are spelled in Rust

It is tempting to write the prelude's date and string operations as ANSI SQL
and let `sqlglot-rust` transpile them. The pinned version (0.11.2, the newest
published) cannot do that, and feeding it dialect-specific SQL text is actively
unsafe:

- **The translations are missing.** `DATE_TRUNC('DAY', x)`, `EXTRACT(YEAR FROM
  x)`, `x + 3 * INTERVAL '1' DAY`, `LEFT` / `RIGHT`, and `STRPOS(s, 'a')` all
  pass through unchanged to MySQL and SQLite, which have no `DATE_TRUNC`,
  `INTERVAL` arithmetic, or `STRPOS`.
- **The typed date functions are only partly covered.** sqlglot parses
  `DATE_TRUNC` and `DATE_ADD` into typed nodes and regenerates them; the
  generator special-cases T-SQL and Oracle only, so MySQL and SQLite still
  receive `DATE_TRUNC('DAY', x)`.
- **Re-parsing corrupts correct SQL.** A `sql` template is parsed by sqlglot
  and then rewritten for the target dialect, so dialect-specific text does not
  survive: `DATE_ADD(x, INTERVAL 3 DAY)` becomes
  `DATEADD(DAY, INTERVAL 3 DAY, x)` for T-SQL and
  `DATE_ADD(x, INTERVAL INTERVAL 3 DAY DAY)` for BigQuery, and
  `TIMESTAMPADD(DAY, 3, x)` becomes `DATE_ADD(DAY, 3)`. SQLGlot has no raw
  expression node that would let such text through untouched.
- **Parsing can also *drop* meaning, silently.** sqlglot reads
  `TRY_CAST(x AS INT)` as a plain `Cast`, so a template spelling of a try-cast
  becomes a hard cast that fails on the first unparseable value instead of
  yielding NULL. The prelude's `try*` functions therefore call a `CAGARA_*`
  intrinsic, which builds `Expr::TryCast` directly.

  Two targets do not have the clause at all, and both are known gaps rather than
  emulated ones:

  - **SQLite** has no `TRY_CAST`, and `CAST('abc' AS INT)` there is `0` rather
    than an error, so no guard can turn a failed cast into NULL without also
    turning every other unparseable value into a silent `0`. Emitting the node
    as-is lets the engine reject it loudly, which is the honest outcome.
  - **PostgreSQL** has no `TRY_CAST` either — `TRY_CAST(x AS DATE)` is a syntax
    error there, measured on PGlite, not a NULL — so a `try*` cast on
    `--dialect postgres` is a **compile error** that names the two ways out:
    `CASE WHEN pg_input_is_valid(x, 'date') THEN CAST(x AS DATE) END`
    (PostgreSQL 16+), or a `to*` cast when the input is known to be shaped. The
    generic `ansi` target keeps `TRY_CAST` as the loose spelling.

So the prelude names a `CAGARA_*` function and
[`crates/cagara-sql/src/intrinsics.rs`](../crates/cagara-sql/src/intrinsics.rs)
builds the target's expression tree directly, per dialect family. A
`CAGARA_*` call the backend does not recognize is a compile error rather than
SQL that the engine would reject.

