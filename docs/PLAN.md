# Architecture and status

Cagara is a typed query language that compiles queries to SQL. The
prelude is written in Cagara; Rust supplies name resolution, type checking,
source elaboration, relational IR, language server, and SQL backend.

## Pipeline

```text
source -> syntax/AST -> type checker -> checked relational tree -> erasure
        -> schema validation -> SQL lowering -> dialect rewrite -> SQL
```

The intended simplification of this pipeline is described in
[`ARCHITECTURE.md`](ARCHITECTURE.md). It separates the pure compiler kernel
from the incremental workspace shell and makes typed relational construction
the single source of validity rules.

| Crate | Role |
|---|---|
| `cagara-syntax` | Lexer, lossless parser, AST, formatter |
| `cagara-hir` | Modules, name resolution, HM rows, checked relational tree, validation |
| `cagara-sql` | SQL lowering, templates, dialect-specific intrinsics |
| `cagara-lsp` | Diagnostics, hover, definitions, references, symbols, completion |
| `cagara-cli` | `cagara` executable and CLI integration |
| `editors/vscode` / `editors/nvim` | Editor clients for `cagara lsp` |

## Current behavior

- Top-level definitions are generalized; lambda parameters are monomorphic.
- Rows are ordered and kinded. `merge`, `keyMap`, and `mapValue` are reduced
  in the checker and share schema rules with IR validation. See
  [`ROW-TYPES.md`](ROW-TYPES.md).
- Query stages are deferred constraints, so helpers can be reused with
  different rows. Aggregates and windows have explicit phases and cannot be
  placed where SQL would reject them.
- `select` replaces a row; `update` merges computed fields over it. Outer joins
  use `mapValue (AsNullable)` for the nullable side.
- SQL is built as ANSI and rewritten across supported dialects. See
  [`SQL-DIALECTS.md`](SQL-DIALECTS.md).
- `cagara lsp` and both editor clients use one workspace model with unsaved
  buffer overlays.

## Known gaps

- Expressions and deeply nested relational trees have defensive compiler
  budgets to keep compiler and language-server stacks bounded. Common filter
  pipelines are lowered iteratively and have no separate ten-stage limit.
- Some join combinations require extra derived tables because parenthesized
  joins are not yet represented directly.
- `--optimize` is opt-in; it runs sqlglot optimization after Cagara lowering.
- A few loader, template qualification, reserved-word, and deep-recursion edge
  cases remain documented in the issue tracker and tests.

## Roadmap

These features are deliberately deferred while the core query language stays
small and typed:

- Generate checked-in table declarations from a database schema or catalog.
- Add typed runtime parameters and prepared-query metadata to the compiler API.
- Extend relational phases with `having`, filtered aggregates, `qualify`,
  grouping sets, lateral/correlated subqueries, and richer window frames.

Cagara is a typed query language, not a general-purpose functional language.
The type vocabulary is compiler-owned; user programs cannot declare new types.
Plan work around query construction, static checking, SQL generation, and the
developer experience for those workflows.

Run `cargo test --workspace` and `cargo check --workspace` before changing
shared type or lowering rules. The worked report in `examples/report.cagara` is
also exercised against SQLite and DuckDB in CI.
