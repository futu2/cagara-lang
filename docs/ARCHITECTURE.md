# Compiler architecture

Cagara's semantic pipeline is a compiler, not an interpreter:

```text
source -> syntax/AST -> checker-owned typed facts
       -> TypedQuery lowering -> CheckedQuery -> erasure -> Rel
       -> SQL AST -> dialect SQL
```

`TypedQuery` and checker-owned typed expressions are the sole production query
path. The normalized term machinery remains a public, independent test oracle
for agreement and normalization properties; it is not a fallback compiler
route. An accepted construct without a typed lowering rule produces a compiler
capability error at the query boundary.

## Semantic layers

```text
AST            names, source spans, and user syntax
TypeCheck      inferred schemes, typed expressions, rows, and overload choices
TypedQuery     query operations, typed inputs, and checker-owned row boundaries
CheckedQuery   relational structure plus checked rows, phases, and origins
Rel            backend-facing relational structure without type metadata
```

The checker records one typed node for every expression. A node carries its
whole type, row when it is query-shaped, resolved definition or primitive,
overload choices, source span, and stage metadata. `TypedBody::by_stage` keeps
stage applications in application order, while `TypedQuery` stores the
relational skeleton in its own arena. A `NodeRef` identifies the definition and
application node where a stage was written, so helper instantiation preserves
ownership and source identity.

A typed query node carries the row it reads and the row it produces, both closed
by the checker at the application site. Its constructors enforce local rules:
rewrites must consume the row recorded for that stage, predicates must be
boolean and row-phase, expressions must reference available columns,
projections compute their output rows, joins compute nullable sides, and set
operations require compatible rows. No constructor recovers a row from a final
query type.

Typed lowering is recursive and transactional. It follows query aliases,
instantiates helpers with caller rows and checker-selected overloads, copies
composed stage graphs, and builds join/set children in operand order. Its
per-attempt context owns alias expansion, bindings, and cycle detection, so
incremental or concurrent compilations do not share lowering state. If any
payload in an attempted chain is unsupported, the attempt is discarded and the
query boundary reports a typed capability error.

`CheckedQuery` and `CheckedExpr` are immutable trees. Their constructors are the
single source of relational validity rules. Erasure traverses the checked tree
structurally and emits `Rel`; it does not infer types or recover rows. SQL
lowering sees only `Rel` and therefore cannot silently become a second type
checker.

The normalized term module still contains a small calculus for tests: local
binders, columns, literals, templates, lambdas, applications, cases, records,
lists, and relational primitives. Its `Facts` adapters read checker facts to
build the oracle. Production code may share its metadata readers
(`Resolved`, template phase, and declared SQL template handling), but query
construction never calls its source walk.

## Checker and incremental shell

Type checking is bidirectional: `infer` synthesizes a type and `check` pushes an
expected type down. That lets a lambda parameter receive its type from a known
function type instead of guessing from its body. The checker owns rows, phases,
SQL template selection, overload choices, and stage identities; downstream
passes consume those facts rather than re-deriving them.

`Workspace` loads files, imports, overlays, and the embedded prelude. Salsa
memoizes parsing, resolution, and type checking for editor updates. `compile`
is the shared boundary used by CLI and LSP to turn an immutable compiler input
into checked relations and diagnostics. Cache ownership and invalidation live
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

- A checked query's row is the output of its relational constructor.
- Every source column reference is checked against the row available there.
- Phase restrictions are enforced before lowering.
- Outer-join nullability and set-operation compatibility are represented in
  output rows.
- Erasure is structural and leaves no type-checking work to SQL lowering.
- Every accepted query is produced by typed lowering; there is no production
  normalized fallback.
- The normalized route agreement tests compare rows, stage order and identity,
  origins, checked relation structure, and generated SQL across dialects.
- Corpus, shipped-example, incremental, and SQL tests reject compiler gaps and
  preserve source diagnostics across edits.

These checks distinguish a language construct with a lowering rule from a
compiler capability gap. Adding a construct requires a checker record, typed
query builder, checked constructor coverage, and a regression before it is
considered accepted.

## Relational erasure and SQL

The checked tree carries rows, phases, types, and origins needed while building
a valid query. `erase` emits `Rel` structurally and preserves source origins.
The schema validator remains a defensive check over `Rel` and can validate
relations assembled by external callers; it is not a second source of type
information. SQL lowering and dialect rewriting operate after erasure.

## Design boundaries

The pure compiler kernel consumes `CompilerInput` and produces owned results.
The shell owns file access, diagnostics, Salsa state, overlays, and SQL options.
No runtime evaluation is performed during compilation. Typed relational
construction is the only production source of validity rules, while the
normalized term implementation is isolated as a removable, test-only semantic
oracle.
