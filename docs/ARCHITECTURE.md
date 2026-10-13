# A smaller architecture

Cagara's semantic pipeline is a compiler, not an interpreter:

```text
source -> syntax -> type checking -> normalization -> CheckedQuery
       -> erasure -> Rel -> SQL AST -> dialect SQL
```

The relational IR is untyped. Type checking establishes scalar types, rows,
phases, and overload choices. Normalization consumes those facts and reduces an
expression to a normal form — resolving names, inlining definitions, and
discharging applications through one reduction relation — before that normal
form is lowered through checked constructors into a relational tree whose
invariants are already known. Erasure drops the checked metadata at the SQL
boundary.

## Semantic layers

The implementation has five semantic representations:

```text
AST            names, source spans, and user syntax
TypeCheck      inferred schemes, use types, rows, and overload choices
Term           an expression's normal form, built from TypeCheck's facts
CheckedQuery   relational structure plus checked rows, phases, and origins
Rel            backend-facing relational structure without type metadata
```

`Term` is what makes the rest compositional. It is a small calculus — local
binders, columns, literals, templates, lambdas, applications, `case`, records,
lists, and relational primitives — with an application reduction that substitutes
arguments for parameters. A term carries no AST node and no name waiting to be
looked up, so it cannot silently depend on where it was written. Names resolve
and applications reduce once, in the normal form, instead of being rediscovered by
each consumer.

A relational primitive also carries the **row it produces**, taken from the type
the checker recorded at that application. It has to be the node's own type and not
the declaring definition's signature, because a definition can inline two tables —
`innerJoin orders (.<id == .>uid)` over a joined pair — and each needs its own
columns. That is the fact the query lowering rests on: `CheckedQuery::table`
refuses to build without columns, and the term is where they come from.

The shapes are uniform enough to state: every stage is
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

Ordinary functions are inlined by the reduction, not called. A definition that
names itself is reported as recursion rather than expanded without end: the
normalizer carries a stack of the definitions it is already inlining
(`Normalizer::active`), and that stack is now the *only* place recursion is
detected, because there is no longer a second walk to agree with.

Nothing here evaluates. A definition's normal form is built once and cached, and
the result of elaborating a definition is a `CheckedQuery`; SQL lowering never
sees an intermediate value representation.

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
- Normalization is total over the language: every expression form **and** every
  relational construction reduces to a term, and a form without a rule is
  reported as a compiler gap rather than silently lowered by a second path.
  There is no fallback route, for expressions or for queries.

  This is the claim the architecture is built to make, so it is worth saying how
  it is held rather than only that it is asserted. `compile::tests` gates it from
  two directions: `the_normalized_route_handles_every_construct` counts the gaps
  over every program shape the suite exercises and requires zero, and
  `every_query_is_built_by_the_term_route` asserts that a representative program
  per construct reaches the term route rather than an alternative. The latter is
  meaningful because a fallback would have to declare itself to the discarded-gap
  sieve, which is asserted empty.

  Until recently the second half was not true: a definition whose body was a
  query was built by re-walking its source, and that layer could report a
  capability gap *and* try its next reading of an expression. Both are gone —
  `elaborate.rs` is 270 lines where it was 2329 — and no grammar rule or runtime
  path in this crate re-reads source to decide what something means.

The compiler uses an explicit immutable input view and produces an owned
compilation result. The shell still owns Salsa state and the checker uses
mutable inference state internally; this boundary does not assert that every
implementation phase is purely functional.
