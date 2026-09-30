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
| `cagara-hir` | salsa db (`parse_module`, per-module `module_check` queries), workspace/module loading, type checker (`check.rs`), evaluator, `__` primitives, IR, schema/phase validation, placement rules shared by both (`rules.rs`) |
| `cagara-sql` | IR → sqlglot stages, `sql "..."` template expansion, dialect rewriting, end-to-end tests |
| `cagara-lsp` | language server library over stdio (`lsp-server`), run by `cagara lsp`: diagnostics, hover, definitions, references, symbols, completion |
| `cagara-cli` | the single `cagara` executable: `cagara <file> [--dialect NAME] [--only DEF] [--pretty] [--optimize] [--types]`, and `cagara lsp` |
| `editors/vscode` | VS Code extension: TextMate grammar, language configuration, `vscode-languageclient` starting `cagara lsp` |
| `editors/nvim` | Neovim plugin (0.10+): filetype, syntax, `require("cagara").setup()` starting `cagara lsp` |

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
- **One set of placement rules.** Which phase may appear in `where`,
  `select`, `agg`, keys, and join predicates, how phases combine, the join
  output columns, and key-mapper validity live in `rules.rs` /
  `KeyMapper::apply`, used by the checker (once a phase type is known) and
  the IR validator alike. A test checks that every definition the checker
  accepts, including the examples, also evaluates and validates.
