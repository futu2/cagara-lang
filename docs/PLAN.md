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
  Each stage record also carries a **stable identity**
  (`check::NodeRef` — the definition the application was written in, and the
  application node's `ExprId` in it), which travels from the checker through
  normalization onto the reduced term and is preserved by saturation, inlining
  and substitution. The production stage check
  (`normalize::check_stage_records`) matches the lowering's report to the
  checker's record by that identity, having previously asked whether one span
  contained another; the identity is what tells a helper's stage from the
  caller's when the helper is inlined, since the caller records nothing at the
  use and the callee's stage names the callee.
  `check::query::TypedQuery` is the other half of that step: the query skeleton
  as its own arena, where a node carries its identity, its operation, a link to
  its typed child, and the rows the checker closed for it — so a later pass can
  read a stage's boundary instead of recovering it from a composed result type.
  It is not yet on the production path; `docs/TYPE-SYSTEM-REDESIGN.md` step 6
  lists the slices, and `check::query`'s own tests include the check that every
  fact its nodes need is available from the checker where a stage is applied.
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

## Query elaboration migration: plan and status

The end state is the one in [`TYPE-SYSTEM-REDESIGN.md`](TYPE-SYSTEM-REDESIGN.md):
`CheckedQuery` built from the checker's typed query skeleton rather than from a
normalized term, with no source walk and no `Facts` in the query path. The
sections above describe the code as it is; this one states the ordered plan and
how far it has got, so the next session does not have to reconstruct either from
the diffs.

| # | Step | Status |
|---|---|---|
| 1 | Stable `NodeRef` on normalized stage info; match by identity | **Done** |
| 2 | Internal typed query representation (`TypedQueryNode`) | **Done** |
| 3 | First typed lowering slice: table, prefix, suffix, omit, mapValue | **Done** |
| 4 | Run both routes; compare rows, stages, origins, relations, SQL | **Done** |
| 5 | Typed instantiation: helpers, aliases, imports, overloads, repeats | **Done** |
| 6 | Remaining constructs: joins/sets → select/update/agg → windows → expressions | **In progress** |
| 7 | Remove the source walk: `lower_query_recorded`, `check_stage_records`, `Facts` | Not started |

#### Step 6 progress: direct expression-bearing stages

The typed lowering now builds typed query nodes for direct table pipelines that
include `where`, `select`, `update`, `agg`, or `order`, and allows those stages
to mix with the already supported row rewrites and row-preserving stages. The
checker records their stage identity and input/output boundaries at application
time. `where` and `order` take their rows from the typed child; projections and
aggregates carry the checker's recorded output row. A direct typed `where` node
now also carries its checked predicate, and direct typed `order` nodes carry
their checked sort keys in source order. Direct `select`, `update`, and `agg`
nodes now also retain
their checked output field expressions, including window expressions in
projections. Typed lowering now hydrates expression payloads across the
completed typed graph from the matching checked query tree, so helper and
composed stages receive the same payloads as direct stages. When the typed and
checked shapes align, the checked relation is rebuilt from the typed arena;
the normalized route still supplies the comparison tree and expression
payloads for this migration slice, and removing that route remains later work.

`TypedQuery` now has ordered multi-input edges for joins and set operations, and
records each child's checked output row. Typed lowering constructs those nodes
recursively, including both child subtrees, and carries checked join predicates
on typed join nodes. Other expression payloads still use the normalized bridge.
Simple two-input set operations and joins now build their checked relation
directly from typed child queries; join predicates still come from the bridge.
Direct expression pipelines also assemble each checked stage from the typed
child and hydrated payloads, falling back to the normalized shape only when a
wrapper was absorbed from the stage record.
Helper application now instantiates recorded `where`, `order`, `limit`,
`offset`, `select`, `update`, and `agg` stages into typed nodes. Predicate,
sort-key, and projection payloads are copied onto those nodes, and their
checked stages are constructed directly. The normalized query remains the
source of instantiated helper payloads and intermediate projection rows;
helper-expanded checked output still retains normalized call-site origins.
Projection and aggregate helpers can also mix those stages with prefix,
suffix, omit, and map-value rewrites. Their instantiated checked rows provide
the intermediate boundaries needed to connect rewrites around projections.
At the production boundary, a checked relation rebuilt successfully from the
typed arena is now returned directly; the compiler no longer performs a second
normalized lowering and route comparison for that result. Partial typed shapes
keep the normalized checked tree as a template or fallback. The agreement
tests still compare both routes independently.
Direct pipelines made only from row rewrites and row-preserving stages now
stay entirely on that typed path even when the stages are interleaved; the
production regression covers prefix, suffix, omit, mapValue, limit, offset,
and distinct in one chain over an already typed query alias, and asserts that
the definition never enters normalized elaboration. The alias's original
typed stage chain and identities are copied into the consumer before its new
stages are appended.
Expression-bearing stages can now follow that same alias path: the copied
typed graph supplies the relational child and rows, while the existing
normalized bridge supplies only predicates, projections, and sort payloads.
Direct `where` stages whose predicates use plain fields, literals, comparisons,
membership lists, and boolean operators now build their checked expressions
from checker types and also bypass normalized elaboration.
Direct `select` and `update` stages whose fields use the plain expression
subset now have a source checked-expression path: expression types come from
the checker, and the typed projection and `CheckedQuery` are built without
entering normalized elaboration. Aggregate, window, and other unsupported
expressions continue through the payload bridge.
Direct `order` stages whose keys are plain columns with literal `asc` or
`desc` directions use the same typed path, retaining their checked sort keys
without entering normalized elaboration.
Window expressions in projection payloads are carried by the typed projection
node as `WinTemplate`, including when the pipeline starts from a typed query
alias. They are no longer deferred from this slice.
Projection helpers carry the same window payload through their instantiated
typed projection node, preserving the helper's stage identity and checked
output row.
The recursive slice also
accepts ordinary stages around a combine, so a filter, projection, row rewrite,
ordering, or limit can consume a join or set result and a staged query can be
used as either combine operand. Window expressions are retained in typed
projection payloads; the normalized bridge remains their payload source during
this migration slice.

The checker records join and set applications on the typed expression node,
including piped forms where the primitive is partially applied first. Each
record has a stable application identity, operation kind, both operand rows in
operand order, and the output row. Typed lowering consumes these records for
joins and sets over tables or typed query aliases, constructs both child
subtrees recursively, and checks the resulting output row against the checked
route. Ordinary stages can surround a combine and can occur inside either
operand; graph copying preserves unary stages when a typed alias is
instantiated. Row-rewrite helpers are instantiated inside combine operands as
well, with their definition identities preserved. Helpers whose stage
bodies preserve their row (`where`, `order`, `limit`, `offset`, or `distinct`)
are now instantiated inside combine operands too. A helper with one `select`,
`update`, or `agg`, surrounded by any number of those row-preserving stages,
can use the closed row on its call expression as the projection boundary. The
aggregate and projection expressions remain on the normalized bridge. Helpers
with multiple projection or aggregate stages now instantiate each intermediate
output row from the checked use expression, then build the complete typed chain
with the helper stages' original identities. Those helpers can also mix
projections with row rewrites, using instantiated checked rows at each boundary.

### Step 1, done

`check::NodeRef` is `(definition_module, definition, expr_id)`: the definition a
stage application was written in, and the application node's `ExprId` inside it.
It is **stamped where the stage is recorded, never derived from the term**, which
is what makes it survive saturation, inlining and substitution — see the
known-gaps entry above for the mechanism and the code path.

The production stage check (`normalize::check_stage_records`) now matches the
lowering's report to the checker's record by that identity and asserts a strict
one-to-one correspondence in both directions: every record is built exactly once,
and nothing is built under this definition's identity that was never recorded.
The span-interval test it replaced asked whether one span *contained* another,
which is a heuristic that holds on the programs measured and breaks as soon as
two applications share a span or a definition is inlined at two sites.

Witness: `compile::tests::a_stage_identity_names_the_node_that_applied_it` pins
the three properties the match rests on — two adjacent stages are two distinct
identities, a helper's stage keeps the *helper's* identity when the helper is
inlined, and a caller records nothing for a helper's use — and
`production_stage_check_handles_repeated_helpers_and_imports` keeps the
helper/import ownership boundary as regression coverage.

### Step 2, done

`check::query::TypedQuery` is the query skeleton as its own arena. A
`TypedQueryNode` carries its `NodeRef`, its `QueryNodeKind`, a link to its
**typed child** by `NodeId`, and the rows it read and produced as the checker
closed them. `QueryNodeKind` covers table, the four row rewrites, the
row-preserving stages, and the projections — every relational operation the
checked layer's `QueryKind` names.

Two invariants are enforced by the constructors rather than documented:
a rewrite refuses an input whose output row is not the row the stage read, and a
kind cannot be built through the constructor for a group it does not belong to.
Nothing in the type consults a final query type to work out a row.

Its constructors are deliberately partial in the same way the slices are: an
operation whose builder has not landed is a kind nothing constructs, not a kind
with a guess in it. The module carries a scoped `#![allow(dead_code)]` for that
reason, documented at the use site.

### Step 3, what it needs

The first builder is implemented for **direct, closed table pipelines** built
from `prefix`, `suffix`, `omit`, and `mapValue`: it builds a `TypedQuery` chain
from the checker's stage records, uses the recorded stage rows and witnesses,
and constructs the corresponding `CheckedQuery`. `mapValue` has a checked node
whose row records nullable/list wrapping while erasure passes through it, since
the wrapper is type-level and emits no SQL. The production path selects this
builder for the supported slice after comparing it with the term route; other
shapes still use the term route. Expression-bearing stages and general
table/base forms remain for subsequent work.

The comparison checks output rows, stage count/order/identity/kind, stage origins
and input/output rows, and the erased relation after ignoring `Rel::At` wrappers
(which carry route-specific source spans). A SQL backend integration test now
rebuilds every successful shipped/corpus query through the normalized route and
compares ANSI, PostgreSQL, SQLite, DuckDB, MySQL, and T-SQL output. This closes
the step 4 gate.

### Step 5, in progress

Typed lowering now follows query aliases to the definition that owns their
typed stage chain, preserves the original `NodeRef` identities, and checks
aliases against the normalized route. It also instantiates a closed helper
lambda at a use site: the helper's `prefix`, `suffix`, `omit`, and `mapValue`
stages are copied into the caller's typed arena over the caller's child.
Left-associated helper pipelines compose repeatedly, and imported helpers
retain the imported definition module in each stage identity. Focused tests
cover a signed helper, an alias followed by a helper, and an imported helper.

A single rewrite with an open helper row and a closed witness can use the
caller's recorded result row as its instantiated output. This covers a
row-polymorphic helper such as `q => q & prefix "h_"`, including repeated uses
of the same helper in one pipeline; its input is checked against the typed
child row and its result is still compared with the normalized route.

A single open row rewrite can also take its witness from a literal argument at
the helper call, as in `affix => q => q & prefix affix` or
`wrapper => q => q & mapValue wrapper`. The helper parameter is matched to the
stage witness, and the literal has already been checked at the call site.

Helpers with multiple supported row rewrites now instantiate each stage in
order from the typed child's current row. This supplies per-stage boundaries
when the helper's own rows are open, while checking any closed helper rows
against the instantiated rows. Each witness can be a closed helper literal or a
string literal passed to the corresponding helper parameter.

Overloaded query helpers use the checker's `Choice::Def` at the helper use
site, so the typed route instantiates the selected definition and retains its
stage identity. A test selects two overloads by their input rows and compares
both typed results with the normalized route.

The slice deliberately declines helpers with witnesses that cannot be tied to
a supplied literal, unsupported relational stages, and helper uses whose
choice remains an open `Choice::Hole`; those uses stay on the normalized term
route. Repeated direct stages are already covered by the first slice. Helper
bodies with expression-bearing relational stages also await their expression
builders in step 6.

### Step 6, first slice

Direct pipelines made only of integer literal `limit` and `offset` stages, or
the argument free `distinct` stage, now build typed row preserving nodes. The
builder derives each stage boundary from the typed child row and uses the
checked constructors for the count and the unchanged output row. The
normalized route remains the oracle, including its location independent
relation comparison. Expression bearing stages such as `where`, `select`,
`update`, `agg`, and `order` still use the term route until their checked
expression bridge is added.

Slice 1 is a **builder** that walks a definition's stage records and populates
the arena, then a `CheckedQuery` constructed from the tree. The feasibility claim
behind it is already pinned:
`check::query::tests::the_checker_supplies_every_fact_a_typed_query_node_needs`
asserts, off the checker's own records, that a stage's identity is a `NodeRef`,
that both of its rows are closed, and that its **input query is a typed child in
the same definition's tree** whose recorded row equals the stage's input row —
which is the link the builder follows and `TypedQuery::rewrite` validates.

Two things to know before starting it:

- **The expression arguments stay on the bridge.** A `where`'s predicate, a
  `select`'s fields, a join's `on` are `CheckedExpr`s built by the existing term
  lowering. Slice 1 reads operations, rows, resolutions and origins from typed
  nodes and takes its expressions from the bridge, exactly as step 6's slice 1
  says. The expression path is migrated last (step 6, group 4).
- **`TypeCheck::stage_node` is a linear scan** over a definition's stage list,
  which the bridge calls once per application. That is fine at the current call
  rate and a builder walking a whole chain should index the list instead of
  scanning it per node.

### Step 4, the oracle

Until the new slice passes the corpus and the incremental-compilation tests, the
term route stays authoritative. The comparison is over output rows, stage order,
each stage's input/output rows, origins, checked relation structure, and
generated SQL — the last of which the corpus snapshot
(`cagara-hir/tests/corpus/snapshots/`, driven by `cagara-sql/tests/corpus.rs`)
already pins byte-for-byte, and which is why a change that silently alters
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
