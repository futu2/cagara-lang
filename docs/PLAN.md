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

## Recently closed

- `unnest .x` of a non-list says `unnest needs a list, and this is not one`. The
  conflict still surfaces in the row unification, but the application records its
  argument's span (`unnest_args`) and the definition's error is rewritten when it
  falls inside one.
- A `maybe` used where a value is needed carries its `??`/`just` fix on the
  diagnostic itself, and `cagara check` prints it (`fix: Use `?? ""` — .s → (.s
  ?? "")`) and carries it in `--json` as `fixes`. The projection's refusal comes
  out of its constraint, so the checker keeps two side tables — where each field
  value is written (`field_spans`, keyed by the stage's span) and where each
  column access is written (`field_uses`) — and builds the suggestion from the
  message's innermost `field `name`` plus that column's span. The language
  server's message rule stays as the fallback for a diagnostic without one.

## Next steps

1. Separate relational rewrites from SQL emission. A `Rel -> Rel` pass can fuse
   stages or choose derived tables while keeping dialect rules in the backend.
2. Extend the execution-backed coverage in `crates/cagara-sql/tests/engines.rs`
   beyond SQLite, DuckDB and PostgreSQL (PGlite). Every shared case already runs
   under both lowerings (the default and `--optimize`); MySQL, T-SQL and the
   warehouses stay text-only because they need a server this repository does not
   start.

## Deferred language work

Catalog declarations, typed bind parameters, a decimal/numeric scalar type for
exact money, name-aligned set operations, `qualify`, grouping sets, lateral or
correlated subqueries, and richer window frames remain future work.

`decimal` is the one with a correctness argument behind it: `toDecimal` is a SQL
cast today and the Cagara type stays `float`, so a money column summed in Cagara
is summed as an IEEE double. Adding it as a parameterless scalar (`ScalarType`,
`ty_to_scalar`, `SCALARS`, `CONS`, `Display`) and then the places that name a
scalar's SQL type — `CAGARA_TO_DECIMAL`, `tryDecimal`, the declaration types, the
`+ - *` overloads and `sum`'s signature — would make the type follow the cast.
Precision and scale stay call-site arguments, as `toDecimal 10 2` has them today. The remaining features should extend
the typed core, normalizer, relational arena, and SQL backend together.

See [`CORE.md`](CORE.md) for semantics and [`ROW-TYPES.md`](ROW-TYPES.md) for
row algebra.
