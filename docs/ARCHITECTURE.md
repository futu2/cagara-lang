# A smaller architecture

Cagara already has the right language-level ideas: rows are typed, query
stages are ordinary functions, and SQL is a backend for a relational value.
The implementation is harder to explain than those ideas because the compiler
kernel is mixed with incremental loading, interpretation of the prelude,
diagnostic recovery, and SQL optimisation.

This document describes a smaller architecture for the kernel. It is a target
for the implementation and a possible paper presentation. The existing
workspace remains useful as an adapter around that kernel.

## The boundary

The paper should present one pure pipeline:

```text
source
  -> parsed modules
  -> resolved modules
  -> typed core
  -> typed relational IR
  -> SQL AST
  -> dialect SQL
```

Each arrow is a function. Errors are values returned by the function; no phase
updates a global table, reads a file, or prints a diagnostic.

The editor and command-line layers sit outside this pipeline:

```text
files, overlays, Salsa cache, LSP state
  -> immutable module snapshot
  -> pure compiler kernel
  -> diagnostics and SQL
```

Salsa is therefore an optimisation of the adapter, not part of the language
semantics. A paper implementation can omit it and use the same kernel API.

## The kernel data types

The kernel needs four representations. It should not use one representation
for all four jobs.

```text
Surface      parser output; names and source spans are still present
Resolved     names are identifiers, never strings looked up during evaluation
TypedCore    every expression and query carries its inferred type and origin
Relational   only query constructors and SQL expressions remain
```

The important distinction is between a typed term and a runtime value. The
compiler does not need a general interpreter for the prelude. Elaboration turns
applications of query operations and SQL templates into constructors in
`Relational`; ordinary user functions are beta-reduced by a small pure
normalizer before that point.

A conceptual core is:

```text
Term ::= Var Id
       | Lit Literal
       | Lam Id Term
       | App Term Term
       | Record [(Label, Term)]
       | SqlTemplate Template
       | Query QueryTerm

QueryTerm ::= Table TableName Row
            | Where QueryTerm Expr
            | Select QueryTerm [(Label, Expr)]
            | Update QueryTerm [(Label, Expr)]
            | Aggregate QueryTerm [(Label, AggExpr)]
            | Order QueryTerm [OrderKey]
            | Limit QueryTerm Int
            | Join JoinKind QueryTerm QueryTerm Predicate
            | Set SetKind QueryTerm QueryTerm

Expr ::= Column Side Label
       | Constant Literal
       | Scalar Template [Expr]
       | Aggregate Template [RowExpr]
       | Window Template WindowSpec [RowExpr]
       | Group RowExpr
```

`QueryTerm` is not an untyped `Rel`. It is constructed with an output row and
an origin:

```text
Query { row: Row, origin: Origin, node: QueryNode }
Expr  { phase: Phase, ty: ScalarTy, origin: Origin, node: ExprNode }
```

Constructors check their local rule and return `Result`. For example,
`where` accepts a row-phase boolean expression, `aggregate` accepts only
row-phase arguments, and a left join applies `maybe` to the right row. Once a
constructor returns, the SQL backend can rely on those invariants. The current
`schema` pass becomes a small debug validator rather than a second semantic
checker.

Rows are still extensible, but row operations are normal forms in the type
checker rather than executable values:

```text
Row ::= Empty
      | Extend Label Ty Row
      | RowVar Var
      | Merge Row Row
      | MapKey KeyMap Row
      | MapValue ValueMap Row
```

The checker normalises these terms and emits a closed row for each query that
reaches SQL. The evaluator does not need `MapKey`, `MapValue`, or `Merge` at
runtime.

## Type checking as a pure calculation

The type checker may use an efficient mutable union-find internally, but its
semantic interface should be a state transition with no hidden inputs:

```text
infer : Env -> SurfaceExpr -> Result (TypedExpr, Env, [TypeError])
```

In a paper presentation this is written with immutable substitutions:

```text
unify  : Subst -> Ty -> Ty -> Result Subst
infer  : Env -> Subst -> Term -> Result (Subst, Ty, CoreTerm)
```

The current `trail`, rollback, and overload fitting are implementation details
of this calculation. They should not leak into the core data model. An
overloaded name elaborates to a selected definition (or an explicit dictionary
argument); evaluation must never discover overload choices dynamically.

A module result should be a value such as:

```text
CheckedModule {
  defs: [CheckedDef],
  exports: Scope,
  errors: [Diagnostic]
}
```

`CheckedDef` contains its scheme, typed core body, and the choices made at its
uses. This removes the current split where `TypeCheck` stores choices in one
map and `Evaluator` later interprets the original AST to consume them.

## Elaboration replaces the dynamic evaluator

The current `Value` enum and `Evaluator` are useful for bootstrapping the
language, but they are the largest source of accidental complexity:

* `Value` contains closures, partially applied primitives, templates, query
  IR, frames, and bounds in one open-ended sum type.
