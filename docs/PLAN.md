# Development plan

The production query path now uses the compile-time normalizer. The compiler
architecture and its invariants are documented in [`ARCHITECTURE.md`](ARCHITECTURE.md);
this page tracks the remaining boundaries and language work.

## Compatibility boundaries

- `typed_lower.rs`, `query_lowering.rs`, and `CheckedQuery` remain for existing
  checked-tree consumers and route tests. Retire them after those consumers
  move to the relational arena API.
- The route-differential snapshot records intentional source-origin differences
  for reused leaves and combine nodes. They do not change the relation or SQL.
  Keep the comparison until the SQL corpus covers those cases independently.
- Some joins need derived tables because parenthesized joins are not yet a
  first-class `Rel` shape.
- A filter after an aggregate currently becomes an outer `WHERE` over a derived
  table. Add explicit phase metadata before considering a `HAVING` rewrite.
- SQL optimization remains opt-in and runs after Cagara lowering.

## Next steps

1. Move public checked-tree consumers to the arena API, then remove the legacy
   adapters and route tests that only protect those adapters.
2. Separate relational rewrites from SQL emission. A `Rel -> Rel` pass can fuse
   stages or choose derived tables while keeping dialect rules in the backend.
3. Add execution-backed examples against SQLite and DuckDB, then reassess
   whether the independent term oracle still adds useful coverage.

## Deferred language work

Catalog declarations, typed bind parameters, filtered aggregates, `qualify`,
grouping sets, lateral or correlated subqueries, and richer window frames remain
future work. Each feature should extend the typed core, normalizer, relational
arena, and SQL backend together.

See [`CORE.md`](CORE.md) for semantics and [`ROW-TYPES.md`](ROW-TYPES.md) for
row algebra.
