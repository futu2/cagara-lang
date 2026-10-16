# Architecture and status

Cagara is a typed query language that compiles queries to SQL. The
prelude is written in Cagara; Rust supplies name resolution, type checking,
normalization, relational IR, language server, and SQL backend.

## Pipeline

```text
source -> syntax/AST -> type checker -> normalization -> checked relational tree
        -> erasure -> schema validation -> SQL lowering -> dialect rewrite -> SQL
```

The intended simplification of this pipeline is described in
[`ARCHITECTURE.md`](ARCHITECTURE.md). It separates the pure compiler kernel
from the incremental workspace shell and makes typed relational construction
the single source of validity rules.

| Crate | Role |
|---|---|
| `cagara-syntax` | Lexer, lossless parser, AST, formatter |
| `cagara-hir` | Modules, name resolution, HM rows, checked relational tree, validation |
| `cagara-sql` | SQL lowering, templates, dialect-specific intrinsics |
| `cagara-lsp` | Diagnostics, hover, definitions, references, symbols, completion |
| `cagara-cli` | `cagara` executable and CLI integration |
| `editors/vscode` / `editors/nvim` | Editor clients for `cagara lsp` |

## Current behavior

- Top-level definitions are generalized; lambda parameters are monomorphic.
- Rows are ordered and kinded. `merge`, `keyMap`, and `mapValue` are reduced
  in the checker and share schema rules with IR validation. See
  [`ROW-TYPES.md`](ROW-TYPES.md).
- Query stages are deferred constraints, so helpers can be reused with
  different rows. Aggregates and windows have explicit phases and cannot be
  placed where SQL would reject them.
- **Each query stage owns its row rule, and every use instantiates it.** A
  stage's operation is declared once, in the stage's own definition — the
  `keyMap (prependSym k) r` term `prefix`'s signature names, the `removeKey k r`
  `omit`'s names, the `mapValue w r` `mapValue`'s names — and the stage
  constraint that ties the input row to the output row travels in that
  definition's scheme. No pass reconstructs a stage's boundary from a composed
  result type, which is what made a body with two key stages — `two = p => q =>
  q & prefix p & prefix p`, and the same with a literal second affix — ask for
  `keyMap (m, r) ~ r` and fail an occurs check. Both spellings check now, as do
  three stages in a row, a helper reused at two affixes, and a helper whose
  wrapper is a parameter (`w => q => q & mapValue w`).
- Row algebra is one reducer: `crates/cagara-hir/src/check/row.rs` owns
  flattening, mapper composition, removal, merging, and value wrapping, and
  answers with a closed row, an open row (its columns so far and the tail it
  could not reduce), or a specific failure — never a plausible partial row.
- Checking records one **typed stage node** per stage application: the
  operation, the row it read, and the row it produced, with an assertion that
  the operation reproduces the output from the input. Adjacent nodes chain, so a
  stage's input row is the previous stage's output row.
- A filter over an aggregate **wraps a derived table**: it lowers to a `WHERE`
  outside the aggregate's subquery, like any other filter that cannot fold into
  its input. See "Aggregate filters and `HAVING`" under *Deliberately absent*
  for why the fold was removed and what reinstating it would take.
- `select` replaces a row; `update` merges computed fields over it. Outer joins
  use `mapValue (AsNullable)` for the nullable side.
- SQL is built as ANSI and rewritten across supported dialects. See
  [`SQL-DIALECTS.md`](SQL-DIALECTS.md).
- `cagara lsp` and both editor clients use one workspace model with unsaved
  buffer overlays.

## Deliberately absent

### Aggregate filters and `HAVING`

A filter written after an `agg` stage currently wraps a derived table:

```sql
SELECT user_id, revenue, n
FROM (SELECT user_id, SUM(amount) AS revenue, COUNT(*) AS n
      FROM orders GROUP BY user_id) AS t1
WHERE (n >= 5)
```

It used to fold into a `HAVING` on the aggregate, avoiding the subquery:

```sql
SELECT user_id, SUM(amount) AS revenue, COUNT(*) AS n
FROM orders GROUP BY user_id HAVING (COUNT(*) >= 5)
```

The fold was removed, not lost. It was the one decision in
`crates/cagara-sql/src/lower.rs` that read **two levels of the tree** to answer
one question — whether a predicate could fold depended on the relation beneath
the stage as well as the stage itself. Every other decision there is local to the
stage (`Stage::needs_barrier`). Removing it leaves a lowering pass whose rules
can each be read from one place, at the cost of a derived table per
aggregate-filtered query. The corpus snapshot in
`crates/cagara-hir/tests/corpus/` is where the current shape is pinned.

Reinstating it properly means giving `Rel` or the lowering pass an explicit
notion of a stage's phase, so "this predicate may run inside the aggregate's own
clause" is a property of the predicate's position rather than a fact recovered by
matching the relation underneath it. The `HAVING` field on `Stage` and the
`COUNT(*) > 0` use in the constant-key aggregate case remain, because those do
not involve the fold: a constant-key `agg` needs `HAVING COUNT(*) > 0` for
correctness, not as an optimization.

## Known gaps

- Expressions and deeply nested relational trees have defensive compiler
  budgets to keep compiler and language-server stacks bounded. Common filter
  pipelines are lowered iteratively and have no separate ten-stage limit.
  Inlining is bounded by depth (200 links) and a definition's normal form by
  size (200 000 nodes), but the size budget is checked on the **finished** term:
  a chain of definitions that each use their argument twice still pays for the
  expansion before being refused. Bounding the expansion itself means charging
  the budget inside the reduction, where `substitute` does the copying, which
  needs a failure channel through `Term::apply`.
