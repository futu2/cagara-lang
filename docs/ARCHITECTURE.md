# Compiler architecture

Cagara is a compile-time functional language whose result is a typed SQL
relation. A source program is reduced to a relation; it is never run against
data by the compiler.

```text
source
  -> parse and resolve
  -> bidirectional type checking
  -> typed core (check/typed.rs)
  -> normalize (beta-reduction, aliases, helpers, overloads)
  -> relational arena (normalize/arena.rs)
  -> structural erasure to Rel
  -> schema validation
  -> SQL lowering and dialect rewriting
  -> SQL
```

`CORE.md` is the semantic reference. It defines the reduction rules, the
relational constructors, and the SQL-visible choices such as bags, phases,
nullability, ordering, and templates.

## The compiler kernel

The checker records one typed node for every expression. Each node carries its
type, its query row when applicable, name resolution, overload choices, source
origin, and stage metadata. A stage record includes the input and output rows
from that stage's own signature, so later passes do not reconstruct a stage
from a composed result type.

The normalizer consumes this typed core. Names follow the checker-resolved
definition, lambdas become closures, and application performs beta-reduction
through an environment. Primitive definitions become values that accumulate
arguments. Once saturated, a query primitive calls one relational constructor;
once saturated, a SQL template builds one checked scalar expression. The
normalizer produces aliases, closures, labels, scalar expressions, and
relational nodes, never table data.

The relational arena is the only place that constructs a query node. Every
constructor computes its output row and checks its own rule: predicates and
keys must use the input row, projections compute their fields, joins qualify
columns and add nullability, and set operations require equal rows. Erasure
walks the finished arena and emits `Rel` without inferring or checking again.

`Control::Unsupported` represents a compiler capability gap. `Control::Program`
represents a rule that rejected an invalid program. The compiler keeps that
classification when it turns the result into diagnostics.

## Production boundary

`compile` is the shared boundary used by the CLI, LSP, and incremental shell.
After checking a root definition, `compile_definition` calls
`normalize::route::lower`, validates the erased `Rel`, and stores the relation
and its fault class in `Compilation`:

```text
Compilation {
  queries: [CompiledQuery],
  diagnostics: [Diag]
}
```

Each normalization attempt owns its arena, active-definition path, and work
budget. The workspace owns files, imports, overlays, diagnostics, and Salsa
cache state. This separation makes a compilation deterministic and lets
incremental or concurrent requests share read-only checker results without
sharing lowering state.

## Invariants

- Type checking precedes normalization for every accepted definition.
- A relational node's row is computed by its constructor.
- Column, phase, nullability, join-side, and set-operation rules are enforced
  before erasure.
- Erasure is structural; SQL lowering receives only `Rel`.
- Origins survive normalization and erasure, so diagnostics and SQL snapshots
  retain user source locations.
- A compiler capability gap is reported as a compiler fault and cannot be
  mistaken for a program error.
- The pipeline golden snapshot, corpus SQL snapshots, incremental tests, and
  dialect tests guard the complete path.

Adding a language construct therefore requires a checker fact, a normalizer
reduction, an arena constructor or payload rule, and a regression covering the
erased relation and its source origins.

## Shell and backend

The pure kernel consumes an immutable `CompilerInput` and produces owned
relations and diagnostics. The shell handles file access, overlays, editor
state, and SQL options. The SQL backend lowers `Rel` to ANSI SQL and rewrites
the result for each supported dialect. It does not perform language
type-checking; those facts are complete before SQL lowering begins.
