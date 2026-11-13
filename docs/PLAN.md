# Development plan

The production query path now uses the compile-time normalizer. The compiler
architecture and its invariants are documented in [`ARCHITECTURE.md`](ARCHITECTURE.md);
this page tracks the remaining boundaries and language work.

## Current boundaries

- The compile-time normalizer and relational arena are the only production query
  path. The former typed lowering adapter and its route differential have been
  removed after the corpus reached agreement.
- Some joins need derived tables because parenthesized joins are not yet a
  first-class `Rel` shape.
- A filter after an aggregate currently becomes an outer `WHERE` over a derived
  table. The phase metadata a `HAVING` rewrite needs is already there (`ir::Phase`
  with `rules::{mix, nested, place}`, and a per-stage record of the input and
  output rows in `check/stage.rs`); what is missing is the backend rule that
  recognises an `Agg`-phase predicate over the stage that produced it.
- Aggregates are values, modified by three combinators (`filtered`,
  `distinctAgg`, `over`) that rewrite the aggregate's template — `SUM(CASE WHEN
  … THEN x END)` and `SUM(DISTINCT x)`, which every engine accepts. A
  first-class `FILTER (WHERE …)` node would be smaller SQL and needs an IR field
  plus a backend rule; the combinators are the portable spelling until then.
- A single lowercase letter in a type position is a type *parameter* (that is
  what makes `id : a -> a` work); anything else must be a constructor the
  language has, and a near miss is reported with a "did you mean". An explicit
  binder for type parameters would let a one-letter typo be reported too, and is
  a language change rather than a check.
- SQL optimization remains opt-in and runs after Cagara lowering.

## Next steps

1. Separate relational rewrites from SQL emission. A `Rel -> Rel` pass can fuse
   stages or choose derived tables while keeping dialect rules in the backend.
2. Extend the execution-backed coverage in `crates/cagara-sql/tests/engines.rs`
   beyond SQLite and DuckDB, and run it with `--optimize` as well as the
   default lowering.

## Deferred language work

Catalog declarations, typed bind parameters, a decimal/numeric scalar type for
exact money (`toDecimal` is a SQL cast today; the Cagara type stays `float`, and
no `decimal` constructor exists to inhabit), name-aligned set operations, first-class aggregate `FILTER`
emission (the prelude's `*Filter` helpers already cover the built-in aggregates
portably), `qualify`, grouping sets, lateral or correlated subqueries, and
richer window frames remain future work. The remaining features should extend
the typed core, normalizer, relational arena, and SQL backend together.

See [`CORE.md`](CORE.md) for semantics and [`ROW-TYPES.md`](ROW-TYPES.md) for
row algebra.
