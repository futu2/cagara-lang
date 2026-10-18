# Type system and pipeline redesign

## Current implementation status

The redesign described here is implemented, and the pipeline has moved on from
it. Production routes every accepted query through `TypedQuery` and
checker-owned expression payloads, with no normalized query fallback; that work
is recorded as the completed migration in [`PLAN.md`](PLAN.md). What replaces
it next is the normalizer in `crates/cagara-hir/src/normalize/`, whose rules and arena
are stated in [`CORE.md`](CORE.md); [`PLAN.md`](PLAN.md), "Consolidating the
normalizer", says where that stands. The normalized term machinery is a test-only
oracle in both shapes. See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the current
layers.

## Why change it

The deferred `prefix`/`suffix` defect is a symptom of a representation mismatch.
Inference sees a whole pipeline as a nested row type, while each deferred stage
constraint separately tries to recover the stage's own input/output relationship
from that whole type. When inference has already composed stages, the constraint
can no longer tell which `keyMap` layer belongs to it. It either identifies the
wrong row and fails an occurs check, or accepts a row that does not represent the
stage's result.

The checker used to leave facts in several maps (`use_tys`, resolutions, overload
choices, schemes) for normalization and elaboration to recover in later passes.
Those maps are gone — the typed tree carries the facts, keyed by `ExprId` or by
the span a use was filed under — but the recovery is still a second pass:
production walks the AST and asks the tree per node. A fact and the checked node
it justifies can still get out of step, so the redesign should make the typed
relational result the product of checking, with stage boundaries represented
explicitly from the moment the stage is inferred.

## Goal and invariant

Move to one semantic pipeline:

```text
source -> parse/resolve -> bidirectional checking + typed elaboration
       -> CheckedQuery / CheckedExpr -> Rel erasure -> SQL
```

The key invariant is:

> Every query-stage application owns its input row, operation, and output row.
> Its output is computed or constrained by that operation exactly once. No
> later pass recovers the stage boundary by inspecting a composed result type.

For example, two deferred prefixes must be represented as two adjacent typed
nodes:

```text
Prefix(p, input=r, output=keyMap(p, r))
Prefix(p, input=keyMap(p, r), output=keyMap(p, keyMap(p, r)))
```

The second stage consumes the first stage's output. It must never equate the
inner `keyMap` with the original `r`.

## Target representations

### Types

Keep Hindley–Milner generalization for ordinary functions and scalar types, but
separate row algebra from general `Ty` constructor recursion. Use explicit kinds
and row terms:

```text
Type   = Var | Scalar | Maybe(Type) | List(Type) | Expr(Phase, Row, Type)
       | Query(Row) | Function(Type, Type) | ...

Row    = Var | Empty | Fields([(Label, Type)], Row)
       | MapKeys(KeyMap, Row) | Remove(Label, Row)
       | Merge(Row, Row) | MapValues(ValueMap, Row)
```

Rows stay symbolic when inputs are open. A single row reducer owns flattening,
composition, label collisions, and closed-row materialization. It returns a
structured result (`Reduced`, `Open`, or a specific row error), never a plausible
partial row. Row equality and row operations must not silently treat an
unreduced term as equal merely because it cannot yet be inspected.

`MapKeys` remains an operation in a row expression, not a mutable constraint
that guesses which layer of another expression belongs to it. Canonicalization
may compose adjacent mappers, but it must preserve the ordered stage nodes and
the identity of each boundary.

### Typed expression and query

Checking returns both a type and a typed node:

```text
infer : Env -> Expr -> Result<(Subst, Type, TypedExpr), TypeError>
check : Env -> Expr -> Type -> Result<(Subst, TypedExpr), TypeError>
```

`TypedExpr` carries the resolved name, source identity, scalar type, phase, and
overload evidence needed by later phases. `TypedQuery` is built during this same
walk. Its private nodes retain stage origins and explicit boundaries; constructors
remain the only way to make nodes, preserving the current `CheckedQuery`
guarantees.

Ordinary functions and helper definitions remain polymorphic schemes. At a use,
instantiate their scheme, check the arguments, and elaborate the instantiated
body with the use-site substitutions and overload evidence. Helpers that return
stages therefore compose typed stage nodes; they do not have to encode the
runtime composition by reverse-engineering their final `query` type.

## Constraint-solving rules

1. **Stage inputs are explicit.** A stage application is given its actual input
   query node and input row before its output is checked.
2. **Each stage owns its row rule.** `where`, `order`, `limit`, and `distinct`
   preserve the exact input row; `select` produces its field row; `update`
   merges rows; `omit` removes one known label; `prefix`/`suffix` construct a
   `MapKeys` row expression; joins and set operations use their existing row
   rules.
3. **Open information remains explicit.** A rule that needs a closed row waits
   on a typed obligation attached to that exact node. Obligations are retried
   against their operands, never against a surrounding function's final result.
