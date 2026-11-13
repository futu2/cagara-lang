# Development plan

The production query path now uses the compile-time normalizer. The compiler
architecture and its invariants are documented in [`ARCHITECTURE.md`](ARCHITECTURE.md);
this page tracks the remaining boundaries and language work.

## Current boundaries

- The compile-time normalizer and relational arena are the only production query
  path. The former typed lowering adapter and its route differential have been
  removed after the corpus reached agreement.
- A join chained on the left (`a & innerJoin b … & innerJoin c …`, which is what
  a pipeline writes) lowers to one `FROM` chain, with no derived table. A join
  *nested on the right* (`a & innerJoin (b & innerJoin c …) …`) puts its right
  input behind a derived table, because SQL has no parenthesized-join node in the
  AST this backend builds: `a JOIN (b JOIN c)` is only expressible as a subquery.
  That is a projection away from the parenthesized form, and it is correct SQL,
  so it stays until an AST node exists for it.
- A filter after an aggregate is `HAVING`; a filter over a window, a `distinct`
  or a `limit` still wraps a derived table, because those run after grouping.
- Aggregates are values, modified by three combinators (`filtered`,
  `distinctAgg`, `over`) that rewrite the aggregate's template — `SUM(CASE WHEN
  … THEN x END)` and `SUM(DISTINCT x)`, which every engine accepts. A
  first-class `FILTER (WHERE …)` clause would be smaller SQL and needs an IR
  field plus a backend rule; the combinators are the portable spelling until
  then.
- A single lowercase letter in a type position is a type *parameter* (that is
  what makes `id : a -> a` work); anything else must be a constructor the
  language has, and a near miss is reported with a "did you mean". An explicit
  binder for type parameters would let a one-letter typo be reported too, and is
  a language change rather than a check.
- SQL optimization remains opt-in and runs after Cagara lowering.

## Lists

`list a` is a value type: `arrayAgg` builds one, `[1, 2]` is one, and `<>` concatenates two. The
spelling is the target's — `ARRAY[…]`/`ARRAY_AGG`/`||` on PostgreSQL, `list_value`/`list`/
`list_concat` on DuckDB — and a dialect with no array type refuses the query instead of writing
JSON text, which would change the type silently. A list can be built (`[1, 2]`, `arrayAgg`), combined (`<>`), measured and searched (`listLength`,
`listContains`) and unpacked (`unnest`). What is missing is comparison as a value (`==` over two
lists) and the JSON-facing functions; both would need their own intrinsic per dialect.

## Next steps

0. `unnest .x` where `x` is not a list still reports a `type mismatch` whose
   halves read swapped ("expected `int`, found `list a`"): the column's value
   variable is bound to the parameter's list element before the table's row
   arrives, so the conflict surfaces in the row unification rather than at the
   argument. A message that names the operation needs the argument's value slot
   checked before it is bound, which is the same deferral problem as item 0
   below.

1. A `maybe` used where a value is needed now gets its `coalesce`/`just` fix from
   the language server, which reads the message (`field \`s\``, `expected string,
   found maybe string`) and the diagnostic's line. The check-side version — a
   `Suggestion` on the diagnostic itself — still wants `Cons` to carry the span
   of the argument it was posted for, the way `join_args` carries a join's, so
   that a client other than this language server gets the fix too. The channel
   exists (`Diag::suggestions`) and the join warning already uses it.

2. Separate relational rewrites from SQL emission. A `Rel -> Rel` pass can fuse
   stages or choose derived tables while keeping dialect rules in the backend.
3. Extend the execution-backed coverage in `crates/cagara-sql/tests/engines.rs`
   beyond SQLite, DuckDB and PostgreSQL (PGlite). Every shared case already runs
   under both lowerings (the default and `--optimize`); MySQL, T-SQL and the
   warehouses stay text-only because they need a server this repository does not
   start.

## Deferred language work

Catalog declarations, typed bind parameters, a decimal/numeric scalar type for
exact money (`toDecimal` is a SQL cast today; the Cagara type stays `float`, and
no `decimal` constructor exists to inhabit), name-aligned set operations,
`qualify`, grouping sets, lateral or correlated subqueries, and richer window
frames remain future work. The remaining features should extend
the typed core, normalizer, relational arena, and SQL backend together.

See [`CORE.md`](CORE.md) for semantics and [`ROW-TYPES.md`](ROW-TYPES.md) for
row algebra.
