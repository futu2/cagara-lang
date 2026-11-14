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

- The `??` advice reaches overload failures too: `.n + .n` on a `maybe int` names
  the nullable argument and the two ways out, where it used to list candidates.
- A `decimal` cast finds its parameters through a partly applied cast
  (`lift (toDecimal 10 2) .d`) and through a named constant (`p = 10`), both of which
  used to leave `decimal a b` open in `--types`.
- The documented decimal range is `0 ≤ s ≤ p ≤ 38` — a scale of zero is a whole
  number of units and was always allowed; only the prose said otherwise.

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

   *Where to start*, now that the code has been read with this in mind: the stage
   plan and the SQL text are already two layers — `stage.rs` holds a mutable
   `SELECT` with no dialect in it, and `lower.rs` decides when a node *fuses* into
   the current stage or becomes a derived table (`Fuse`, `grouped`, the depth
   bound, `derived`). The decisions live in `lower.rs`, where the emitter is, so a
   test can only see them as SQL text. The first step is therefore to lift the
   fuse-or-wrap decision into a function `Rel -> [Stage]` that names no sqlglot
   type except through `Stage`, then have `lower.rs` render the stages and let the
   tests assert on the stage list. That is a small change to a large file, and it
   is the part every later rewrite (identity projection, filter-into-join,
   `distinct` placement) needs.
2. Extend the execution-backed coverage in `crates/cagara-sql/tests/engines.rs`
   beyond SQLite, DuckDB and PostgreSQL (PGlite). Every shared case already runs
   under both lowerings (the default and `--optimize`); MySQL, T-SQL and the
   warehouses stay text-only because they need a server this repository does not
   start.

## Deferred language work

Catalog declarations, typed bind parameters, a decimal/numeric scalar type for
exact money, name-aligned set operations, `qualify`, grouping sets, lateral or
correlated subqueries, and richer window frames remain future work.

`decimal` was the one with a correctness argument behind it, and it is in — see
above for what ships and what was decided against. The decisions as they stand:

- **One type everywhere.** `+`, `-`, `*` take two operands of the same `(p,s)` and
  return it; a comparison needs the same type on both sides; a different `(p,s)` is
  a compile error whose message names both parameters and says `toDecimal` is how a
  value moves. There is no widening coercion and no mixed comparison.
- **The declared width is the promise.** `+`/`-`/`*` keep the operand type rather
  than widening the way SQL does: the engine's own arithmetic is exact anyway, and a
  *computed* result type cannot be stated as a signature without type-level
  arithmetic. A product's scale is the engine's, so `toDecimal p s` is how a value
  comes back to the declared one.
- **Division is refused**, because PostgreSQL keeps a decimal quotient exact and
  DuckDB promotes it to `DOUBLE`: no same-type rule holds on both targets.
- **Literals lift.** A numeric literal takes the `(p,s)` its context demands, with
  its digits checked at compile time, the way `1` is already an `int` or a `float`
  depending on context. Columns and expressions never lift.
- **Parameters may be zero-scaled.** `decimal 38 0` is a whole number of units;
  `1 ≤ p ≤ 38` and `0 ≤ s ≤ p`.
- `sum`/`avg` are signatures, not rules: `sum : expr r (decimal p s) -> agg
  (expr r (maybe (decimal 38 s)))`, because the width is a constant in the portable
  family (PostgreSQL's `numeric` is unbounded, which is why the language states the
  portable 38 rather than either engine's best).

The remaining deferred items are unchanged: catalog declarations, typed bind
parameters, name-aligned set operations, `qualify`, grouping sets, lateral or
correlated subqueries, and richer window frames.

See [`CORE.md`](CORE.md) for semantics and [`ROW-TYPES.md`](ROW-TYPES.md) for
row algebra.
