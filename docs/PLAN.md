# Cagara: plan and status

Cagara is a small typed functional language that compiles to SQL. The Rust
core stays minimal; the user-facing library is written in Cagara itself
(`prelude.cagara`, imported into every module automatically).

## Goals

- Pipelines over queries: `users & where (.age >= 18) & select { id = .id }`.
- Columns as expressions: `.x` for a single input, `.<x` / `.>x` for join sides.
- One level of aggregation and windowing: `agg` and `win` results cannot be
  nested or mixed with ungrouped columns; later stages see them as plain columns.
- No "keys" concept: `pick`, `omit`, `rename` are all built on `keyMap`.
- Modules via `import "path.cagara" [as alias]`; prelude is implicit.
- Incremental parsing with salsa, lossless syntax trees with rowan.
- SQL through sqlglot-rust: ANSI by default, any sqlglot dialect via `--dialect`.

## Pipeline

```
source ──logos──▶ tokens ──rowan──▶ lossless CST ──▶ owned AST      (cagara-syntax)
AST ──salsa parse_module──▶ workspace (prelude + imports, scopes)  (cagara-hir)
      ──type checker (HM + extensible rows)──▶ well-typed defs        (cagara-hir)
      ──compile-time evaluator──▶ relational IR ──schema checks──▶   (cagara-hir)
IR ──stage lowering / fusion──▶ sqlglot AST ──▶ SQL text            (cagara-sql)
```

| Crate | Contents |
|---|---|
| `cagara-syntax` | lexer, parser (Pratt operators, column-0 layout rule, error recovery), AST lowering; operators desugar to calls (`a + b` → `_+_ a b`) |
| `cagara-hir` | salsa db and `parse_module` query, workspace/module loading, type checker (`check.rs`), evaluator, `__` primitives, IR, schema/phase validation |
| `cagara-sql` | IR → sqlglot stages, `sql "..."` template expansion, end-to-end tests |
| `cagara-cli` | `cagara <file> [--dialect NAME] [--only DEF] [--pretty] [--types]` |
| `cagara-core` | unused stub (to delete or repurpose) |

### Design decisions

- **Overloads are resolved at compile time.** The checker records, per
  definition and use site, which candidate (or which of its own holes) each
  overloaded use means; evaluation is keyed by definition plus hole
  assignment, and closures capture it.
- **Rust provides only primitives.** About 20 `__` functions (table, where,
  select, agg, order, limit/offset, join, group, asc/desc, key mappers, frame
  bounds) plus the `sql "..."` template mechanism. `__` names are visible only
  inside the prelude.
- **Templates carry their phase in the signature.** A `sql` definition must be
  annotated; its arity comes from the arrows, and the result head (`expr`,
  `agg`, `win`) decides whether it builds a scalar, aggregate, or window node.
- **Phases wrap expressions.** Surface types are `expr r a`, `agg (expr r a)`,
  and `win (expr r a)`. Internally `expr` has a phase slot (`row`/`agg`/`win`
  or a variable). A plain `expr` in a signature is phase-polymorphic (so `_+_`
  works on aggregates too), except when the result is `agg`/`win`, where its
  `expr` arguments must be row-phase: the depth-1 rule, in the types.
- **Column references can be `row` or `win`, never `agg`.** That is how
  ungrouped columns in `agg` are rejected by the checker.
- **Query stages are deferred constraints.** `where`, `select`/`agg`, and
  joins create constraints solved once their inputs are known; unsolved ones
  travel with a definition's type scheme and are re-instantiated at each use.
- **Phases are structural in the IR.** Aggregate and window nodes accept only
  row-phase arguments, which enforces the depth-1 rule.
- **Tables get columns from their annotation:**
  `users : query { id = int, ... } = table "public" "users"`.
- **Lowering fuses stages** into one SELECT when safe and wraps a derived table
  after aggregation, windows, or LIMIT/OFFSET.

## Status

Done and tested (`cargo test --workspace`: 46 tests, no clippy warnings):

- Lexer, parser, AST lowering (15 tests), including recovery and losslessness.
- Salsa parse query with re-parse on edit (3 tests).
- Type checker (20 tests): HM with let-polymorphism for top-level definitions,
  monomorphic lambdas, Rémy-style rows, rigid (checked) signatures, constant
  lifting into `expr` with deferred int→float / string→date widening,
  expressions as sort keys, records as window specs. Rejects scalar type
  errors (`.name + 1`), missing/removed columns, phase errors, join-side
  errors, and bad window spec fields before evaluation, with the error at the
  offending argument. `--types` prints inferred types.
- Typed key mappers: literal lists and records of strings get label types, so
  `pick` / `omit` / `rename` compute their output row statically (missing
  columns, duplicates, and collisions are type errors, and later stages and
  joins are checked). Stage constraints wait for the query's row, so printed
  types keep declaration order.
- Overloading: a name defined more than once, each with a signature, is an
  overload set (prelude: `+ - * / negate sum avg` on int and float; `+` on
  strings is `||`). Uses are resolved by trial unification against each
  candidate. A helper whose overloads stay open (`twice = x => x + x`) keeps
  them as holes in its scheme; every use fills them, and the evaluator
  follows the recorded choices, so one helper can compile to `age + age` and
  `name || name` in the same query. Query definitions default leftover
  literals before reporting ambiguity. Works through imports and aliases.
- Evaluator, prelude, imports with aliases, cycle and duplicate detection.
- IR validation: missing columns, join sides, key-mapper collisions, nested
  aggregates, ungrouped columns, filtering on aggregates/windows.
- SQL lowering for where/select/agg/order/limit/offset/keyMap/joins/windows,
  frames, constant-only global aggregates (9 end-to-end tests).
- CLI with `file:line:col` diagnostics and non-zero exit on errors.
- Examples: `examples/report.cagara`, `public.cagara` + `schema.cagara`,
  `errors.cagara`.

Known gaps:

- **Non-static key mappers** (`prefix`, `suffix`, or a column list that is
  not a literal) give an unconstrained row; the IR validator checks them.
- **Overload limits:** there is no implicit int → float conversion for
  columns (`.age * 1.5` is a type error; literals still widen), and duplicate
  candidates with the same signature are only reported as ambiguous at use.
- **Nullability is not tracked**, including outer-join sides.
- **Coarse error locations** for errors found only by the IR validator
  (start of the definition). Checker errors point at the argument.
- **Extra subqueries** in some cases, e.g. `rename` before a join.
- `--optimize` (sqlglot optimizer) is not wired in; its predicate pushdown
  can move filters across window boundaries, so it must stay opt-in.

## Roadmap

1. ~~Type checker~~ and static key-mapper types (done; see Status).
2. ~~Overloading~~ (done; see Status).
3. **Nullability** (`maybe a`) through outer joins and aggregates such as `sum`.
4. **Precise error spans** by carrying source spans into IR nodes.
5. **Salsa beyond parsing:** memoize name resolution and type checking per module.
6. **Tidy-ups:** remove `cagara-core`, reduce avoidable subqueries, opt-in
   `--optimize`, more dialect tests.
7. **Later:** language server on top of the salsa db.
