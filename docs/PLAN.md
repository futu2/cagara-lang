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
  inside the prelude. The one other thing Rust owns is the per-dialect
  spelling of the `CAGARA_*` date and string functions: sqlglot cannot
  translate them, and a dialect-specific template string would be re-parsed
  and corrupted on the way out (`docs/SQL-DIALECTS.md`).
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
- **Order is kept across derived tables.** SQL does not keep a derived
  table's order, so wrapping a stage moves its ORDER BY to the outer query
  (the inner keeps it only for LIMIT / OFFSET); a sort key that is not an
  output column is passed out as a hidden `__kN` column. Join inputs drop
  an ORDER BY that no LIMIT needs.
- **Semantics that differ per engine are pinned.** NULLs sort last in
  both directions (the clause is left out where it is the default and
  emulated with a leading `CASE WHEN x IS NULL` key where the dialect has
  no NULLS LAST); int `/` truncates toward zero (`CAGARA_IDIV`: `DIV`,
  `DIVIDE`, or exact arithmetic where `/` is decimal); `%` is `MOD` for
  BigQuery; `length` counts trailing spaces on T-SQL; month arithmetic
  clamps on SQLite (`floor`, SQLite 3.46+); a condition used as a T-SQL
  column becomes `CASE WHEN p THEN 1 WHEN NOT (p) THEN 0 END`. Trino /
  Presto / Athena and Spark / Databricks have their own intrinsic
  spellings. Joins keep the left column on a shared name (by design).
