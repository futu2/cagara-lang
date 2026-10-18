# Architecture and status

Cagara is a typed query language that compiles queries to SQL. The
prelude is written in Cagara; Rust supplies name resolution, type checking, the
normalizer, the relational arena and IR, the language server, and the SQL
backend.

## Pipeline

```text
source -> syntax/AST -> resolve + type checking -> typed core
       -> normalizer (normalize/: beta-reduction, aliases, helpers, overloads)
       -> typed relational arena -> erasure -> Rel
       -> schema validation -> SQL lowering -> dialect rewrite -> SQL
```

Production today is the older shape of the same pipeline: the source-directed
lowering in `typed_lower.rs` builds a `TypedQuery` graph and a `CheckedQuery`
tree, reconciled through `carry_checked_payloads` and `rebuild_checked`, and
the normalizer in `normalize/` is the module that replaces both with one reduction and
one arena. The normalizer in `term_oracle.rs` is a test-only oracle for route
agreement; it never compiles a query. An accepted shape the production route
cannot construct produces a compiler capability error until a rule is added.
See [`CORE.md`](CORE.md) for the normalizer's rules and the arena's
constructors, and "Consolidating the normalizer" below for what is left.

The target shape, and how the kernel is separated from the incremental workspace
shell, are described in [`ARCHITECTURE.md`](ARCHITECTURE.md). In both shapes typed
relational construction is the single source of validity rules.

