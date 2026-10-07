# A smaller architecture

Cagara's semantic pipeline is a compiler, not an interpreter:

```text
source -> syntax -> type checking -> source elaboration -> CheckedQuery
       -> erasure -> Rel -> SQL AST -> dialect SQL
```

The relational IR is untyped. Type checking establishes scalar types, rows,
phases, and overload choices. Source elaboration consumes those facts and uses
checked constructors to build a relational tree whose invariants are already
known. Erasure drops the checked metadata at the SQL boundary.

## Semantic layers

The implementation has four semantic representations:

```text
AST            names, source spans, and user syntax
TypeCheck      inferred schemes, use types, rows, and overload choices
CheckedQuery   relational structure plus checked rows, phases, and origins
Rel            backend-facing relational structure without type metadata
```

`CheckedQuery` and `CheckedExpr` are immutable owned trees. Their constructors
enforce local rules: predicates must be boolean and row-phase, expressions must
refer to available columns, projections compute their output rows, joins compute
nullable sides, and set operations require compatible rows. The node enums are
private so callers cannot bypass those constructors.

Ordinary functions are substituted during source elaboration. The elaborator's
small `CheckedValue` enum represents expressions, queries, stages, and
partially-applied callables only while a source expression is being expanded.
It produces no runtime values. Once a definition is elaborated, its result is a
`CheckedQuery`; SQL lowering never sees this temporary representation.

The type checker currently records facts in `TypeCheck` and uses mutable
inference state internally. A paper formulation can describe the same rules
with immutable substitutions and a result value:

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

The compiler uses an explicit immutable input view and produces an owned
compilation result. The shell still owns Salsa state and the checker uses
mutable inference state internally; this boundary does not assert that every
implementation phase is purely functional.
