# SQL dialect support

Cagara builds a SQL AST and asks `sqlglot-rust` to rewrite and validate it for
the selected dialect. A dialect accepted by the library is a code-generation
target; that alone does not mean Cagara's custom date and string operations
have been checked against that database engine.

| Coverage | Dialects |
|---|---|
| Executed against real engines in CI, under both lowerings | SQLite 3.46+, DuckDB, PostgreSQL (through [PGlite](https://pglite.dev/), Postgres compiled to WASM and driven by `node`) |
| Cagara-specific date, string, and integer arithmetic rewrites | ANSI/Postgres fallback; MySQL family (MySQL, Doris, SingleStore, StarRocks); SQLite; DuckDB; T-SQL/Fabric; BigQuery; Snowflake; Trino/Presto/Athena; Spark/Databricks |
| SQLGlot Rust can recognize and validate | Other dialects exposed by the pinned `sqlglot-rust` version; their Cagara-specific intrinsic spelling uses the ANSI/Postgres fallback unless listed above |

Rust unit tests check generated SQL for selected constructs across these
dialects. Those tests catch regressions in the emitted text, while the
SQLite/DuckDB differential test checks query results for the cases in
[`crates/cagara-sql/tests/engines.rs`](../crates/cagara-sql/tests/engines.rs).
The live-engine suite is required in CI and can be skipped locally when the
shells are unavailable.

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
| SQLite, T-SQL | *refused* | `STRFTIME` / `FORMAT` | `%Y…` / `yyyy…` |

The middle column differs for a reason worth stating: MySQL's format language
(which Trino's `date_parse` follows) reads `%i` for minutes and `%M` for the
month's *name*, so passing a C-style format through unchanged would silently
parse a minute as a month name. Translating is what makes one `formatDate`
mean one thing everywhere. A specifier the language does not define is a compile
error, and the format must be a literal — it is compiled in.

SQLite and T-SQL have no format *parser* (SQLite reads ISO strings only, T-SQL's
`PARSE` is culture-driven), so `parseDate` is refused there; both can still print
by format.

Two constructs are deliberately refused rather than emitted in a dialect that
cannot mean them:

- `fullJoin` is an error for the MySQL family, which has no
  `FULL [OUTER] JOIN`. Rewriting it would require deciding which predicate
  null-extends which side; the portable workaround is
  `leftJoin`/`rightJoin` plus `unionAll`.
- An empty `inList []` lowers to a constant `FALSE` (or `TRUE` when negated),
  because SQL has no empty `IN ()`. The target never sees an empty list.

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
and let `sqlglot-rust` transpile them. The pinned version (0.10.30, the newest
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

