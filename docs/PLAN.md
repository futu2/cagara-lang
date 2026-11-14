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
  input behind a derived table. That is a limit of the AST, not of SQL: sqlglot's
  `TableSource` has `Table`, `Subquery`, `TableFunction`, `Lateral`, `Unnest` and
  nothing for a parenthesized join, so `a JOIN (b JOIN c ON …) ON …` cannot be
  built. Both engines here *do* accept that form (measured on DuckDB and
  PostgreSQL), so the derived table is a workaround for the AST rather than a
  portability requirement — and it is correct SQL either way. Lifting it means an
  AST node for a nested join, which is sqlglot's to add.
- A filter after an aggregate is `HAVING`; a filter over a window, a `distinct`
  or a `limit` still wraps a derived table, because those run after grouping.
- Aggregates are values, modified by three combinators (`filtered`,
  `distinctAgg`, `over`). `filtered p a` is written as a first-class
  `FILTER (WHERE …)` clause where the target has one (PostgreSQL, DuckDB, SQLite,
  Trino, Snowflake) and as `SUM(CASE WHEN p THEN x END)` where it does not (MySQL,
  T-SQL, BigQuery, Spark) — the IR carries the predicate and `dialect.rs` picks the
  spelling. `distinctAgg` is `SUM(DISTINCT x)` everywhere.
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
`listContains`), compared as a value (`==`, `!=`, `<` — the generic comparisons take `a` on both
sides and a `list a` is an `a`; measured on DuckDB and PostgreSQL, which compare arrays
element-wise) and unpacked (`unnest`). What is missing is the JSON-facing functions, which would
need their own intrinsic per dialect.

## Recently closed

- The `Rel -> Rel` pass (`rewrite.rs`) and the lowering rules around it: pagination
  now means what it says (`limit 5 & offset 2` is rows 3..=5), a sort key is written
  as the output column it names, and four derived tables that protected nothing are
  gone (identity projection, semi/anti join inputs, a null-extending join input whose
  computed column stays NULL, and a projection over paging).
- A statement nested inside an expression is rewritten for the target dialect.
  `EXISTS (…)` from a semi/anti join used to carry `NULLS LAST` to MySQL and T-SQL,
  which cannot read it. Every dialect is now checked for the spellings it cannot read
  (`each_dialect_avoids_what_it_cannot_read`) as well as for well-formedness.

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

