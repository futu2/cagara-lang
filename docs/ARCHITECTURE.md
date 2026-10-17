# A smaller architecture

Cagara's semantic pipeline is a compiler, not an interpreter:

```text
source -> syntax -> typed checking
       -> typed query lowering -> CheckedQuery -> erasure -> Rel
       -> SQL AST -> dialect SQL
                \-> normalized query adapter -> CheckedQuery (transitional)
```

The relational IR is untyped. Type checking establishes scalar types, rows,
phases, and overload choices. Typed query lowering consumes checker-owned stage
boundaries and builds checked relational constructors for supported shapes.
The normalized term adapter still resolves names, inlines definitions, and
reduces applications for shapes that have not migrated. Erasure drops checked
metadata at the SQL boundary.

## Semantic layers

The implementation has six semantic representations:

```text
AST            names, source spans, and user syntax
TypeCheck      inferred schemes, typed expressions, rows, and overload choices
TypedQuery     query operations, typed inputs, and checker-owned row boundaries
Term           a transitional query normal form, built from TypeCheck's facts
CheckedQuery   relational structure plus checked rows, phases, and origins
Rel            backend-facing relational structure without type metadata
```

`Term` remains a small calculus — local binders, columns, literals, templates,
lambdas, applications, `case`, records, lists, and relational primitives — with
an application reduction that substitutes arguments for parameters. It is now a
transitional query adapter rather than the compiler's semantic center. Typed
query lowering owns the relational skeleton and stage boundaries; the adapter
is used only for shapes whose expression payload or helper instantiation has not
migrated yet. Names resolve and applications reduce once in that adapter instead
of being rediscovered by each backend consumer.

A typed query node carries the **row it produces**, taken from the checker at
that application. It has to be the node's own row and not the declaring
definition's signature, because a definition can inline two tables —
`innerJoin orders (.<id == .>uid)` over a joined pair — and each needs its own
columns. The typed graph is the boundary source for migrated shapes. The legacy
term adapter carries the same per-application row fact for shapes still using
that route.

In the legacy adapter, the shapes are uniform enough to state: every stage is
`Primitive "__stage" [own arguments…, piped input]`, in declaration order with the
piped input last. `users & where p` is `__where [p, __table [schema, name]]`; a
join is `__innerJoin [right, on, input]`. Pipe syntax, operator sugar, aliases and
composition are already gone by the time a term exists, which is why lowering is
mechanical and why a stage *value* — a partially applied primitive, or a lambda
over one — needs no case of its own: applying it is the reduction the normalizer
already performs.

`CheckedQuery` and `CheckedExpr` are immutable trees. Query nodes own their
structure, while unchanged output rows are shared through `Arc`; computed rows
remain immutable after construction. Their constructors
enforce local rules: predicates must be boolean and row-phase, expressions must
refer to available columns, projections compute their output rows, joins compute
nullable sides, and set operations require compatible rows. The node enums are
private so callers cannot bypass those constructors.

Ordinary functions in the legacy adapter are inlined by reduction, not called.
Typed query helpers are instantiated by the checker-owned query graph. A
definition that names itself is reported as recursion rather than expanded
without end: the typed route carries an explicit per-attempt lowering context,
and the legacy normalizer keeps its own reduction stack for the adapter path.

Nothing here evaluates. The typed graph or legacy adapter produces a
`CheckedQuery`; SQL lowering never sees an intermediate value representation.

The type checker records facts in `TypeCheck` and uses mutable inference state
internally. Checking is bidirectional: `infer` synthesizes a type, and `check`
pushes an expected type down. That direction is what lets a lambda's parameter
type come from a known function type rather than being guessed from the body.

How much that direction buys is measured rather than assumed. With the checking
overrides for records, lambdas, and lists disabled so that every expression took
the synthesize-then-coerce path, the checker's test suite still passed: the
constraint solver's worklist defers a comparison and re-runs it, recovering by
scheduling what checking recovers by direction. The direction becomes
load-bearing once that deferral is removed, because an expectation that now
arrives up front would have nowhere to wait.

A paper formulation can describe the same rules with immutable substitutions and
a result value:

```text
infer : Env -> Subst -> Expr -> Result (Subst, Type, TypedExpr)
```

That presentation is a model for the semantics, not a claim about the current
implementation's internal data structures.

## Relational erasure