- **Identifiers are quoted when needed** (not a lowercase word, or
  reserved), in the dialect's quotes; string literals double backslashes
  for dialects that treat `\` as an escape.
- **SQL is built as ANSI and rewritten per dialect** over every block and
  expression slot (sqlglot's own pass only covers the outer SELECT's
  columns / WHERE / GROUP BY / HAVING). `||` becomes `CONCAT` for MySQL and
  T-SQL; a lone `offset` gets MySQL's max LIMIT, and T-SQL requires an
  `order` before `offset`.

## Status

Done and tested with focused HIR/SQL suites, `cargo check --workspace`, and
the VS Code extension compile:

- Lexer, parser, AST lowering (28 tests), including recovery, losslessness,
  and a nesting / chain-length limit (expressions, types, projection
  chains) that reports a syntax error instead of overflowing the stack on
  pathological input. Strings end at their line; an identifier in column 0
  is never taken into the item above (`x :\ny = 1`); recovery stops in
  front of closing delimiters; an int literal out of range is an error.
- Formatter property tests: generated programs with comments between any
  tokens format losslessly, idempotently, and keep every comment; random
  token soup never panics. A trailing comment ends its line wherever it is.
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
  when the imports changed, since loading files stays outside salsa. Salsa's
  text and the workspace's are written together and nowhere else, so a span
  from a query always indexes the text diagnostics are rendered against: a
  refused edit leaves both alone instead of stranding the db on the new
  text, and a span that is not a char boundary of the rendered text is
  snapped rather than panicking.
- Language server (`cagara lsp`, crate `cagara-lsp`): full-text sync, one shared
  workspace/module graph for all open documents, and unsaved-buffer overlays
  passed to importers (reloaded when imports change). Publishes all diagnostics for the file (syntax, type, schema)
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
  location. The VS Code client registers `.cagara` file watching, while the
  Neovim client opts into repo-local binaries explicitly.
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
  Strings concatenate with `<>` (right-assoc, just looser than `+` / `-`). Pipeline
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
  `expr r (maybe a)` are disjoint. Operators and aggregate/window inputs
  reject `maybe`; use `coalesce` explicitly before passing a nullable column.
  `sum` / `avg` / `min` / `max`, `lag` / `lead`, `sumOver` / `avgOver` return
  `maybe`; `count`, `countOf`, `countDistinct`, and `countOver` return non-null
  counts. `just`, `isNull`, `isNotNull`, and `isTrue` handle nulls. Outer joins
  make the missing side's columns `maybe` (never twice); join predicates see
  the plain types. Join kinds are separate primitives (`__leftJoin`, ...) so
  the checker sees them.
- Evaluator, prelude, imports with aliases, cycle and duplicate detection.
- IR validation: missing columns, join sides, key-mapper collisions, nested
  aggregates, ungrouped columns, filtering on aggregates/windows.
- No input aborts the compiler. Every failure is a diagnostic: nesting and
  chain length in the parser, application depth in the evaluator (so
  `f = x => f x` is reported instead of recursing for ever), a chain of
  forward-referenced definitions in the checker (reported instead of
  overflowing the stack), and the open
  overload count in the checker. A definition that nothing uses is still
  rejected when its overloads cannot be satisfied for any type
  (`bad = .age + "x"`), while leftover literals keep their polymorphic type.
- The checker and the evaluator accept the same programs. `asc` / `desc` take
  an expression rather than a sort key (`asc (desc .x)` is a type error), a
  key mapper's list or record must hold strings, and a `sql` template must
  have the signature that gives it its arity and phase — all of which the
  evaluator would otherwise reject after the checker had accepted them.
- Checker performance and incrementality: union-find path compression,
  versioned variable state, and cached overload fitting avoid retrying an
  overload unless its inputs changed. Salsa stores raw diagnostic spans and
  messages, rendering them only outside the query so dependent edits do not
  invalidate diagnostics unnecessarily.
- SQL lowering for where/select/agg/order/limit/offset/keyMap/joins/windows,
  frames, constant-only global aggregates, join-input inlining, dialect
  rewriting (postgres, mysql, sqlite, duckdb, tsql, bigquery, snowflake) over
  every block — derived tables, CTE bodies, set-operation branches, join
  inputs — and every expression slot (including ORDER BY and a join's ON),
  CASE, casts, `distinct`, membership, semi/anti joins, set operations, and
  automatic CTE reuse for repeated relational subtrees. `distinct` is a
  lowering barrier: a projection, aggregate, window, or key mapper that
  follows it sees the deduped rows instead of folding into the DISTINCT, and
  ORDER BY is always emitted outside it (an emulated NULLS LAST key may not
  appear in a DISTINCT select list). A branch of a set operation that has
  its own LIMIT / OFFSET is put behind a derived table, since the set
  operator's tail would otherwise swallow it (`SELECT ... LIMIT 3 UNION
  SELECT ... LIMIT 2` is rejected by every engine); a plain branch stays
  flat. `--optimize` remains
  opt-in, with differential tests that run the same queries on SQLite and
  DuckDB (`tests/engines.rs`; skipped when a shell is missing, required in
  CI).
- Date and string prelude: `currentDate`, `now`, `add{Days,Weeks,Months,
  Quarters,Years,Hours,Minutes,Seconds}`, `trunc{Year,Quarter,Month,Week,
  Day,Hour,Minute}`, `year` / `month` / `dayOfWeek` / ..., `daysBetween`,
  `toDate` / `toTimestamp` / `toString`; `substring`, `left`, `right`,
  `strpos`, `contains`, `startsWith`, `endsWith`, `replaceAll`, `ltrim`,
  `rtrim`, `ilike`. Templates call `CAGARA_*` intrinsics that
  `cagara-sql/src/intrinsics.rs` spells per dialect, since sqlglot's typed
  date functions do not transpile reliably and re-parse dialect-specific SQL
  fed back through a template (see `docs/SQL-DIALECTS.md`). A `CAGARA_*` call
  the backend does not recognize is a diagnostic, not SQL passed through.
  `addWeeks` / `addQuarters` are ordinary Cagara built on `addDays` /
  `addMonths`. Prelude functions take their subject last (`addDays 7 .d`,
  `coalesce 0 x`, `contains "@" .email`), so partial applications compose
  with `>>>`.
- Precise error locations: query stages built in user code are tagged with
  their source span (`Rel::At`, transparent to schema and lowering), so an
  IR-validator error points at the innermost failing stage. Diagnostics show
  the source line with the span underlined.
- CLI with `file:line:col` diagnostics and non-zero exit on errors.
- Examples: `examples/report.cagara`, `public.cagara` + `schema.cagara`,
  `errors.cagara`.

Known gaps:

- **Non-static key mappers** (`prefix`, `suffix`, or a column list that is
  not a literal) give an unconstrained row; the IR validator checks them. A
  literal list or record must hold strings, so `only [1]` and `rename { id =
  5 }` are now type errors.
- **No implicit conversions (by design).** `.age * 1.5` and `.amount +
  .user_id` are type errors; only literals take the type their context
  needs, like Haskell's numeric literals. Duplicate overload candidates with
  the same signature are only reported as ambiguous at a use.
- **Extra subqueries** remain where a join is the right input of another
  join (no parenthesized joins yet).
- `--optimize` runs sqlglot's optimizer (constant folding, boolean
  simplification, pushdown). Tests pin that it keeps filters outside
  window / LIMIT / aggregate boundaries; it stays opt-in.
- **`offset` without `limit` is not spelled for SQLite.** SQLite rejects a
  lone `OFFSET`, and the rewriter only adds the missing `LIMIT` for MySQL
  (and requires an `order` for T-SQL). A query that offsets without a limit
  therefore compiles to SQL that SQLite refuses.
- **Smaller gaps from the same review:** `open_with_buffers` can add one
  file twice (duplicate diagnostics); a nested definition check takes the
  completion probe from the definition being completed; template `$n`
  validation misses `$0`, gaps, and a `$n` inside a name or a type;
  `qualify` rewrites a bare keyword in a template into `t1.DAY`; the
  reserved-word list misses MySQL 8 words such as `rank`; `MAX_DEPTH`
  reports recursion for a deep but terminating program.
- **Names still hardcoded in Rust, not Cagara:** the six pipeline-operator
  names (`eval.rs`), the window-spec field names (checker and evaluator),
  and user-facing names inside error strings (`coalesce`, `isNull`, `agg`,
  `where`, `group`, `keyMap`, `only`, `replace`, `wholePartition`). These
  need a prelude marker or a shared constant before they can move.

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
10. ~~SQL semantics~~ (done: ORDER BY kept across derived tables; int
    division and `%` per dialect; NULLs last everywhere; T-SQL `LEN` and
    condition columns; `$n` errors in templates; Trino / Spark families;
    differential tests on SQLite and DuckDB, run in CI. Joins stay
    left-wins on a shared column name, now documented).
11. ~~Parser and formatter~~ (done: strings end at a newline; column-0
    names end the item above in types, records, imports, and lambdas;
    depth limits for types and projection chains; recovery keeps closing
    delimiters; int overflow is a diagnostic; trailing comments anywhere;
    `{ | r }`; `<>` looser than `+` / `-`; generated-program and
    token-soup property tests).
12. ~~Language server and CLI~~ (done; see Status): one shared workspace with
    open buffers fed to their importers, reloaded on save / watched-file events; the
    completion probe on a snapshot; `url`-based URI conversion; CLI
    `args_os`, broken pipe, and `--types --only` exit code; trust gating for
    repo-local binaries in the editor plugins.
13. ~~Checker performance~~ (done; see Status): union-find path compression
    and re-checking an overload only when its variables change (a 250-operator chain takes
    1.8 s); spans out of salsa results so edits to an import do not re-check
    every dependent.
14. ~~Language~~ (done; see Status): conditionals (`CASE`), `distinct`, set
    operations, `in` / semi/anti-joins, `cast`, automatic CTEs for repeated
    relational subtrees, and strict aggregate/window nullability requiring
    explicit `coalesce`.
15. ~~Review fixes, second pass~~ (done; see Status): the dialect rewriter now
    walks CTE bodies, set-operation branches, and their own ORDER BY / LIMIT /
    OFFSET; an unrecognized `CAGARA_*` intrinsic is a diagnostic instead of
    SQL passed through; nested `||` flattens to one `CONCAT`; `addWeeks` /
    `addQuarters` moved into `prelude.cagara`; the duplicated `paren` /
    `atomic` helpers unified; clippy clean (was 10 warnings).
16. ~~Checker guards from the second review~~ (done; see Status): a chain of
    forward-referenced definitions is reported instead of overflowing the
    stack; `asc` / `desc` reject a sort key; a key mapper's list or record
    must hold strings; a `sql` template needs a type signature.
17. ~~`distinct` lowering~~ (done; see Status): a projection, aggregate,
    window, or key mapper after `distinct` no longer folds into it, and
    ORDER BY is emitted outside the DISTINCT — which also makes the T-SQL
    NULLS LAST emulation valid. SQLite differential cases cover it.
18. ~~Set-operation branches and the salsa / `Workspace` text desync~~ (done;
    see Status): a branch with its own LIMIT / OFFSET is wrapped in a
    derived table (a SQLite differential case covers it), and the db text
    and the workspace text are updated together, so a refused edit cannot
    strand the db on text the workspace never took, and a span that is not a
    char boundary of the rendered text no longer panics.
19. Next: `offset` without `limit` for SQLite, and the smaller gaps from the
    same review (see Known gaps).
