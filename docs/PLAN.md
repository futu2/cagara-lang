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
- Filtered aggregates are prelude templates over `CASE`, not a relational node.
  They cover every built-in aggregate without a dialect-specific `FILTER`
  lowering; a first-class `FILTER` would need an IR field and a backend rule.
- A name in a type position that is not one of `CONS` (`check/reduce.rs`) is a
  type *parameter*, because signatures carry their parameters implicitly. That
  is what makes `id : a -> a` work, and it also means a mistyped type
  (`amount = decimal`) silently makes the definition polymorphic instead of
  being reported. Distinguishing a parameter from a typo needs an explicit
  binder for type parameters — the fix is a language change, not a check.
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