The checked tree carries rows, phases, types, and origins required while
constructing a valid query. `erase` traverses it structurally and emits `Rel`.
It does not infer types or recover rows. An unresolved call is rejected at
erasure because every function application must be expanded before SQL
lowering.

The schema validator still runs at the compilation boundary as a defensive
check over `Rel`. It can also validate relations assembled by external callers.
It is not a second source of type information; the checked constructors are
responsible for semantic validity on the source compilation path.

## Shell and kernel

`Workspace` loads files, imports, overlays, and the embedded prelude. Salsa
memoizes parsing, resolution, and type-checking queries for editor updates.
`compile` is the shared boundary used by CLI and LSP to turn a checked
workspace into relations and diagnostics. It preserves source definition
identity even when names repeat. A definition also has a source-based key
separate from its source-order index. This key is an identity hint; it is not
a proof that a query can be reused.

Salsa memoizes exact per-definition facts: source, locations, closed types,
overload choices, and referenced bindings. The incremental shell compares
these facts and propagates changes through the dependency graph using an
iterative worklist. Only successful, unaffected queries are reused. Moved
definitions are rebuilt so `Rel::At` spans stay current, and failures are
rendered against the latest text. Module inference is still memoized at module
granularity; `TypeCheck` shares its immutable results across requests.
Imported type schemes are borrowed as needed. Workspace views share immutable
name maps, and elaboration borrows them without copying a catalog. See
[catalog performance](PERFORMANCE.md) for reproducible measurements at up to
one million definitions and the remaining costs of module-wide edits.

`CompilerInput` and `ModuleSnapshot` expose the source graph and checker facts
read-only to the compiler. Cache ownership and invalidation live in
`incremental.rs` and `Workspace`; compilation and erasure consume immutable
inputs. File access, diagnostic formatting, and SQL options stay in the shell.

The compilation result is:

```text
compile : Workspace -> Compilation

Compilation {
  queries: [CompiledQuery],
  diagnostics: [Diag]
}
```

## Paper claims

The current checked layer supports local, testable claims:

- A checked query's row is the output of its relational constructor.
- Every source column reference is checked against the row available there.
- Phase restrictions are enforced before lowering.
- Outer-join nullability and set-operation compatibility are represented in
  output rows.
- Erasure is structural and leaves no type-checking work to SQL lowering.
- The query route has one explicit transitional fallback. Typed lowering
  returns its graph and records whether it aligned with the checked tree well
  enough to reconstruct its nodes. A typed compiler error is never retried through the adapter; if
  the builder declines the whole shape, `query_lowering::lower` invokes the
  normalized adapter. The final architecture removes this branch after the
  remaining expression payloads and helper instantiation slices migrate.

This is the claim the architecture is built to make, so it is worth saying how
it is held rather than only that it is asserted. Three gates, all on the signal
the compiler raises itself — `Fault::Compiler`, which means a construct reached
the language without a rule:

* `the_normalized_route_handles_every_construct` runs a program per expression
  shape and requires no compiler gap on any of them;
* `every_query_is_built_by_the_term_route` currently guards that the legacy
  adapter handles every query shape that can still reach it. Typed slices have
  separate tests that require their builders to bypass normalization for the
  shapes they claim;
* `no_corpus_program_hits_a_compiler_gap` extends it to every shipped example
  and every corpus case.

These replaced two counters — a gap tally and a discarded-gap sieve — that had
become **vacuous**: when the source walk was deleted, nothing wrote to either,
so both read zero whether the route worked or not. That is not a hypothetical.
Breaking one lowering case made the counter-based gate *pass* while
`no_corpus_program_hits_a_compiler_gap` failed and named the program. A counter
is only evidence while something increments it; a fault is raised by the code
under test.

The honest limit that remains: these gates say the constructs they exercise
have a rule, not that every construct does. The corpus is the defence against
the second — adding a language feature without a lowering case fails it, because
the case for that feature would be written before the rule.

The old source elaboration walk is gone — `elaborate.rs` is 270 lines where it
was 2329. The remaining normalized query adapter is a named, temporary route
in `query_lowering.rs`; it is not a second attempt after a typed error, and no
grammar rule or runtime path may add another fallback beside it.

The compiler uses an explicit immutable input view and produces an owned
compilation result. The shell still owns Salsa state and the checker uses
mutable inference state internally; this boundary does not assert that every
implementation phase is purely functional.
