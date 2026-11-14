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
cast today and the Cagara type stays `float`, so a money column is *typed* as an
IEEE double even where the engine's column type is exact. It gets parameters —
`decimal p s`, both literal integers, `1 ≤ s ≤ p ≤ 38` — because that is what the
engines actually have, and because a mismatch can then say which way it is wrong.

The decisions, so the next round does not relitigate them:

- **No implicit conversion in arithmetic.** `+`, `-`, `*` take two operands of
  the *same* `(p,s)`; a different one is a compile error that suggests
  `toDecimal p s`. There is no widening coercion between decimal types.
- **Results still widen**, which is why arithmetic is a checker rule rather than
  a signature: `+`/`-` give `(max(p-s) + s + 1, s)`, `*` gives `(p₁+p₂, s₁+s₂)`,
  and a result past 38 digits is refused with the operands named, since no cast
  can rescue it — the operands have to be narrowed.
- **Literals lift.** A numeric literal takes the `(p,s)` its context demands, the
  way `1` is already an `int` or a `float` depending on context, with its digits
  checked at compile time. Columns and expressions never lift.
- **Comparisons may mix `(p,s)`.** `decimal 10 2` against `decimal 12 2` compares
  exact values in SQL and produces no value, so no conversion is implied;
  arithmetic is the strict one.
- `sum`/`avg` are signatures, not rules: `sum : expr r (decimal p s) -> agg
  (expr r (maybe (decimal 38 s)))`, because the width is a constant in the
  portable family (PostgreSQL's `numeric` is unbounded, which is why the language
  states the portable 38 rather than either engine's best).
- `/` stays refused: the scale of a quotient is engine-defined (PostgreSQL goes to
  1000 digits, Trino errors, DuckDB truncates), so the answer is `roundTo` plus
  `toDecimal`, written out.
- Type-level numbers are `Ty::Con("10", [])` rather than a new `Ty` variant, so
  elaboration and every walker stay untouched; the parser gains numeric type
  atoms and the checker validates the range.

S1 is in: the type exists (`decimal p s` in declarations and signatures, with
`Ty::Nat` parameters, `ScalarType::Decimal`, and the range check),
`toDecimal`/`tryDecimal` return it with the parameters read from the literals, and
`toString`/`toInt`/`round`/`roundTo`/`toFloat` have their decimal forms — the SQL
is the cast the template already wrote (`DECIMAL(10, 2)`, `NUMERIC`, `NUMBER`).

The aggregates are in too: `sum`/`avg` of a `decimal p s` are `maybe (decimal 38
s)` — stated in the signature, because 38 is a constant in the portable family —
and `min`/`max` keep the operand's type.

S2 is in as well, with one deliberate disagreement with the plan above: `+`/`-` on
two decimals of the same `(p,s)` return that *same* type rather than SQL's wider
one, and `*`/`/` are refused. The wider result type is a function of the operands,
and the overload machinery cannot express that: declaring the result as a variable
and filling it in from a `Cons` worked for the arithmetic, but the candidate then
shadowed the generic `_==_` wherever the argument type was still open (the manual's
own `where (.name == n)` stopped type-checking) and made `twice = x => x + x` infer
`expr a b -> expr a c`. `Cons::DecimalArith` exists in that history and was removed
with the experiment; doing it properly needs overload arbitration to prefer the
generic candidate, or a real type-level arithmetic in the type language.

A cast on SQLite is refused (its `NUMERIC` affinity stores REAL or INTEGER, so the
precision is lost silently), and `money_is_exact_on_the_engines` runs the sum of
three 19.99s and `0.1 + 0.2` on DuckDB and PostgreSQL.

The mixed-parameter comparison is a **decision**, not a gap: one type everywhere.
`decimal 10 2 == decimal 12 2` is a compile error whose message names both
parameters and says to bring them together with `toDecimal`. That keeps the type's
promise simple — a value of one `(p,s)` never silently becomes another — and it is
what arithmetic already does.

It was nearly the other way. An earlier design allowed mixed comparisons (they
produce no value, so nothing is converted) and it could not be built without
damage: a decimal-pair candidate for `==` competes with the generic
`expr r a -> expr r a -> expr r bool` whenever an argument is still open, and
`prefer` waits for the arguments before deciding, so a third candidate delays that
decision and the later ranking prefers the `maybe` twin for a `.score : maybe int`
— right for a nullable-aware comparison, wrong for the case-arm refusal that the
non-null reading produces. Lowering the candidate's score, or only the
parameterized constructors' score, left exactly those two tests failing. Recorded
because it is the shape any future "let these two types meet" feature will hit.

Multiplication is in as well, under the same rule: the product is exact, and its
scale — the sum of the operands' — is the engine's, so `toDecimal p s` is how the
value comes back to the declared one. Division is the one operation left out, by **decision**, and not for a type-system
reason: PostgreSQL keeps a decimal quotient exact, DuckDB promotes it to `DOUBLE`
(measured: `typeof(20.00::DECIMAL(10,2) / 4.00::DECIMAL(10,2))` is `DOUBLE`, and the
value comes back `5.0` against PostgreSQL's `5.0000000000000000`), so a same-type
rule would be a promise one target breaks. The refusal's message says exactly that
and names the way through — `toFloat`, then `toDecimal p s` — instead of listing the
`int` and `float` signatures that were never going to match.

What is left of the decimal work is therefore: a result type that *widens* the way
SQL's does (which needs type-level arithmetic in the language, or an arbitration
that can rank a computed result), a division that stays exact on every target (which
needs a cast whose parameters come from the type, i.e. the same type-level
machinery), and `--ddl` if the schema side ever emits column types. Mixed
comparisons are not on the list: they are refused by decision.

See [`CORE.md`](CORE.md) for semantics and [`ROW-TYPES.md`](ROW-TYPES.md) for
row algebra.
