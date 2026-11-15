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
columns and add nullability, and set operations require equal columns and answer in the
left operand's order. Erasure
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

## Where code lives

Six crates, one direction of dependency: `cagara-syntax` ← `cagara-hir` ←
`cagara-sql`/`cagara-fmt` ← `cagara-cli`, with `cagara-lsp` above `cagara-hir` and
beside `cagara-sql` (it reads `cagara-sql` to report what the lowering refuses, which
is the one thing an editor cannot learn from the checker). Nothing lower knows about
anything above it.

| crate | what it owns | depends on |
|---|---|---|
| `cagara-syntax` | lexer, parser, AST, the operator table, the `##` doc comments | — |
| `cagara-hir` | resolution, the checker, the typed core, the normalizer, the relational arena, erasure to `Rel`, schema validation | syntax |
| `cagara-sql` | `Rel` → SQL: `rewrite` (`Rel` → `Rel`), `lower` (`Rel` → `Stage`), `stage` (`Stage` → statement), `dialect` (statement → target), `intrinsics` (the `CAGARA_*` templates) | hir |
| `cagara-fmt` | source formatting, on the lossless rowan tree | syntax |
| `cagara-lsp` | diagnostics, hover, completion, code actions, inlay hints, rename, folding ranges, semantic tokens — all read the same `Workspace` the CLI does, and diagnostics also ask `cagara-sql` so the editor shows what the compiler will refuse | hir, sql, syntax, fmt |
| `cagara-cli` | one module per subcommand (`main` for the file compiler, `fmt`, `check`, `doc`) | everything |

Two rules keep that table true, and both are cheap to check:

- **A dialect rule lives in `dialect.rs`.** `lower.rs` and `stage.rs` mention no
  `Dialect`, so a question about a target is asked once, on the finished statement,
  and the pass that builds it stays dialect-free.
- **A rule about two stages at once lives in `rewrite.rs`.** The lowerer decides one
  node against the stage built so far; anything that needs to look at a *pair*
  (`limit` then `offset`, an identity projection over a barrier) is a `Rel` → `Rel`
  rewrite, so it can be tested without SQL.