4. **Errors retain ownership.** Missing columns, phase errors, invalid mapped
   labels, and signature mismatches identify the operation or annotation that
   introduced the obligation.
5. **Signatures check the inferred result.** An annotation constrains the
   definition's typed result after body checking. It does not replace the body's
   stage tree or silently narrow a produced row.
6. **No successful-but-pending equality.** A row equality that is open is
   represented as pending with its source owner. It is not returned as success
   and then assumed to have been validated by a different constraint.

## Pipeline boundaries

The current shell and SQL backend need not be redesigned with the checker.
Keep `Workspace`, Salsa queries, diagnostics, incremental compilation, SQL AST
lowering, and dialect rewrites. Replace the semantic middle in stages:

```text
Current: AST -> TypeCheck facts -> normalized Term -> CheckedQuery -> Rel
Target:  AST -> TypedExpr / CheckedQuery --------------------------> Rel
```

The normalized term implementation remains an independent test oracle. It
does not participate in production query compilation and can be removed after
the agreement tests no longer need it.

## Migration plan

### 1. Freeze the behavior and failure

Add regression cases for the two reported programs, plus three and mixed
`prefix`/`suffix` stages, reuse at two different literal affixes, signatures,
projection after rename, and failures for collisions or missing columns. Assert
both inferred rows and successful `CheckedQuery`/SQL output where applicable.
Capture the current passing workspace suite as the compatibility baseline.

### 2. Make row algebra a small independent subsystem

Introduce the explicit `Row` representation and reducer behind adapters for the
current `Ty` row constructors. Property-test mapper composition and compare
closed results with the schema functions used by checked IR. Test open rows by
binding tails in different orders. Do not change inference behavior in this
step.

### 3. Introduce typed stage nodes at application sites

Build a typed node for each primitive-stage application, recording the actual
input node/row and the operation's output expression. Initially keep existing
scheme inference and derive the typed node from the application path. Add an
assertion in debug/tests that the stage node's output row agrees with the
instantiated signature. This makes disagreements local and observable.

Done: the stage record is a field on the application's `TypedExpr`
(`TypedExpr::stage`), and the body keeps the application order in an index
(`TypedBody::by_stage`) because a chain is recorded in application order while
the arena follows source order. `TypeCheck::row_stages` reads it, and
`check::stage::finish_row_stages` carries the assertion.

### 4. Move expression facts into the typed tree

Return `TypedExpr` from checking and carry resolved definitions, overload
choices, phase, scalar type, and source locations there. Adapt normalization and
checked construction to consume those facts. Keep the public `TypeCheck` query
surface as a derived compatibility view during the transition.

Done for the facts: the tree carries the type, row, printed form, resolution,
choices, and stage of every expression, and the maps that published them are
deleted. The compatibility view is derived from the tree rather than kept
alongside it. What has not moved is the *walk*: normalization still reads
syntax for each node's shape and asks the tree for its facts. The two have been
shown to cover the same nodes, except a projection's base, which the projection
node itself resolves.

### 5. Replace deferred stage equations

Stage obligations now live on typed stage nodes. The checker keeps the row
constraints needed for inference, while typed lowering consumes the closed
input/output rows and preserves their source ownership at each use.

### 6. Remove duplicate semantic paths

Route source compilation through typed checking directly to `CheckedQuery`.
The production route is now typed checking to `CheckedQuery`; erasure remains
structural and SQL lowering remains type-agnostic.

The migration is complete: checking returns a typed tree that carries the type,
row, printed form, resolution, overload choices, and stage of every expression;
no per-expression compatibility map or production normalized walk remains.
Normalized traversal is retained only as an independent test oracle.

#### Why the normalized oracle remains

The test oracle can build a `CheckedQuery` by normalizing source to a `Term`:
`elaborate_query_via_terms` walks the AST through the bridge
(`normalize::expression`), which resolves names, inlines definitions, and
reduces applications, and `lower_query` reads the finished term. Production
query compilation instead consumes `TypedQuery` and checker-owned payloads.

A walk over the typed tree cannot simply replace it. The tree deliberately does
not copy syntax — a node carries its `ExprId` and span, and a consumer that
needs a node's *shape* reads the AST node it already has — so a tree-driven walk
still consults the AST for each node's kind. Taking children from
`TypedBody::children` instead of `child_exprs` yields the same nodes in the same
order, which
`compile::tests::the_bridge_visits_every_node_the_typed_tree_holds_except_projection_bases`
already pins. Re-pointing the walk alone buys nothing.

What the tree *does* add is the query skeleton: a definition's stage
applications, each with the operation it applied and the rows its own signature
instance read and produced (`TypedExpr::stage`, ordered by `TypedBody::by_stage`).
That is the boundary this step exists to make authoritative, instead of
recovering it from a composed result type. `check::query::TypedQuery` is that
skeleton given its own shape — a chain of nodes linked to their typed children,
each owning its rows — and `check::NodeRef` is the name that lets a lowered
stage be matched to the application that produced it. See "What is already in
place" below.

