# Checked relational IR

The compiler has one production path from a query definition to SQL:

```text
source -> type check -> source elaboration -> CheckedQuery -> erase -> Rel -> SQL
```

Source elaboration reads types, rows, and overload choices from `TypeCheck`, constructs checked
nodes, and substitutes user functions while walking their source bodies. The
SQL backend only receives `Rel` after checked erasure.

## Checked nodes

`CheckedQuery` stores its output `RowType`, node, and source `Origin`.
`CheckedExpr` stores its scalar type, phase, node, and origin. Their node enums
are private; callers build them through constructors that enforce row,
column, phase, join, and set-operation rules.

Erasure is structural. It removes the type and phase information after those
facts have served their purpose and produces the existing `Rel` used by SQL
lowering. The production boundary keeps schema validation as a defensive check
over that erased value. An unresolved call cannot be erased and produces an
internal compiler diagnostic.

## Elaboration

`elaborate_module` returns one result per definition in source order. A
definition's source index is its identity; names are only labels and may repeat
for overloads. Non-query definitions are omitted from query output, while
query definitions are built directly from their AST and the facts recorded by
type checking.

The elaborator handles direct SQL templates, relational primitives, ordinary
functions, partial applications, and pipeline stages. Its temporary
`CheckedValue` representation exists only inside this source transformation;
it is temporary source-expansion state and never survives into `CheckedQuery` or `Rel`.
Primitive declarations live in `primitive.rs` because name resolution and type
inference need their static names and arities. The old primitive interpreter
has been removed.

## Remaining boundary

The query compilation entry point is `compile`, which returns one owned
`Compilation` value containing definition identities, relations, and
diagnostics. CLI and LSP use it to obtain their results. The workspace keeps a
previous successful compilation after an edit, and `compile` reuses individual
erased queries only when a source-based definition key and its checked
dependency fingerprint match. The workspace and `TypeCheck` still expose
separate APIs, and the type checker uses mutable inference state internally.
Those are implementation choices outside the checked relational IR; replacing
them with a fully immutable whole-program compilation result remains future
work.

The checked constructors and erasure are covered directly by unit tests. CLI
tests and SQL integration tests exercise source elaboration through generated
SQL. There is one source-to-IR implementation to maintain.
