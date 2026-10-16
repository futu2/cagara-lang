# Type system and pipeline redesign

## Why change it

The deferred `prefix`/`suffix` defect is a symptom of a representation mismatch.
Inference sees a whole pipeline as a nested row type, while each deferred stage
constraint separately tries to recover the stage's own input/output relationship
from that whole type. When inference has already composed stages, the constraint
can no longer tell which `keyMap` layer belongs to it. It either identifies the
wrong row and fails an occurs check, or accepts a row that does not represent the
stage's result.

The checker also leaves facts in several maps (`use_tys`, resolutions, overload
choices, schemes) for normalization and elaboration to recover in later passes.
That design has been made safer over time, but a type fact and the checked node
it justifies can still get out of step. The redesign should make the typed
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

During migration, the existing normalizer can consume the new typed result or
remain behind an adapter. The adapter must be temporary and must not infer,
resolve, choose overloads, or recompute rows. Once the typed tree covers all
constructs, remove the side-map dependency and old path rather than keeping two
semantic implementations.

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

### 4. Move expression facts into the typed tree

Return `TypedExpr` from checking and carry resolved definitions, overload
choices, phase, scalar type, and source locations there. Adapt normalization and
checked construction to consume those facts. Keep the public `TypeCheck` query
surface as a derived compatibility view during the transition.

### 5. Replace deferred stage equations

Move each stage's local obligations onto its typed stage node. Remove
`Cons::MapKey` and the analogous stage constraints only when their rules have
equivalent typed-node implementations and diagnostics. In particular, validate
that constraints generalize with helper schemes and are freshly instantiated at
each use.

### 6. Remove duplicate semantic paths

Route source compilation through typed checking directly to `CheckedQuery`.
Delete transitional maps and adapters after downstream consumers have migrated.
Keep erasure structural and SQL lowering type-agnostic.

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