1. ~~Separate relational rewrites from SQL emission.~~ **Done: the pass exists and
   earns its keep.** `crates/cagara-sql/src/rewrite.rs` is a `Rel -> Rel` pass with
   no dialect and no SQL in it, applied in `compile` after validation and before
   lowering. It holds the rules the lowerer cannot express, because they look at two
   stages at once:

   | rule | why it cannot be a stage decision |
   |---|---|
   | `limit a & limit b` → `limit (min a b)` | a pair of nodes, not one node against a stage |
   | `offset a & offset b` → `offset (a+b)` | same |
   | `limit a & offset b` → `limit (a-b) & offset b` | the fold is legal in one order only, and which order a pipeline is in is a property of the pair |
   | `distinct & distinct` → `distinct` | same |
   | an identity projection → the input | it has to cross a barrier to be worth removing, which is a property of the pair |

   The pagination rule fixed a real bug: `limit 5 & offset 2` keeps rows 3..=5, and
   the pair folded into `LIMIT 5 OFFSET 2` keeps five rows starting at the third. The
   pass turns it into one statement with the right rows, and `Fuse::Offset` now
   refuses to fold into a stage that already has a limit, so the shape cannot come
   back through the lowerer.

   The layering is otherwise as it was, and stays by concern:

   | module | does | knows about dialects? |
   |---|---|---|
   | `rewrite.rs` | `Rel` → `Rel` | no |
   | `lower.rs` | `Rel` → `Stage`: fuse or wrap (`Fuse`, `needs_barrier`, `grouped`, the depth bound) | no |
   | `stage.rs` | `Stage` → a `sqlglot` `SelectStatement` | no |
   | `dialect.rs` | `rewrite` / `validate_dialect_support` on the finished statement | yes — all of it |

   Two of those rules fixed answers rather than plans: the pagination fold, and a
   sort key written as the column it names instead of that column's *expression*
   (`select { id = .a, a = .id } & order [asc .id]` used to order by `a`, which is
   an output alias for `id`).

   A semi/anti join now inlines both of its inputs: it is an existence test, neither
   side is null-extended, and a derived table there protects nothing. That one lives
   in `lower.rs` (`join_input` with `can_place`) rather than in the pass, because it
   is a decision about one input against a stage, not about a pair of nodes.

   A projection that only renames and reorders (a permutation of the stage's
   columns) now crosses the barrier a general one cannot: it cannot change which
   rows a `LIMIT` or a `DISTINCT` below it selects. That rule is in `stage.rs`
   (`keeps_every_column`) and consulted from `lower_select`, because the question is
   one node against a stage — the same boundary as the semi-join inlining.

   A *dropping* projection now folds over paging too (a `LIMIT` picks rows by an
   `ORDER BY` over the input columns, which SQL allows), but never over a
   `DISTINCT` — that is the one where dropping changes how many rows are deduped. A
   projection that reuses a sort key's name for another column is refused as well.

   A null-extended join side now inlines a *computed* column when the expression
   stays NULL without its row (`amount * 2`), and keeps the derived table when it
   does not (a literal, or a call — `coalesce` exists, so no call is assumed). That
   is `null_propagating` in `lower.rs`.

   What is *not* left to do, having checked: a filter on an inner join already folds
   into the outer `WHERE` (`lower_join` hands the sides' `wheres` over), and a
   projection that only renames a join operand already folds into it. The remaining
   candidates are `count(*)` over an aggregate
   (`agg { c = count } & agg { d = count }` is `COUNT(*)` over one row per group,
   which SQL can write as a nested aggregate only with a window or a derived
   table — so it stays). None of them is needed for correctness today.

2. ~~Extend the execution-backed coverage beyond SQLite, DuckDB and PostgreSQL.~~
   **Done to the extent this repository can reach, and the limit is documented.**
   Three engines can be started here (`sqlite3`, `duckdb`, and PostgreSQL through
   PGlite); MySQL, Trino, Spark, BigQuery, Snowflake and T-SQL need a server this
   repository does not start, and there is no container runtime or `nix` on the
   machine the suite runs on. So the other seven targets are covered by the two
   checks a text pipeline can actually make, over every shared case:

   | check | what it proves | what it does not |
   |---|---|---|
   | `every_statement_is_well_formed_sql` | the generator produced well-formed SQL for that target (balanced, no placeholder left, no empty clause) | that the target *accepts* it — sqlglot's parser is dialect-agnostic here, which is measured in the test itself |
   | `each_dialect_avoids_what_it_cannot_read` | the dialect pass removed the spellings that target cannot read (`LIMIT` and `NULLS LAST` for T-SQL, `||` and `FULL JOIN` for MySQL, `::` everywhere but PostgreSQL, backticks outside MySQL) | anything about values — only an engine can answer that |

   The first of those found a real bug: a statement nested inside an expression
   (`EXISTS (…)` from a semi/anti join) was never rewritten, so `NULLS LAST` reached
   MySQL and T-SQL inside the subquery. That is exactly the class of defect the
   engines here could not see, and the reason the text checks were worth adding.

   What would move this item further, in order: a MySQL server (the only target
   whose expression semantics differ in ways text cannot check — `||` is OR, integer
   division differs), then Trino (strict typing), then the warehouses. Each needs a
   service to start, which is why they are not here.

## Deferred language work

Catalog declarations, typed bind parameters, name-aligned set operations,
`qualify`, grouping sets, lateral or correlated subqueries, and richer window
frames remain future work.

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
