# SQL dialect support

Cagara builds a SQL AST and asks `sqlglot-rust` to rewrite and validate it for
the selected dialect. A dialect accepted by the library is a code-generation
target; that alone does not mean Cagara's custom date and string operations
have been checked against that database engine.

| Coverage | Dialects |
|---|---|
| Executed against real engines in CI | SQLite 3.46+, DuckDB |
| Cagara-specific date, string, and integer arithmetic rewrites | ANSI/Postgres fallback; MySQL family (MySQL, Doris, SingleStore, StarRocks); SQLite; DuckDB; T-SQL/Fabric; BigQuery; Snowflake; Trino/Presto/Athena; Spark/Databricks |
| SQLGlot Rust can recognize and validate | Other dialects exposed by the pinned `sqlglot-rust` version; their Cagara-specific intrinsic spelling uses the ANSI/Postgres fallback unless listed above |

Rust unit tests check generated SQL for selected constructs across these
dialects. Those tests catch regressions in the emitted text, while the
SQLite/DuckDB differential test checks query results for the cases in
[`crates/cagara-sql/tests/engines.rs`](../crates/cagara-sql/tests/engines.rs).
The live-engine suite is required in CI and can be skipped locally when the
shells are unavailable.

Filtered aggregates do not depend on the dialect: `sumFilter` and the other
`*Filter` helpers are prelude templates over `CASE`, not a `FILTER` clause, so
they need no per-dialect rewrite.

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

  SQLite is the exception that cannot be papered over: it has no `TRY_CAST`,
  and `CAST('abc' AS INT)` there is `0` rather than an error, so no guard can
  turn a failed cast into NULL without also turning every other unparseable
  value into a silent `0`. Emitting the node as-is lets the engine reject it
  loudly, which is the honest outcome; a `try*` cast is not portable to SQLite.

So the prelude names a `CAGARA_*` function and
[`crates/cagara-sql/src/intrinsics.rs`](../crates/cagara-sql/src/intrinsics.rs)
builds the target's expression tree directly, per dialect family. A
`CAGARA_*` call the backend does not recognize is a compile error rather than
SQL that the engine would reject.

