# Compiler architecture

Cagara's semantic pipeline is a compiler, not an interpreter:

```text
source -> syntax/AST -> resolve + bidirectional type checking
       -> typed core (check/typed.rs: one typed node per expression)
       -> normalizer (normalize/: beta-reduction, aliases, helpers, overloads)
       -> typed relational arena (normalize/arena.rs)
       -> erasure -> Rel
       -> schema validation -> SQL lowering and dialect rewriting -> SQL
```

The normalizer's rules, the arena's constructors, and the SQL-observable
semantics (bags, nullability, phases, ordering, `sql` templates) are stated in
[`CORE.md`](CORE.md).

**Production during the transition.** Today production is the source-directed
lowering in `typed_lower.rs`, called through `query_lowering.rs`; it builds a
`TypedQuery` graph and a `CheckedQuery` tree and has to keep the two in step
(`check/query.rs::TypedQuery::carry_checked_payloads`, `rebuild_checked`).
`normalize/` is the incoming route: every module of it exists — including the
evaluate-and-erase boundary, `normalize/route.rs::lower` — but `elaborate.rs` does
not call it yet, so the two are compared rather than one replacing the other.
`tests/normalize_differential.rs` runs both over every query definition in the
examples, the HIR corpus and the inline shapes and currently reports 47 of 99
agreeing; the mismatching definitions are recorded in
`tests/snapshots/normalize_mismatches.txt` and the test refuses to let the list grow.
The production route's own output is pinned by
`tests/snapshots/pipeline_golden.txt` (driven by `tests/pipeline_golden.rs`):
every example and corpus program's erased `Rel` (locations included),
fault class, diagnostics with their locations, and the checked tree's kinds,
origins and rows. `docs/CORE.md` §1a lists the refusals still standing between the two.

## Semantic layers

```text
AST            names, source spans, and user syntax
TypeCheck      inferred schemes, typed expressions, rows, and overload choices
Typed core     check/typed.rs: TypedBody, TypedExpr — one node per expression,
               carrying its type, row, resolution, choices and stage record
Normalization     normalize/: values, closures, environments, and the reduction rules
Arena          normalize/arena.rs: relational nodes whose constructors are the rules
Rel            backend-facing relational structure without type metadata
```

The checker records one typed node for every expression. A node carries its
whole type, its row when it is query-shaped, what its name resolved to, the
overload choices recorded at that use site, its source span, and — for a stage
application — the operation and the rows its own signature instance read and
produced. `TypedBody` indexes those nodes by `ExprId` and by the span the checker
filed a use under, and keeps stage applications in application order, which is
not the arena's source order. A `NodeRef` identifies the definition and
application node where a stage was written, so helper instantiation preserves
ownership and source identity.

The normalizer reduces that core. A name evaluates the definition the checker
resolved it to, so aliases disappear without a separate walk; a lambda closes
over an environment, so application is beta-reduction and no argument can be
captured; a `primitive "…"` body evaluates to a function value that accumulates
its arguments and constructs its relational node when the last one arrives. One
`Control` type states the only two ways a reduction stops: a construct the
normalizer has no rule for (a compiler capability gap) and a construct whose rule
refused what it was given (a program error). The per-attempt `Normalizer` owns its
arena, its active-definition path and its budget, so incremental or concurrent
compilations share no lowering state.

In the arena, a node records its operation, its site, its origin, its children in
operand order, the row its constructor computed, and exactly one payload shape
for its kind. Constructors are the single source of relational validity: a
filter takes a boolean row-phase predicate over the input's columns, a projection
computes its output row from its fields, a join requires side-qualified columns
and wraps the nullable side's types, a set operation requires equal rows. No
constructor recovers a row from a final query type. Row formers live in
`core.rs::RowType`; phase and side rules live in `rules.rs` and are shared with
the checker's types, so one rule cannot be stated twice in two ways.

`CheckedQuery` and `CheckedExpr` (`checked.rs`) are what production builds today
and what the arena is designed to subsume: both are immutable trees whose
constructors are the relational rules, and both erase structurally to `Rel`.
Erasure (`normalize/arena.rs::CompiledQuery::erase`, `checked::erase`) walks the
structure and emits `Rel`, stamping each node with its origin; it infers
nothing. SQL lowering sees only `Rel` and therefore cannot silently become a
second type checker.

The normalized term module (`term_oracle.rs`) is a **test-only oracle**: its own
`Term` calculus, its own textual capture-avoiding substituter, and its own query
lowering by primitive name. It has no production role, and its independence is
the point — the agreement tests drive it directly for the same programs the
compiler compiles, so the two routes can disagree.

## Checker and incremental shell

Type checking is bidirectional: `infer` synthesizes a type and `check` pushes an
expected type down. That lets a lambda parameter receive its type from a known
function type instead of guessing from its body. The checker owns rows, phases,
SQL template selection, overload choices, and stage identities; downstream
passes consume those facts rather than re-deriving them. Facts more than one
layer reads — `Resolved`, `TemplateKind`, `DefId` — live in
`check/core_facts.rs`, not in the oracle.

`Workspace` loads files, imports, overlays, and the embedded prelude. Salsa
memoizes parsing, resolution, and type checking for editor updates. `compile`
is the shared boundary used by CLI and LSP to turn an immutable compiler input
into erased relations and diagnostics. Cache ownership and invalidation live
in `incremental.rs` and `Workspace`; compilation and erasure consume read-only
inputs. Moved definitions are rebuilt so `Rel::At` spans stay current.

The compilation result is:

```text
compile : Workspace -> Compilation

Compilation {
  queries: [CompiledQuery],
  diagnostics: [Diag]
}
```

## Invariants and validation

- A query node's row is the output of its relational constructor.
- Every source column reference is checked against the row available there.
- Phase restrictions are enforced before erasure.
- Outer-join nullability and set-operation compatibility are represented in
  output rows.
- Erasure is structural and leaves no type-checking work to SQL lowering.
- Every accepted query is produced by one production route; there is no
  normalized fallback.
- The oracle agreement tests compare rows, stage order and identity, origins,
  relation structure, and generated SQL across dialects.
- The pipeline snapshot pins every example and corpus program's erased `Rel`,
  fault class, diagnostics, and checked tree, so a change of production route
  that alters behaviour fails there.
- Corpus, shipped-example, incremental, and SQL tests reject compiler gaps and
  preserve source diagnostics across edits.

These checks distinguish a language construct with a lowering rule from a
compiler capability gap. Adding a construct requires a checker record, an
normalizer rule that constructs its node, constructor coverage for that node, and
a regression before it is considered accepted.

## Relational erasure and SQL

The arena and the checked tree carry what is needed while building a valid
query: rows, expression phases, types, and origins. Erasure emits `Rel`
structurally and preserves source origins, including walking through operations
with no SQL form (`mapValue` changes the checked row and nothing else). The
schema validator remains a defensive check over `Rel` and can validate
relations assembled by external callers; it is not a second source of type
information. SQL lowering and dialect rewriting operate after erasure.

## Design boundaries

The pure compiler kernel consumes `CompilerInput` and produces owned results.
The shell owns file access, diagnostics, Salsa state, overlays, and SQL options.
No runtime evaluation is performed during compilation: the normalizer evaluates
*programs into queries*, and the values it produces are aliases, closures, labels
and relational nodes — never data. Relational construction is the only source of
validity rules, while the normalized term implementation is isolated as a
removable, test-only semantic oracle.
