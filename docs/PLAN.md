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

## In progress: values where only rows existed

Two features need the same thing: a value position where today only a row
position works.

**The date family at value level.** `year .d` reads a column; `year
currentDate` should be a plain `int`. The value forms exist as twins
(`year : date -> int` beside `year : expr r date -> expr r int`) and they work
inside a query — `select { a = truncYear currentDate }` is `date` — but not in
an annotated definition: `y : int = year currentDate` still picks the row form.
The reason is the argument's type: `currentDate` is only *known* to be `date`
after the overload is arbitrated, and while it is open the ranking treats it as
a variable and prefers the row-indexed candidate. Pinning the application's
result to the signature's type fixes the value case but not the row case: a
record field's column (`year .d`) starts a *fresh* row, so the overload still
cannot see that `.d` is a `date`. That needs the record's input row to be bound
before its fields are checked — `record_expr(phase, row, fields, out)` carries
the row in its second slot, but the stage's signature row is a *different*
variable, bound only after the record is checked. The fix is a deferred record
constraint: check the fields, then re-check the ones whose columns were open
once the row closes. Attempts that bind the row eagerly (a `record_row` context
in `check_record`) resolve the overload but mistype `update` and `case` arms,
which is why this is a design step rather than a patch.

**`list` as a value type.** `list a` is a type today only for key lists
(`order`, `partition`) and `inList`, never a column. Making it a value needs
four pieces together: the type accepted in `expr r (list a)` and printed; an
aggregate that produces one (`arrayAgg : expr r a -> agg (expr r (list a))`);
list literals, spelled per dialect (`ARRAY[1, 2]` on PostgreSQL, `[1, 2]` on
DuckDB); and `<>` extended with a `list a` instance (`||` on PostgreSQL,
`list_concat` on DuckDB). SQLite has no array type: a `list` there is a compile
error naming the dialect, not a silent fallback to JSON text — a fallback would
change the type without saying so. The differential engine tests grow a
DuckDB-only case.

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