* the evaluator walks the source AST again, performs dynamic shape checks, and
  attaches source locations after applications have already happened.
* the prelude is interpreted to find the relational constructor hidden behind a
  primitive name.

The target is an elaborator over `TypedCore`:

```text
elaborate : TypedCore -> Result Relational
```

Prelude declarations remain a convenient user-facing library, but the
elaborator resolves their known definitions once. `table`, `where`, `select`,
joins, aggregates, and templates become explicit core constructors. A user
function is represented as a core lambda and normalised with lexical
substitution; it is not represented as an `Rc` closure with an environment in
the relational phase.

This also gives a clean recursion rule. Definitions form an acyclic dependency
graph for compilation. A recursive reference is rejected during resolution or
normalisation, before any evaluation depth counter is needed.

## One place for relational validity

Today a stage is constrained in three places: row equations in the checker,
runtime checks in `prims`, and column/phase checks in `schema`. The target has
one authoritative layer: typed relational constructors.

For example, `select` has the following semantic shape:

```text
select : Query r -> Fields r s -> Query s
```

where `Fields r s` can only be built from row expressions referring to `r`.
`agg` takes `AggFields r s`; `join` takes a predicate over `Join r s`; and a
left join returns `Merge r (MapValue Nullable s)`. The checker proves the
premises and the constructor records the result. SQL lowering only traverses
the resulting value.

Source spans should be an `Origin` on every node, rather than a transparent
`Rel::At` wrapper that every later phase has to remember to unwrap:

```text
Origin { module: ModuleId, span: Span }
```

An error can therefore point to the node that created the invalid operation
without changing the relational tree's shape.

## SQL lowering without hidden state

SQL lowering has two separate jobs and should expose both:

```text
plan_names : Relational -> NamePlan
emit       : NamePlan -> Relational -> Result SqlAst
```

`NamePlan` assigns aliases, hidden sort columns, and CTE names in a deterministic
preorder. `emit` is then a pure traversal. This replaces the mutable
`Lowerer` counters, CTE stack, and use-count bookkeeping while retaining the
same CTE and derived-table decisions. Dialect rewriting remains a pure
`SqlAst -> Result SqlAst` pass.

The implementation can later use a state-passing writer for speed; the paper
model stays the two functions above and makes the generated names deterministic
for a given relational value.

## The shell around the kernel

`Workspace` should become an adapter with one mutable concern: maintaining
open documents and the module cache for an editor. Its public operation is a
snapshot:

```text
snapshot : Workspace -> ModuleSnapshot
compile  : ModuleSnapshot -> Compilation
```

`ModuleSnapshot` owns canonical paths, source text, import edges, and source
locations. It does not own inferred types, evaluated values, or SQL state.
`set_source` returns a new snapshot (the LSP server may replace its current
one); Salsa can memoize the pure functions used to build the snapshot.

The CLI and LSP then share one small reporting adapter:

```text
report : Compilation -> [Diagnostic]
```

They should not call `check`, `root_queries_checked`, and SQL compilation as
three independent passes with partially overlapping error policies.

## Migration sequence

The architecture can be reached without a flag day.

1. Introduce `core` types for `Origin`, `Query`, `Expr`, and closed rows. Keep
   the existing `Rel` as a compatibility conversion.
2. Move phase, column, join-side, and output-row checks into the core
   constructors. Keep `schema` as a debug assertion and compare its result in
   tests while the conversion is in place.
3. Make type checking return typed definitions, including resolved overload
   choices. Add an elaboration pass that consumes those definitions.
4. Replace `Evaluator`, `Value`, and `prims::call` in the production path with
   the elaborator. Keep the old evaluator only for compatibility tests until
   all examples use the new path.
5. Add `plan_names` and make SQL emission consume the plan. Remove mutable
   counters from the backend after SQL snapshots match existing tests.
6. Reduce `Workspace` to the shell adapter and expose a pure `compile` entry
   point for the CLI, tests, and a future paper artifact.

At each step the existing examples and SQL golden tests remain the behavioral
contract. The paper artifact can be cut at step 3 or 4: it already has a pure
surface-to-typed-core story without depending on Salsa, filesystem loading,
LSP state, or a SQL dialect library.

## Claims the smaller design can support

The architecture makes the useful correctness claims local and testable:

* **Phase safety:** a relational constructor can only be built from an
  expression in its permitted phase.
* **Row safety:** every column reference in a constructor belongs to its input
  row, and every output row is the constructor's declared row transformation.
* **Null safety:** nullable sides of outer joins and nullable aggregate
  results are represented by `maybe` in the row type.
* **Backend preservation:** SQL lowering is a homomorphic traversal of the
  validated relational value; it does not infer missing columns or phase rules.
* **Determinism:** compilation depends only on the module snapshot and explicit
  compiler options. Name planning is deterministic, so repeated compilation
  produces the same SQL text.

These are stronger paper statements than "the evaluator and schema validator
currently agree" and they reduce the amount of implementation a reader must
hold in mind.
