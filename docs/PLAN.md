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
      ──compile-time evaluator──▶ relational IR ──schema checks──▶   (cagara-hir)
IR ──stage lowering / fusion──▶ sqlglot AST ──▶ SQL text            (cagara-sql)
```

| Crate | Contents |
|---|---|
| `cagara-syntax` | lexer, parser (Pratt operators, column-0 layout rule, error recovery), AST lowering; operators desugar to calls (`a + b` → `_+_ a b`) |
| `cagara-hir` | salsa db and `parse_module` query, workspace/module loading, evaluator, `__` primitives, IR, schema/phase validation |
| `cagara-sql` | IR → sqlglot stages, `sql "..."` template expansion, end-to-end tests |
| `cagara-cli` | `cagara <file> [--dialect NAME] [--only DEF] [--pretty]` |
| `cagara-core` | unused stub (to delete or repurpose) |

### Design decisions

- **Rust provides only primitives.** About 20 `__` functions (table, where,
  select, agg, order, limit/offset, join, group, asc/desc, key mappers, frame
  bounds) plus the `sql "..."` template mechanism. `__` names are visible only
  inside the prelude.
- **Templates carry their phase in the signature.** A `sql` definition must be
  annotated; its arity comes from the arrows, and the result head (`expr`,
  `agg`, `win`) decides whether it builds a scalar, aggregate, or window node.
- **Phases are structural in the IR.** Aggregate and window nodes accept only
  row-phase arguments, which enforces the depth-1 rule.
- **Tables get columns from their annotation:**
  `users : query { id = int, ... } = table "public" "users"`.
- **Lowering fuses stages** into one SELECT when safe and wraps a derived table
  after aggregation, windows, or LIMIT/OFFSET.

## Status

Done and tested (`cargo test --workspace`: 28 tests, no warnings):

- Lexer, parser, AST lowering (15 tests), including recovery and losslessness.
- Salsa parse query with re-parse on edit (3 tests).
- Evaluator, prelude, imports with aliases, cycle and duplicate detection.
- IR validation: missing columns, join sides, key-mapper collisions, nested
  aggregates, ungrouped columns, filtering on aggregates/windows.
- SQL lowering for where/select/agg/order/limit/offset/keyMap/joins/windows,
  frames, constant-only global aggregates (9 end-to-end tests).
- CLI with `file:line:col` diagnostics and non-zero exit on errors.
- Examples: `examples/report.cagara`, `public.cagara` + `schema.cagara`,
  `errors.cagara`.

Known gaps:

- **No type checker.** Signatures are read only on `sql` templates; elsewhere
  they are documentation. `.name + 1` is not rejected.
- **No overloading.** A name defined twice in a module is an error.
- **Nullability is not tracked**, including outer-join sides.
- **Coarse error locations** for schema errors (start of the definition).
- **Extra subqueries** in some cases, e.g. `rename` before a join.
- `--optimize` (sqlglot optimizer) is not wired in; its predicate pushdown
  can move filters across window boundaries, so it must stay opt-in.

## Roadmap

1. **Type checker** (HM with extensible rows) over the AST, before evaluation:
   - `query r`, `expr r a`, `agg r a`, `win r a`, records with row tails,
     `join l r` inputs for `.<x` / `.>x`.
   - Literals lift into `expr` at call sites.
   - Deferred projection constraints so `select` / `agg` check fields statically.
   - Signatures become checked, not documentation.
2. **Overloading** of operators by argument type, resolved by the checker.
3. **Nullability** (`maybe a`) through outer joins and aggregates such as `sum`.
4. **Precise error spans** by carrying source spans into IR nodes.
5. **Salsa beyond parsing:** memoize name resolution and type checking per module.
6. **Tidy-ups:** remove `cagara-core`, reduce avoidable subqueries, opt-in
   `--optimize`, more dialect tests.
7. **Later:** language server on top of the salsa db.
