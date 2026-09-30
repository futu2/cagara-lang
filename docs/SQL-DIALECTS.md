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

When targeting another engine, inspect the generated SQL and run representative
queries against that engine, especially if they use dates, string functions,
integer division, null ordering, or T-SQL-specific boolean handling. Add a
dialect rewrite and an expected-output or engine test when Cagara needs a
different intrinsic spelling or semantic adjustment.