| Crate | Role |
|---|---|
| `cagara-syntax` | Lexer, lossless parser, AST, formatter |
| `cagara-hir` | Modules, name resolution, HM rows, the normalizer, the relational arena, validation |
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
  size (200 000 nodes), but in the **oracle** the size budget is checked on the
  finished term: a chain of definitions that each use their argument twice
  still pays for the expansion before being refused. Bounding it there means
  charging inside the reduction, where `substitute` does the copying, which
  needs a failure channel through `Term::apply`. The production normalizer
  already charges as it goes (`normalize/budget.rs`: depth 200, work 200 000, the
  work spent before a node is built), so this gap is the oracle's alone.
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
  The normalized `Facts` trait remains for the test-only oracle, which reads
  checker-owned typed nodes through it. Production query lowering reads those
  nodes directly; no compatibility walk or side maps remain for that path.
  The oracle has no production query-elaboration role; the corpus and SQL
  agreement suites guard that every accepted shape has a production rule.
  Stage metadata is on the nodes now: `TypedExpr::stage` holds the operation a
  node applied and the rows its signature instance read and produced, and
  `TypedBody::by_stage` keeps the application order, which is not the arena's
  source order. `TypeCheck::row_stages` reads that index. The order is the part
  worth remembering: a chain is recorded as it is applied, while the arena
  follows source order, so `users & prefix "u_" & suffix "_v2"` applies the
  prefix first but writes the suffix application outermost.
  Each stage record also carries a **stable identity**
  (`check::NodeRef` — the definition the application was written in, and the
  application node's `ExprId` in it). Typed lowering uses that identity when it
  constructs a query node; the legacy adapter no longer carries a parallel
  stage report or compares a second set of rows.
  `check::query::TypedQuery` is the query skeleton as its own arena, where a
  node carries its identity, operation, typed children, checker-closed rows,
  and checked payloads. It is the current production query path. Because the
  checked layer is still a separate tree, `TypedQuery::rebuild_checked`
  records whether the typed graph reconstructs every checked node exactly, and
  `carry_checked_payloads` copies expression payloads across. That duplication
  is the scaffolding the normalizer removes: see "Consolidating the normalizer".
  `check::query` tests pin that every node fact is available from the checker
  where a stage is applied.
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

## Query elaboration migration: completion record

The migration replaced a normalizing elaborator with a typed one. It is
finished: production builds every accepted query from checker-owned facts, the
normalizer is a test-only oracle, and the record below says what each old
function did, what replaced it, and what is left.

Everything in this chapter describes a state that **held** at the end of the
migration. The next section, "Consolidating the normalizer", says what is
happening now.

### What the migration removed

| Old function (`typed_lower.rs` unless noted) | What it did | Replaced by |
|---|---|---|
| `lower` / `lower_impl` / `lower_unhydrated` | The entry point: a chain of `if let Some(lowered) = lower_…_pipeline(...)` attempts, each declining by returning `None` | `normalize::definition` (one reduction) |
| `lower_plain_value` / `apply_plain_value` | A second partial functional normalizer: `PlainValue::{Expr, Compose, Overloads, Function}`, one-argument environments, `PlainFunctionBody::{Expr, Template}` | `normalize/value.rs` (`Value`, `apply_closure`, `apply_stage`) |
| `lower_plain_definition` / `scalar_definition_target` / `scalar_alias_cycle` | Alias following, depth 16, with cycle detection by following `Resolved::Def` chains | `normalize/value.rs` (`Normalizer::definition`, `Normalizer::overloads`) + `normalize/budget.rs` |
| `apply_helper`, `apply_plain_helper_pipeline` | Helper instantiation: parameter substitution through `LoweringContext.query_bindings` and `stage_bindings`, transactional retries (`lower_plain_composed_expr`) | `Normalizer::apply_closure` (a helper is a closure like any other) |
| `lower_plain_composed_expr`, `lower_composed_expr`, `lower_combine_pipeline`, `lower_composed_pipeline`, `apply_plain_stage_expr`, `lower_shorthand_stage`, `lower_inline_stage_body`, `lower_keep_pipeline`, `lower_simple_{projection,where,order}_pipeline`, `lower_plain_expression_chain` | Shape-dispatching entry points, each walked the syntax in its own order and declined by returning `None` | One reduced name: `normalize/value.rs` (`Normalizer::build_stage`, `build_combine`) |
| `copy_typed_graph`, `copy_typed_chain`, `LoweringContext.query_bindings` | Copying a whole sub-graph into a caller's arena | Not needed: all uses share one arena (`normalize/arena.rs`) |
| `index_exprs`, `find_expr_id`, `expr_depth_to_id`, `find_base`, `contains_*` (`distinct`, `combine`, `unmigrated_stage`, `other_stage` …) | Syntax searches used to decide which slice applies | Not needed: no slices |
| `substitute_expr`, `lambda_params`, `lambda_tail` | Substitution of helper arguments into helper syntax | The environment (`normalize/env.rs`) |
| `scalar_operator_template`, `declared_template`, `template_definition`, `resolved_template_for_target`, `declared_primitive`, `primitive_definition` | Resolved prelude names and SQL templates through definitions and overload choices | `normalize/scalar.rs`, reading `Resolved` and the checker's `Choice` |
| `lower_plain_window_spec`, `lower_plain_frame`, `lower_plain_bound` | Window specification and frame construction from source | `normalize/scalar.rs` (the spec is a record as usual) + `normalize/frame.rs` |
| `rows_match`, `unordered_rows_match`, `checked_kind_matches`, `query_error`, `QueryErr` | Cross-checks between the two trees, and a third error type | The arena's constructors (`normalize/arena.rs`) and `normalize::Control` |
| `carry_checked_payloads`, `require_checked_payloads`, `rebuild_checked`, `checked_node_kind` (`check/query.rs`) | Copying payloads from the checked tree to the typed one and rebuilding one from the other | `TypedQuery` and `CheckedQuery` merged into one arena (`normalize/arena.rs`) |

Together with `check/query.rs` (1 581 lines) that is the bulk of the query
pipeline; `normalize/` replaces it with one reducer and one arena (`normalize/arena.rs`
is under a thousand lines, erasure included).

### What is kept, and why

`term_oracle.rs` survives as a **test-only oracle**. It carries its own `Term`
representation, its own textual capture-avoiding `substitute`, and its own query
lowering by primitive name (`lower_query_primitive`). It is not a shared
implementation of anything: the agreement tests drive it directly for the same
programs the compiler compiles, and the test is worth running only while the
two can disagree. The oracle's gates are
`cagara-hir/tests/lower_query_agreement.rs` (shapes and rows) and
`cagara-sql/tests/typed_route_agreement.rs` (generated SQL in six dialects).

The corpus snapshots (`cagara-hir/tests/corpus/snapshots/`, driven by
`cagara-sql/tests/corpus.rs`) still pin the emitted SQL byte-for-byte.

### What the migration left behind

The end state was *one typed tree*. The migration produced **two**: a
`TypedQuery` arena in `check/query.rs` and a `CheckedQuery` tree in
`checked.rs`, with `carry_checked_payloads` and `rebuild_checked` keeping them
in step at the boundary. That duplication was scaffolding, and removing it is
the work described next.

## Consolidating the normalizer: the state now

The end state aimed for is:

```text
source -> resolve + type-check -> typed core
       -> one compile-time normalizer (beta-reduction, aliases, composition,
          helpers, overloads)
       -> one typed relational arena
       -> erasure -> Rel -> SQL
```

A pure functional language does not compile by matching the surface spelling of
its values, and the current lowering does exactly that: a series of
`lower_*_pipeline` entry points, each recognizing one family of query shape,
plus `lower_plain_value` / `apply_plain_value` — a second, partial normalizer that
grew beside them to handle the function values the shapes could not express. The
normalizer makes the reduction explicit and canonical, and the primitives
construct relational nodes at saturation.

| Step | What | Status |
|---|---|---|
| 1 | Move shared facts (`Resolved`, `TemplateKind`, `DefId`) out of `term_oracle.rs` into `check/core_facts.rs` | **Done** |
| 2 | The normalizer: values, environments, reduction rules (`normalize/mod.rs`, `normalize/value.rs`, `normalize/env.rs`, `normalize/scalar.rs`, `normalize/frame.rs`) | **In progress** |
| 3 | The relational arena with its constructors and structural erasure (`normalize/arena.rs`); budgets (`normalize/budget.rs`); the evaluate-and-erase boundary (`normalize/route.rs`) | **Done as a module** |
| 4 | Differential gate between the two routes (`tests/normalize_differential.rs`, `tests/snapshots/normalize_mismatches.txt`) | **In place: 47 of 99 query definitions agree** |
| 5 | Pipeline snapshot gate: every example and corpus program's erased `Rel`, fault class, diagnostics and checked tree (`tests/pipeline_golden.rs`) | **Done** |
| 6 | Cut production over: `elaborate.rs` calls `normalize::route`; delete `typed_lower.rs`, `check/query.rs`, and the `TypedQuery`/`CheckedQuery` duplication; fold `CheckedQuery` onto the arena | **Remaining** |
| 7 | Keep the oracle test-only and retire it once the corpus is broad enough | **Remaining** |

**Where it stands.** Production is unchanged: `elaborate.rs::elaborate_def`
calls `query_lowering::lower`, which calls `typed_lower::lower_with_status`, and
the resulting `TypedQuery` graph is reconciled with the `CheckedQuery` tree
through `carry_checked_payloads` and `rebuild_checked`, with a
`rebuilt_checked_tree` flag recording whether the reconciliation was exact.

The normalizer is not yet the production route. Every module of it exists, and
`tests/normalize_differential.rs` runs it beside the lowering it replaces over every
query definition in `examples/`, the HIR corpus and eleven inline shapes. Of the
99 definitions it compares, **47 agree and 52 do not**; the mismatching
definitions are recorded in `tests/snapshots/normalize_mismatches.txt` and the test
fails if that list grows. The 52 are four refusals the normalizer still emits:

| Refusal | Definitions |
|---|---|
| "a name the checker left unresolved" | 37 |
| "this template takes an expression" | 9 |
| "a window function's first argument is a window specification" | 3 |
| "no overload candidate accepts this argument" | 3 |

`docs/CORE.md` §1a carries the same table with what is missing behind each
refusal. The cut-over happens when that list is empty, and the constraint on it
is absolute: `tests/snapshots/pipeline_golden.txt` must stay byte identical and
the corpus snapshots and six-dialect agreement tests must keep passing.

`docs/CORE.md` states the normalizer's rules and the arena's constructors;
`docs/ARCHITECTURE.md` states the target shape and the transition.

## Remaining recommended steps

### 1. Separate relational optimization from SQL emission

Today `cagara-sql/src/lower.rs` decides two kinds of thing in one pass: which
`Rel` node becomes which SQL clause, and whether a stage *fuses* into the one
below it (`Stage::needs_barrier`, `wrap`, `derived`). The second is a relational
optimization — a question about the relation, not about SQL text — and it is
the reason a lowering rule sometimes has to read two levels of the tree.

The target is: `Rel -> Rel` rewrites (fuse stages, choose CTEs, hoist
predicates) as one pass over the arena, then a lowering pass whose every rule is
local to a node. The `HAVING` fold below is a case of the same problem, and the
"Deliberately absent" section states what it needs: a stage's phase as a
property of the node, not a fact recovered by matching the relation underneath.

### 2. Restore the aggregate-filter `HAVING` fold

A filter after `agg` currently wraps a derived table. Folding it into `HAVING`
is desirable, but it must be done through an explicit phase annotation on
relational nodes — "this predicate may run inside the aggregate's own clause" —
rather than by re-introducing a lowering rule that inspects the child. See
*Deliberately absent → Aggregate filters and `HAVING`* for the history and the
corpus snapshot that pins the current shape.

### 3. Retire the oracle once the corpus is broad enough

The oracle earns its keep while two independent implementations exist. A
candidate end state is the oracle replaced by a broader differential *test*
corpus: more programs, more SQL, judged against execution on SQLite and DuckDB
(the engines the shipped examples already run on in CI). That would let
`term_oracle.rs` go entirely. Until then it stays test-only, and the tests that
compare the routes are the reason it exists.
emitted SQL fails there rather than passing a structural comparison.

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