- **A `sql` template's result follows its `agg` / `win` arguments.**
  `inc : agg (expr r int) -> expr r int` is an aggregate (the arguments
  may not mix `agg` and `win`). A `select` field whose phase is still open
  (a helper's parameter) is fixed to row-or-window, so an aggregate passed
  in is rejected at the use.
- **Tables get columns from their annotation:**
  `users : query { id = int, ... } = table "public" "users"`.
- **Lowering fuses stages** into one SELECT when safe and wraps a derived table
  after aggregation, windows, or LIMIT/OFFSET. Join inputs that only project
  or filter a table (or an earlier join) are inlined: filters of a preserved
  side move to WHERE, those of a left join's right side into ON; a filtered
  null-extended side of a right / full join keeps a derived table, and so
  does a null-extended side with computed columns (they must be NULL on
  unmatched rows). Constant ORDER BY / GROUP BY keys are dropped (SQL would
  read `1` as a position); an all-constant grouping keeps its "no rows in,
  no rows out" meaning with `HAVING COUNT(*) > 0`.
- **Identifiers are quoted when needed** (not a lowercase word, or
  reserved), in the dialect's quotes; string literals double backslashes
  for dialects that treat `\` as an escape.
- **SQL is built as ANSI and rewritten per dialect** over every block and
  expression slot (sqlglot's own pass only covers the outer SELECT's
  columns / WHERE / GROUP BY / HAVING). `||` becomes `CONCAT` for MySQL and
  T-SQL; a lone `offset` gets MySQL's max LIMIT, and T-SQL requires an
  `order` before `offset`.

## Status

Done and tested (`cargo test --workspace`: 119 tests, no clippy warnings):

- Lexer, parser, AST lowering (21 tests), including recovery, losslessness,
  and a nesting / chain-length limit that reports a syntax error instead of
  overflowing the stack on pathological input.
- Salsa parse query with re-parse on edit (3 tests).
- Per-module type checking as a salsa query (`module_check` over a
  `ModuleInput`): type schemes are self-contained (scheme-local variables
  with their own flags), each module is checked against its imports'
  schemes only, and editing a file re-checks it and its dependents, not the
  prelude (1 test counts query runs).
- Name resolution as salsa queries: `module_own` (exports, overload sets)
  and `module_scope` are derived from the parsed file and resolved imports,
  so an edit that adds or removes a definition updates scopes, checks, and
  evaluation. `Workspace::set_source` applies an edit; it returns `false`
  when the imports changed, since loading files stays outside salsa.
- Language server (`cagara lsp`, crate `cagara-lsp`, 12 tests): full-text sync, one workspace
  per open document updated with `set_source` (reloaded when imports
  change). Publishes all diagnostics for the file (syntax, type, schema)
  with UTF-16 ranges; hover shows the inferred type (every candidate of an
  overload set); go-to-definition jumps to the name, including
  `alias.name` into imports. Names are resolved on the AST, so lambda
  parameters shadow top-level names and operators resolve at their symbol.
  Find-references and document highlights cover the open file; document
  symbols list imports and definitions with their types; completion offers
  names in scope, enclosing lambda parameters, and `alias.` members
  (operators and `__` primitives left out). After `.`, `.<`, or `.>` it
  offers the columns of that row, in declaration order: the file is
  re-checked with a reserved probe column (`PROBE_FIELD`) at the cursor,
  which adds nothing to its row but records what the row is known to
  have, then the text is restored. This works on half-typed lines; an
  open predicate outside a query only knows the columns used beside it. Prelude definitions have no
  location. Imported files are read from disk, and edits to them are not
  watched.
- Type checker (24 tests): HM with let-polymorphism for top-level definitions,
  monomorphic lambdas, Rémy-style rows, rigid (checked) signatures, constant
  lifting into `expr` with deferred int→float / string→date / timestamp
  widening,
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
  overload set (prelude: `+ - * / negate sum avg` on int and float).
  Strings concatenate with `<>` (`infixr 6`, as in Haskell). Pipeline
  shorthands at the level of `&`: `&?` where, `&=` select, `&*` agg, `&.`
  order, `&-` limit. Join operators sit between `&` and `$`: `?` inner, `<?`
  left, `?>` right, `<?>` full (`users & teachers ? .<a == .>b`). `x ?? d`
  is `coalesce d x` (right-assoc, tightest). Uses are resolved by trial unification against each
  candidate. A helper whose overloads stay open (`twice = x => x + x`) keeps
  them as holes in its scheme; every use fills them, and the evaluator
  follows the recorded choices, so one helper can be used at int and float
  in the same query. Query definitions default leftover
  literals before reporting ambiguity. Works through imports and aliases.
- Nullability, strict and explicit: `maybe a` is a real type, and type
  variables in signatures stand for non-null types, so `expr r a` and
  `expr r (maybe a)` are disjoint overloads. Operators reject `maybe`
  arguments; `coalesce`, `just`, `isNull`, `isNotNull`, `isTrue` handle
  nulls. Outer joins make the missing side's columns `maybe` (never twice);
  join predicates see the plain types. `sum` / `avg` / `min` / `max`,
  `lag` / `lead`, `sumOver` / `avgOver` return `maybe`; counts do not. Join
  kinds are separate primitives (`__leftJoin`, ...) so the checker sees them.
- Evaluator, prelude, imports with aliases, cycle and duplicate detection.
- IR validation: missing columns, join sides, key-mapper collisions, nested
  aggregates, ungrouped columns, filtering on aggregates/windows.
- No input aborts the compiler. Every failure is a diagnostic: nesting and
  chain length in the parser, application depth in the evaluator (so
  `f = x => f x` is reported instead of recursing for ever), and the open
  overload count in the checker. A definition that nothing uses is still
  rejected when its overloads cannot be satisfied for any type
  (`bad = .age + "x"`), while leftover literals keep their polymorphic type.
- SQL lowering for where/select/agg/order/limit/offset/keyMap/joins/windows,
  frames, constant-only global aggregates, join-input inlining, dialect
  rewriting (postgres, mysql, sqlite, duckdb, tsql, bigquery, snowflake),
  and `--optimize` (19 end-to-end tests).
- Date and string prelude: `currentDate`, `now`, `add{Days,Weeks,Months,
  Quarters,Years,Hours,Minutes,Seconds}`, `trunc{Year,Quarter,Month,Week,
  Day,Hour,Minute}`, `year` / `month` / `dayOfWeek` / ..., `daysBetween`,
  `toDate` / `toTimestamp` / `toString`; `substring`, `left`, `right`,
  `strpos`, `contains`, `startsWith`, `endsWith`, `replaceAll`, `ltrim`,
  `rtrim`, `ilike`. Templates call `CAGARA_*` intrinsics that
  `cagara-sql/src/intrinsics.rs` spells per dialect, since sqlglot's typed
  date functions do not transpile reliably. Prelude functions take their
  subject last (`addDays 7 .d`, `coalesce 0 x`, `contains "@" .email`), so
  partial applications compose with `>>>`.
- Precise error locations: query stages built in user code are tagged with
  their source span (`Rel::At`, transparent to schema and lowering), so an
  IR-validator error points at the innermost failing stage. Diagnostics show
  the source line with the span underlined.
- CLI with `file:line:col` diagnostics and non-zero exit on errors.
- Examples: `examples/report.cagara`, `public.cagara` + `schema.cagara`,
  `errors.cagara`.

Known gaps:

- **Non-static key mappers** (`prefix`, `suffix`, or a column list that is
  not a literal) give an unconstrained row; the IR validator checks them.
- **No implicit conversions (by design).** `.age * 1.5` and `.amount +
  .user_id` are type errors; only literals take the type their context
  needs, like Haskell's numeric literals. Duplicate overload candidates with
  the same signature are only reported as ambiguous at a use.
- **Extra subqueries** remain where a join is the right input of another
  join (no parenthesized joins yet).
- `--optimize` runs sqlglot's optimizer (constant folding, boolean
  simplification, pushdown). Tests pin that it keeps filters outside
  window / LIMIT / aggregate boundaries; it stays opt-in.

## Roadmap

1. ~~Type checker~~ and static key-mapper types (done; see Status).
2. ~~Overloading~~ (done; see Status).
3. ~~Nullability~~ (done; see Status).
4. ~~Precise error spans~~ (done; see Status).
5. ~~Salsa beyond parsing~~ (done for parsing, name resolution, and type
   checking; loading imported files still needs a reload).
6. ~~Tidy-ups~~ (done: `cagara-core` removed, join inputs inlined,
   `--optimize`, dialect rewriting and tests).
7. ~~Language server~~ (done; see Status).
8. ~~Review fixes, first pass~~ (done: checker panic on overload
   candidates, users of failed definitions, null-extended join inputs,
   identifier quoting and literal escaping, constant sort / group keys,
   `--` from negative numbers, intrinsics inside window specs, language
   server survives malformed messages and panics).
9. ~~Checker and IR validator agree~~ (done: label lists and rename
   records unify by content; open `select` phases stay out of `agg`;
   template results follow `agg` / `win` arguments; key mappers must leave
   a column; outer joins wait for column types before adding `maybe`;
   shared `rules.rs`; consistency test).
10. **SQL semantics.** Keep ORDER BY when a stage is wrapped; integer `/`
    per dialect (`CAGARA_IDIV`); explicit NULLS FIRST / LAST; T-SQL `LEN`
    and boolean select items; error on an unresolved `$n` in a template;
    Trino / Spark intrinsic families; silent column collisions in
    self-joins. Add differential tests that run the SQL on DuckDB / SQLite.
11. **Parser and formatter.** Strings stop at a newline; `at(Ident)` checks
    respect the column-0 boundary (`x :\ny = 1`); depth limits for types and
    projection chains; recovery keeps closing delimiters; int overflow is a
    diagnostic; trailing comments inside `spaced` / `TyApp` / lambdas;
    `{  | r }`; precedence of `<>` against `+`; property tests for
    losslessness and format round-trips with comments.
12. **Language server and CLI.** One shared workspace with open buffers fed
    to their importers, reloaded on save / watched-file events; the
    completion probe on a snapshot; `url`-based URI conversion; CLI
    `args_os`, broken pipe, and `--types --only` exit code; trust gating for
    repo-local binaries in the editor plugins.
13. **Checker performance.** Union-find path compression and re-checking an
    overload only when its variables change (a 250-operator chain takes
    1.8 s); spans out of salsa results so edits to an import do not re-check
    every dependent.
14. **Language.** Conditionals (`CASE`), `distinct`, set operations, `in` /
    semi-joins, `cast`, CTEs for reused definitions; a null-polymorphic
    type variable to halve the `maybe` overloads.