- Some join combinations require extra derived tables because parenthesized
  joins are not yet represented directly.
- `--optimize` is opt-in; it runs sqlglot optimization after Cagara lowering.
- A few loader, template qualification, reserved-word, and deep-recursion edge
  cases remain documented in the issue tracker and tests.
- The checker's per-expression facts reach elaboration through the typed tree
  rather than through side maps. `use_tys` (a `Ty` per `ExprId`), `resolutions`
  (what a name referred to), `choices` (the overload candidate at a site),
  `holes`, the span-keyed `use_types` (the same use types rendered for hover),
  and the definition-keyed stage list are all gone; the tree carries the type,
  row, choices, resolution, printed form, and stage of every expression, plus
  `holes` on the body.
  The seam is one trait: `normalize::Facts` has seven methods (`use_ty`,
  `use_row`, `def_result_ty`, `def_body`, `def_template`, `resolution`,
  `choice`, `holes_at`) and everything downstream reads facts through it, which
  is why each map could be deleted once its last reader read the tree instead.
  What remains of the migration is the walk, not the facts: production still
  walks the AST and asks the tree per node, and
  `docs/TYPE-SYSTEM-REDESIGN.md` step 4 is about feeding `CheckedQuery` from
  the tree directly. The bridge and the tree have been shown to cover the same
  nodes — `compile::tests::the_bridge_visits_every_node_the_typed_tree_holds_except_projection_bases`
  — with one stated difference: a projection's base (`lib` in `lib.shout`) is
  not a semantic step, because `infer_proj` records the field's binding at the
  projection node's own `ExprId`. A walk that takes its children from the tree
  must skip those bases too.
  Stage metadata is on the nodes now: `TypedExpr::stage` holds the operation a
  node applied and the rows its signature instance read and produced, and
  `TypedBody::by_stage` keeps the application order, which is not the arena's
  source order. `TypeCheck::row_stages` reads that index. The order is the part
  worth remembering: a chain is recorded as it is applied, while the arena
  follows source order, so `users & prefix "u_" & suffix "_v2"` applies the
  prefix first but writes the suffix application outermost.
  Two properties of `use_types` are worth keeping in mind because they are not
  obvious from the tree: it was keyed by the span the checker *filed* a use
  under, which is the node's own span in every program measured but is carried
  as `TypedExpr::key_span` rather than assumed; and a repeated key span kept the
  last write, which `TypedBody::by_span` resolves while the tree is built, when
  the recording order is still known. `TypeCheck::use_type` and
  `TypeCheck::typed_node_at` read that index.
  `holes` — the count of overloads a definition leaves open — is the one fact
  that might be a projection of `choices` rather than a fact of its own. An
  overload constraint is born at a site (`overload.rs`), becomes
  `Origin::Hole` when stored in a scheme, and becomes a site of the *using*
  definition again when that scheme is instantiated, so the two agree on the
  application path; `compile::tests::an_inherited_hole_is_recorded_against_the_definition_that_inherits_it`
  pins that re-attribution. The path that keeps the count is overload
  *resolution*: a candidate is a definition, `cand_scheme` returns its checked
  scheme with its constraints, and `step_overload` instantiates it with no site
  (`infer.rs`), so a candidate's own open overloads keep `Origin::Hole` into
  the caller's leftovers — where the count sees them and `choices` records
  nothing. `check/tests.rs::overload_candidates_resolve_their_own_overloads`
  shows the solver resolving them when they are literals; whether one can
  survive unresolved is not proven either way, so the count stays a fact of its
  own until someone proves that path closed.
  `holes` — the count of overloads a definition leaves open — is the one fact
  that might be a projection of `choices` rather than a fact of its own. An
  overload constraint is born at a site (`overload.rs`), becomes
  `Origin::Hole` when stored in a scheme, and becomes a site of the *using*
  definition again when that scheme is instantiated, so the two agree on the
  application path; `compile::tests::an_inherited_hole_is_recorded_against_the_definition_that_inherits_it`
  pins that re-attribution. The path that keeps the count is overload
  *resolution*: a candidate is a definition, `cand_scheme` returns its checked
  scheme with its constraints, and `step_overload` instantiates it with no site
  (`infer.rs`), so a candidate's own open overloads keep `Origin::Hole` into
  the caller's leftovers — where the count sees them and `choices` records
  nothing. `check/tests.rs::overload_candidates_resolve_their_own_overloads`
  shows the solver resolving them when they are literals; whether one can
  survive unresolved is not proven either way, so the count stays a fact of its
  own until someone proves that path closed.

## Roadmap

These features are deliberately deferred while the core query language stays
small and typed:

- Generate checked-in table declarations from a database schema or catalog.
- Add typed runtime parameters and prepared-query metadata to the compiler API.
  A parameter compiles to a bind placeholder in the emitted SQL; its value is
  never spliced into the statement text, and never becomes an identifier. The
  test for this must assert that no parameter value appears anywhere in the
  emitted SQL, so the first implementation cannot quietly degrade into string
  interpolation.
- Extend relational phases with filtered aggregates (`FILTER (WHERE …)`),
  `qualify` (a filter on a window output would no longer need a derived table),
  grouping sets, lateral/correlated subqueries, and richer window frames.

Cagara is a typed query language, not a general-purpose functional language.
The type vocabulary is compiler-owned; user programs cannot declare new types.
Plan work around query construction, static checking, SQL generation, and the
developer experience for those workflows.

Run `cargo test --workspace` and `cargo check --workspace` before changing
shared type or lowering rules. The worked report in `examples/report.cagara` is
also exercised against SQLite and DuckDB in CI.
