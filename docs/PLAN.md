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
  table. Add explicit phase metadata before considering a `HAVING` rewrite.
- SQL optimization remains opt-in and runs after Cagara lowering.

## Next steps

1. Separate relational rewrites from SQL emission. A `Rel -> Rel` pass can fuse
   stages or choose derived tables while keeping dialect rules in the backend.
2. Add execution-backed examples against SQLite and DuckDB to cover the
   generated SQL beyond structural compilation tests.

## Deferred language work

Catalog declarations, typed bind parameters, filtered aggregates, `qualify`,
grouping sets, lateral or correlated subqueries, and richer window frames remain
future work. Each feature should extend the typed core, normalizer, relational
arena, and SQL backend together.

See [`CORE.md`](CORE.md) for semantics and [`ROW-TYPES.md`](ROW-TYPES.md) for
row algebra.