#### The composition model

A definition's tree records only the stages *it* applies directly. A helper's
scheme carries no stage, so a use of a helper records nothing at the call site —
the stages it applies are in its own tree. Composition therefore happens at the
use: elaborating a call means elaborating the callee's tree with the use site's
arguments, which is what inlining already does, and it is why the tree alone
cannot answer "what stages does this query run" without it.

- **Holes**: the candidate for a use is on the call node's `choices`, so the
  assignment travels with the node rather than through the term's substitution
  stack.
- **Expressions**: predicates, projections, aggregate inputs, and scalar
  arguments are checker-owned typed payloads attached to query nodes. The
  bridge remains only for independent agreement tests.
- **Bases**: a table, join, or set operation is an application whose arguments
  are syntax (table names, join predicates). Its *row* is on the node
  (`TypedExpr::view`), and its arguments come from the AST as they do now.

#### Slices, each with its own witness

1. **A direct pipeline over a base.** Build `CheckedQuery` from the recorded
   stage chain, taking each stage.s input and output rows and expression
   arguments from checker-owned typed nodes. Witness: corpus and example
   snapshots, plus
   `shipped_programs_stage_nodes_agree_with_the_relational_rows` and the
   query-lowering agreement test, which compare against an outside source.
2. **Helper-composed pipelines.** Compose the callee's tree at the use, as
   inlining does today. Witness: the same tests, whose cases already include a
   helper reused at two labels and a chain through an alias.
3. **The regression inventory.** `compile::tests::the_normalized_route_handles_every_construct`
   and the corpus agreement suites cover the accepted language shapes.
4. Keep normalized query lowering isolated to agreement tests as an independent
   oracle; it is not a production migration step.

#### What is already in place

Two pieces of the machinery the slices need have landed, and they are the two
the slices would otherwise have had to invent:

* **Stable node identity.** `check::NodeRef` is `(module, definition, expr_id)`:
  the definition a stage application was written in, and the application node's
  `ExprId` inside it. The checker stamps it on every stage it records
  (`PendingRowStage::node`), and typed lowering carries it directly into the
  corresponding typed query node. A helper's stage keeps the helper's identity
  when it is instantiated into a caller, which makes the ownership boundary
  exact: the caller's check does not claim a stage it did not apply. The legacy
  normalized adapter no longer carries a second identity report or compatibility
  walk.
* **The typed query arena.** `check::query::TypedQuery` holds `TypedQueryNode`s
  built *where the stage is applied*, each carrying its `NodeRef`, its kind
  (`QueryNodeKind`: table, the four row rewrites, the row-preserving stages, and
  the projections), a link to its **typed child** by `NodeId`, and the rows it
  read and produced as the checker closed them. Nothing in it consults a final
  query type to work out a row — `output_row` is the node's own answer, and the
  constructor that builds a rewrite refuses an input whose output row is not the
  row the stage read. The kinds and constructors cover every relational
  operation the `QueryKind` enum names; slice 1 fills the projections' expression
  arguments in from the bridge.

The constructors are partial in exactly the way the slices are: an operation
whose builder has not landed is a kind nothing constructs, not a kind with a
guess in it.

#### What must not change

- A `CheckedQuery` is still only made by its constructors, so the row a node
  reports is the row its operation produces.
- A stage's rows are still its *own* signature instance, never recovered from a
  composed result type — the property `check::stage::finish_row_stages` asserts.
- Erasure stays structural and SQL lowering stays type-agnostic.

The migration rewrote the query half of elaboration while keeping the corpus as
the behavior witness. The typed graph owns production query structure, stage
boundaries, and expression payloads end to end. The normalized adapter is
test-only.

## Acceptance criteria

- `two = p => q => q & prefix p & prefix p` checks and maps every label twice.
- `two = p => q => q & prefix p & prefix "z_"` checks and preserves both
  renames in order.
- A helper reused with different labels has independent rows at each use.
- Literal-only chains, mixed prefix/suffix, open rows, aliases, partial
  application, and signatures retain their current correct behavior.
- Invalid signatures, missing columns, mapper collisions, phases, and overloads
  still fail at the right source location.
- Every accepted query produces a `CheckedQuery` whose row agrees with erasure
  and schema validation; no accepted construct needs a source-level fallback.
- `cargo test --workspace` and `cargo check --workspace` pass, and the examples
  produce unchanged SQL except for explicitly approved behavior changes.

## Non-goals

This is not a move to a general-purpose dependently typed language, user-defined
type constructors, or higher-kinded row functions. It does not change SQL
lowering or dialect semantics. It replaces how this language represents and
checks its existing operations, with row operations kept first-order and
compiler-owned.
