# Development plan

The production query path now uses the compile-time normalizer. The compiler
architecture and its invariants are documented in [`ARCHITECTURE.md`](ARCHITECTURE.md);
this page tracks the remaining boundaries and language work.

## Work plan

The order below is the order the work is done in. Each item ends the same way: a
probe that shows the defect, a fix, a test that fails without the fix, and a commit
that says what was measured rather than what was assumed.

### A. The tooling surfaces

The compiler is well covered (every construct has an engine-backed test). The tools
around it are not: `cagara-fmt` has property tests for the shapes it was written
for, `cagara doc` and `cagara check` have one test per mode, and the language server
has 56 tests against `analysis::*` but 8 against the *server*. A boundary here is
anything an editor or a script can reach and a test cannot.

| # | item | done when |
|---|---|---|
| ~~A1~~ | **Done.** | 4 server tests; a real bug was found by writing them | Every capability it advertises (`hover`, `definition`, `references`, `documentHighlight`, `documentSymbol`, `formatting`, `codeAction`, `inlayHint`, `completion`) is driven through the server for a real document, including a UTF-16 position and a document with an import. | each capability has a server-level test, and a request for a capability it does *not* advertise is refused rather than answered |
| ~~A2~~ | **Done.** `references` is cross-file now; a real bug | They walk the root module only (`occurrences`), so a use of `lib.helper` in `main.cagara` is not a reference to `helper`. | a definition used from another open file lists that use |
| ~~A3~~ | **Done.** Two properties, and the mutation showed the first version tested the wrong one | Round trip (format twice = format once), lossless (format then compile = compile), and comments survive — checked over the whole corpus and over the adversarial inputs that break formatters (a comment longer than the width, a string that cannot break, nesting deeper than the width allows). | three property tests over `examples/`, `tests/corpus/` and a generated adversarial set |
| ~~A4~~ | **Done.** One object per name with a `signatures` array; the shape changed | Overloads render as one heading in the markdown but arrive as N definitions in JSON, with the doc on the first only. A consumer cannot tell that `sum`'s six signatures are one name. | the JSON groups overloads the way the markdown does, or says in `docs/` why not |
| ~~A5~~ | **Done.** Found a symlink loop checking one file 41 times | Exit codes, dedupe across workspaces, a file that imports a broken file, a directory with no `.cagara` files, a path that is not a file. | one test per case, including the exit code |

### B. The deferred language work

These are features with a known shape (see the table at the end of this file). They
are done in this order because each one is smaller than the next:

| # | item | done when |
|---|---|---|
| B1 | name-aligned set operations | `union` aligns by name, with the same-row-in-the-same-order rule kept as an option |
| B2 | file-declared typed parameters | a file declares its own parameters, so a query runs without `--param` |
| B3 | richer window frames | `range`/`groups` as two more constructors, with the engine coverage the other frames have |
| B4 | catalog declarations | a module declares tables without a `query … = table` binding |

### C. Documentation that stays true

| # | item | done when |
|---|---|---|
| C1 | every claim in `docs/` is either tested or marked as an expectation | a claim with no test says so in the sentence |
| C2 | `PLAN.md`, `CHANGELOG.md` and `ARCHITECTURE.md` are updated in the same commit as the change | no commit leaves a doc describing the previous behaviour |

## Current boundaries

- The compile-time normalizer and relational arena are the only production query
  path. The former typed lowering adapter and its route differential have been
  removed after the corpus reached agreement.
- A join chained on the left (`a & innerJoin b … & innerJoin c …`, which is what
  a pipeline writes) lowers to one `FROM` chain, with no derived table. A join
  *nested on the right* (`a & innerJoin (b & innerJoin c …) …`) puts its right
  input behind a derived table. That is a limit of the AST, not of SQL: sqlglot's
  `TableSource` has `Table`, `Subquery`, `TableFunction`, `Lateral`, `Unnest` and
  nothing for a parenthesized join, so `a JOIN (b JOIN c ON …) ON …` cannot be
  built. Both engines here *do* accept that form (measured on DuckDB and
  PostgreSQL), so the derived table is a workaround for the AST rather than a
  portability requirement — and it is correct SQL either way. Lifting it means an
  AST node for a nested join, which is sqlglot's to add.
- A filter after an aggregate is `HAVING`. A filter over a window is `QUALIFY`
  where the target has the clause (DuckDB, Snowflake, BigQuery, Teradata) and a
  derived table elsewhere; a filter over a `limit` always wraps, because paging
  decides *which rows* the stage returns and the filter has to see the result. A
  filter over a bare `distinct` does not wrap: deduping whole rows and filtering on them commute, since two identical rows
  agree on every predicate, so it goes inside the `SELECT DISTINCT`. (A stage that
  has *both* an aggregate and a `distinct` still wraps — the aggregate is what
  blocks it, not the dedupe.)
- Aggregates are values, modified by three combinators (`filtered`,
  `distinctAgg`, `over`). `filtered p a` is written as a first-class
  `FILTER (WHERE …)` clause where the target has one (PostgreSQL, DuckDB, SQLite,
  Trino, Snowflake) and as `SUM(CASE WHEN p THEN x END)` where it does not (MySQL,
  T-SQL, BigQuery, Spark) — the IR carries the predicate and `dialect.rs` picks the
  spelling. `distinctAgg` is `SUM(DISTINCT x)` everywhere.
- A single lowercase letter in a type position is a type *parameter* (that is
  what makes `id : a -> a` work); anything else must be a constructor the
  language has, and a near miss is reported with a "did you mean". An explicit
  binder for type parameters would let a one-letter typo be reported too, and is
  a language change rather than a check.
- SQL optimization remains opt-in (`--optimize`) and runs after Cagara lowering, on
  the statement. The differential suite runs every shared case through both settings
  on both engines, so an optimization that changes an answer is a wrong row rather
  than a surprise in production.

## Lists

`list a` is a value type: `arrayAgg` builds one, `[1, 2]` is one, and `<>` concatenates two. The
spelling is the target's — `ARRAY[…]`/`ARRAY_AGG`/`||` on PostgreSQL, `list_value`/`list`/
`list_concat` on DuckDB — and a dialect with no array type refuses the query instead of writing
JSON text, which would change the type silently. A list can be built (`[1, 2]`, `arrayAgg`), combined (`<>`), measured and searched (`listLength`,
`listContains`), compared as a value (`==`, `!=`, `<` — the generic comparisons take `a` on both
sides and a `list a` is an `a`; measured on DuckDB and PostgreSQL, which compare arrays
element-wise) and unpacked (`unnest`). What is missing is the JSON-facing functions, which would
need their own intrinsic per dialect.

## Recently closed

- A sort key is re-aimed whenever the stage's columns change (`retarget_order`): the
  name still selects the same column, the name selects something else and the FROM has
  an alias, or the name is gone. Both failures it fixes were found by probing on a
  real engine, and one of them was introduced by the fix for the other.
- The `Rel -> Rel` pass (`rewrite.rs`) and the lowering rules around it: pagination
  now means what it says (`limit 5 & offset 2` is rows 3..=5), a sort key is written
  as the output column it names, and four derived tables that protected nothing are
  gone (identity projection, semi/anti join inputs, a null-extending join input whose
  computed column stays NULL, and a projection over paging).
- A statement nested inside an expression is rewritten for the target dialect.
  `EXISTS (…)` from a semi/anti join used to carry `NULLS LAST` to MySQL and T-SQL,
  which cannot read it. Every dialect is now checked for the spellings it cannot read
  (`each_dialect_avoids_what_it_cannot_read`) as well as for well-formedness.

- The `??` advice reaches overload failures too: `.n + .n` on a `maybe int` names
  the nullable argument and the two ways out, where it used to list candidates.
- A `decimal` cast finds its parameters through a partly applied cast
  (`lift (toDecimal 10 2) .d`) and through a named constant (`p = 10`), both of which
  used to leave `decimal a b` open in `--types`.
- The documented decimal range is `0 ≤ s ≤ p ≤ 38` — a scale of zero is a whole
  number of units and was always allowed; only the prose said otherwise.

- `unnest .x` of a non-list says `unnest needs a list, and this is not one`. The
  conflict still surfaces in the row unification, but the application records its
  argument's span (`unnest_args`) and the definition's error is rewritten when it
  falls inside one.
- A `maybe` used where a value is needed carries its `??`/`just` fix on the
  diagnostic itself, and `cagara check` prints it (`fix: Use `?? ""` — .s → (.s
  ?? "")`) and carries it in `--json` as `fixes`. The projection's refusal comes
  out of its constraint, so the checker keeps two side tables — where each field
  value is written (`field_spans`, keyed by the stage's span) and where each
  column access is written (`field_uses`) — and builds the suggestion from the
  message's innermost `field `name`` plus that column's span. The language
  server's message rule stays as the fallback for a diagnostic without one.

## Next steps

1. ~~Separate relational rewrites from SQL emission.~~ **Done: the pass exists and
   earns its keep.** `crates/cagara-sql/src/rewrite.rs` is a `Rel -> Rel` pass with
   no dialect and no SQL in it, applied in `compile` after validation and before
   lowering. It holds the rules the lowerer cannot express, because they look at two
   stages at once:

   | rule | why it cannot be a stage decision |
   |---|---|
   | `limit a & limit b` → `limit (min a b)` | a pair of nodes, not one node against a stage |
   | `offset a & offset b` → `offset (a+b)` | same |
   | `limit a & offset b` → `limit (a-b) & offset b` | the fold is legal in one order only, and which order a pipeline is in is a property of the pair |
   | `distinct & distinct` → `distinct` | same |
   | an identity projection → the input | it has to cross a barrier to be worth removing, which is a property of the pair |

   The pagination rule fixed a real bug: `limit 5 & offset 2` keeps rows 3..=5, and
   the pair folded into `LIMIT 5 OFFSET 2` keeps five rows starting at the third. The
   pass turns it into one statement with the right rows, and `Fuse::Offset` now
   refuses to fold into a stage that already has a limit, so the shape cannot come
   back through the lowerer.

   The layering is otherwise as it was, and stays by concern:

   | module | does | knows about dialects? |
   |---|---|---|
   | `rewrite.rs` | `Rel` → `Rel` | no |
   | `lower.rs` | `Rel` → `Stage`: fuse or wrap (`Fuse`, `needs_barrier`, `grouped`, the depth bound) | no |
   | `stage.rs` | `Stage` → a `sqlglot` `SelectStatement` | no |
   | `dialect.rs` | `rewrite` / `validate_dialect_support` on the finished statement | yes — all of it |

   Two of those rules fixed answers rather than plans: the pagination fold, and a
   sort key written as the column it names instead of that column's *expression*
   (`select { id = .a, a = .id } & order [asc .id]` used to order by `a`, which is
   an output alias for `id`).

   A semi/anti join now inlines both of its inputs: it is an existence test, neither
   side is null-extended, and a derived table there protects nothing. That one lives
   in `lower.rs` (`join_input` with `can_place`) rather than in the pass, because it
   is a decision about one input against a stage, not about a pair of nodes.

   A projection that only renames and reorders (a permutation of the stage's
   columns) now crosses the barrier a general one cannot: it cannot change which
   rows a `LIMIT` or a `DISTINCT` below it selects. That rule is in `stage.rs`
   (`keeps_every_column`) and consulted from `lower_select`, because the question is
   one node against a stage — the same boundary as the semi-join inlining.

   A *dropping* projection now folds over paging too (a `LIMIT` picks rows by an
   `ORDER BY` over the input columns, which SQL allows), but never over a
   `DISTINCT` — that is the one where dropping changes how many rows are deduped. A
   projection that reuses a sort key's name for another column is refused as well.

   A null-extended join side now inlines a *computed* column when the expression
   stays NULL without its row (`amount * 2`), and keeps the derived table when it
   does not (a literal, or a call — `coalesce` exists, so no call is assumed). That
   is `null_propagating` in `lower.rs`.

   What is *not* left to do, having checked: a filter on an inner join already folds
   into the outer `WHERE` (`lower_join` hands the sides' `wheres` over), and a
   projection that only renames a join operand already folds into it. The remaining
   candidates are `count(*)` over an aggregate
   (`agg { c = count } & agg { d = count }` is `COUNT(*)` over one row per group,
   which SQL can write as a nested aggregate only with a window or a derived
   table — so it stays). None of them is needed for correctness today.

2. ~~Extend the execution-backed coverage beyond SQLite, DuckDB and PostgreSQL.~~
   **Done to the extent this repository can reach, and the limit is documented.**
   Three engines can be started here (`sqlite3`, `duckdb`, and PostgreSQL through
   PGlite); MySQL, Trino, Spark, BigQuery, Snowflake and T-SQL need a server this
   repository does not start, and there is no container runtime or `nix` on the
   machine the suite runs on. So the other seven targets are covered by the two
   checks a text pipeline can actually make, over every shared case:

   **Trino is now a real engine too.** It was the biggest gap and it closed: Java 17
   is on this machine, Trino 435 is the newest release built for it, and
   `tests/trino.sh` drives a single-node server over the REST API with the same batch
   protocol as `pglite.mjs`. `trino_agrees_when_a_server_is_running` runs every shared
   case on it, and `docs/ENGINE-TESTS.md` says how to start the server (it is not in
   the dev shell: 680 MiB of jars). The first run found two bugs no text check could
   see — a `date` literal reaching `DATE_DIFF` as a string, which Trino rejects rather
   than coerces, and the `OFFSET`/`LIMIT` order, which its grammar reverses.

   The MySQL family closed the same way Trino did: MariaDB ships as a self-contained
   tarball, so `tests/mysql.sh` runs a real server and `mysql_agrees_when_a_server_is_running`
   puts every shared case through it.

   | check | what it proves | what it does not |
   |---|---|---|
   | `every_statement_is_well_formed_sql` | the generator produced well-formed SQL for that target (balanced, no placeholder left, no empty clause) | that the target *accepts* it — sqlglot's parser is dialect-agnostic here, which is measured in the test itself |
   | `each_dialect_avoids_what_it_cannot_read` | the dialect pass removed the spellings that target cannot read (`LIMIT` and `NULLS LAST` for T-SQL, `||` and `FULL JOIN` for MySQL, `::` everywhere but PostgreSQL, backticks outside MySQL) | anything about values — only an engine can answer that |
   | `each_dialect_uses_the_spelling_it_has` | the right spelling is *present* — `CONCAT(` where `||` is OR, `TOP` and `OFFSET … ROWS FETCH NEXT` for T-SQL paging, a `CASE` for a boolean column — which the absence check cannot see, since a construct that is missing entirely passes an absence | the same: it is text |

   The first of those found a real bug: a statement nested inside an expression
   (`EXISTS (…)` from a semi/anti join) was never rewritten, so `NULLS LAST` reached
   MySQL and T-SQL inside the subquery. That is exactly the class of defect the
   engines here could not see, and the reason the text checks were worth adding.

   **MySQL is a real engine too.** MariaDB 11.4 unpacks from a tarball and runs with
   no root, no container and no package manager, so `tests/mysql.sh` drives it with
   the same batch protocol and `mysql_agrees_when_a_server_is_running` runs every
   shared case on it (100 of them). No new dialect bugs — the MySQL rules this
   repository already had all held — but the three harness bugs found on the way are
   exactly the kind that look like dialect bugs, and one of them (the client escaping
   backslashes without `-r`) would have been blamed on the compiler.

   **The rest are emulated.** T-SQL, BigQuery, Snowflake and ANSI are translated to
   DuckDB by Python's sqlglot — an independent implementation — and executed there
   (`the_emulated_dialects_mean_the_same_thing`, 403 statements). It found two BigQuery
   bugs on its first run, both invisible to a text check: a bare `UNION` (that grammar
   wants `UNION { ALL | DISTINCT }`) and a doubled `''` quote (it parses as two adjacent
   literals). It is a second opinion, not a server: blind to anything both
   implementations get wrong, to any type, and it runs on DuckDB's engine. Those limits
   are written down in `docs/ENGINE-TESTS.md` and pinned in the test.

   So every dialect now executes somewhere. What would still move this item: a real SQL
   Server, because boolean handling and paging differ from everything else there and the
   emulation cannot judge a type.

   **The capability tables are checked too.** `supports_qualify`, `supports_filter`,
   `supports_full_join`, `null_order` and the paging order are hand-written, and a wrong
   entry is SQL that looks right — which is how the two BigQuery bugs got in.
   `every_construct_we_emit_is_one_the_engine_reads` compiles one query per conditional
   rule and *runs* whatever came out, asserting the implication rather than a recorded
   string. It was verified by mutation: adding SQLite to `supports_qualify` makes it fail,
   and the first version of it silently passed because it compiled the table definition
   instead of the query. No table turned out to be wrong — they were unchecked, not
   broken — and the same mutation test is what makes that statement worth anything.

## Direction

Where the work goes from here, in the order the evidence supports it. Each item
names the observation that puts it there, because a plan without one is a wish.

1. **Real engines beyond the three.** Three can be started here (SQLite, DuckDB,
   PGlite); seven targets are checked only as text. The gap is not hypothetical —
   `countOf` on a nullable column was wrong for a whole release and no text check
   could see it, and the semi-join `NULLS LAST` bug was found by the text checks
   only after it had reached MySQL and T-SQL. A MySQL server (the target whose
   expression semantics differ most: `||` is OR, integer division truncates
   differently) buys the most per unit of setup, then Trino for its strict typing.
   This is infrastructure, not language work, and it is the highest-value item.
2. ~~**`qualify`.**~~ **Done, as a lowering rule rather than a language feature.**
   A filter over a window folds into `QUALIFY` on the four targets that have the
   clause (DuckDB, Snowflake, BigQuery, Teradata) and into a derived table on the
   rest. No new syntax: `where` already means "filter the rows", and the pipeline
   order says whether a window came before it. Two exclusions, both measured: a
   `limit`/`offset` cannot fold (SQL runs `QUALIFY` before `LIMIT`), and the
   predicate names the window *function*, not the output alias, because a later
   projection can drop the alias.
3. **The decimal widening result type.** **Attempted and reverted, with the blocker
   now known.** The type-level arithmetic is straightforward — three `Ty` variants
   (`NatAdd`, `NatMin`, `NatMax`) and a `reduce_nat` that folds them once the operands
   are numbers; `decimal (atMost 38 (plus (larger p q) 1)) s` reduces to `decimal 11 2`
   at the use site, and that part worked. Two things blocked it:

   * **The engines do not agree on multiplication**, so only `+`/`-` can widen.
     Measured: `(10,2) * (12,2)` is `DECIMAL(18,4)` on DuckDB and `decimal(22,4)` on
     Trino — DuckDB caps the product at 18 digits for a literal fold. Addition *is*
     portable: `(max(p,q)+1, s)`, capped at 38, on both. So the widening would have
     been half a rule, with multiplication still same-type.
   * **A computed precision survives into the row type unreduced**, so a stage that
     reads the column cannot match it. `select { t = .a + .b }` was fine and printed
     `decimal 11 2`, but `& order [asc .t]` then failed with
     `no overload of `asc` matches … decimal (atMost 38 (plus (larger a b) 1)) 2` — the
     term with the *signature's* variables still in it, in a row the printer could
     reduce and the matcher could not. Making `reduce_nat_terms` walk `Ty::Row` and
     reducing in `lift` and in the recorded `uses` were each tried and neither closed
     it, so the remaining work is an ordering fix in the checker: a computed parameter
     must reduce before the row is recorded, which means knowing the operand bindings
     at that point.

   The same-type rule is what makes the type honest, and it is what ships. Doing this
   properly is worth it only if someone needs SQL's `(p+1, s)` — and then the ordering
   problem above is the work, not the arithmetic.

   One thing the attempt did establish, worth keeping: the widening is *necessary*, not
   cosmetic. `DECIMAL(10,2)` holds `99999999.99`, its sum with itself is
   `199999999.98`, and casting that back to `DECIMAL(10,2)` is an out-of-range error on
   DuckDB. Both engines answer `DECIMAL(11,2)`; a type that says `10 2` would be
   describing a value it cannot hold.
4. ~~**Grouping sets / `rollup` / `cube`.**~~ **Done.** `rollup e` and `cube e` are
   primitives beside `group`, and the key they mark is `maybe` in the output row — the
   type is the whole point, since there is no cast that makes a subtotal row's key
   exist. The shape belongs to the stage: `ROLLUP(a, b)` is one clause over every marked
   key, so a plain `group` beside them goes first in the clause, which is what makes it
   mean "grouped within the subtotals". SQLite and the MySQL family refuse it, for
   different reasons — SQLite has no grouping sets, and MySQL's `WITH ROLLUP` rolls up
   the whole key list and cannot combine with `ORDER BY`.
5. ~~**Correlated subqueries.**~~ **Investigated: no capability is missing.** Every
   correlated pattern this plan named is reachable today with the join spellings, and
   `the_join_spellings_match_a_correlated_subquery` proves it by running each one beside
   the correlated subquery a user would have written and requiring the same rows:

   | question | how it is written today |
   |---|---|
   | "does this row have a child" | `leftJoin (child & agg { k = group .k, n = count }) … & select { has = (.n ?? 0) > 0 }` |
   | "how many children" | the same `leftJoin` + `agg` |
   | "this row against its group's max" | `leftJoin (self & agg { k = group .k, m = max .x }) …` |
   | "top N per group" | `rowNumber { partition = … }` + `where` |
   | "a value from the whole table beside each row" | a cross join: `leftJoin (q & agg {…}) (1 == 1)` |

   So the item was mis-stated. What was actually missing is **ergonomics**, and it is
   narrower than "a scope in the IR": `semiJoin`/`antiJoin` are *stages*, so an `EXISTS`
   could not be a column or sit inside `||`. **Fixed:** `markExists q on` appends one
   `bool` column named `exists` and takes a join predicate, so

       q = users & markExists orders (.<id == .>user_id) & select { id = .id, has = .exists }

   is `EXISTS` as a value. One wart remains: a side-less `.x` in its predicate is
   refused with a unification message rather than the join's `.<x` / `.>x` hint, because
   the column's row type is concrete by then. A general correlated subquery is still not
   needed and would be a large change with no evidence behind it.

What is *not* on the list, and why: replacing sqlglot (it is a parser/transpiler and
the crate does not parse; the two weaknesses found — a dialect-agnostic parser and no
parenthesized-join node — are local and covered), and adding an execution path
(sqlx is an orthogonal layer; a compiler that runs queries needs a database to build
against, which is the opposite of what this is).

## Maintenance

What the next reader needs to know before changing something, in the order the
questions come up.

**Where to put a change.** [`ARCHITECTURE.md`](ARCHITECTURE.md) has the crate table
and the two rules that keep it true: a dialect rule goes in `cagara-sql/src/dialect.rs`,
and a rule that needs to look at two stages at once goes in `rewrite.rs`. Everything
else in the backend is one node against the stage built so far.

**The three big files are big for a reason, and it is the same reason.**
`check/infer.rs` (3765 lines) is one `impl Checker` with five marked sections —
definitions, inference, coercions, primitives, deferred constraints — and they share
the trail, the variable store and the pending-constraint list, so splitting them
would mean handing a dozen fields across a module boundary on every call. The
sections are the seams; a new one is a new marker, not a new file.
`check/tests.rs` and `cagara-sql/src/tests.rs` are test bodies, which have no reason
to be anywhere else.

**Adding a language construct** is four steps, and the architecture doc lists them:
a checker fact, a normalizer reduction, an arena constructor (or a payload rule), and
a regression that covers the erased `Rel` *and* its source origins. A construct that
skips one of those compiles and then misbehaves in a way no test is looking for.

**Adding a backend rule** is two steps: a `Rel -> Rel` rule in `rewrite.rs` (if it
needs a pair), or a `Fuse`/`needs_barrier` answer in `stage.rs` (if it is one node
against a stage) — and then a case in `engines.rs`, because a lowering bug is a wrong
row and only an engine can see it. The differential suite is the only thing standing
between a "harmless" reordering and silently different results.

**What the tests are for**, so a failure points somewhere:

| suite | catches |
|---|---|
| `cagara-hir` unit tests | a checker rule, a message, a suggestion |
| `cagara-sql/src/tests.rs` | the SQL a query lowers to, per dialect |
| `cagara-hir/tests/corpus` | the SQL of the shipped examples, as a snapshot (`CAGARA_BLESS=1` to update) |
| `cagara-sql/tests/engines.rs` | wrong *rows* on real SQLite, DuckDB and PostgreSQL, in both lowerings |
| `cagara-sql/tests/fuzz.rs` | a crash, a panic, or non-determinism over generated programs |
| `cagara-cli/tests/doc*.rs` | the prelude's documentation, and that the manual's code blocks compile |
| `cagara-sql/tests/perf.rs` | a lowering or checking regression in time |

**A recorded expectation is not a derivation.** The differential suite asserted a
wrong row order for a whole round because the expectation had been recorded from the
engine's output instead of reasoned from the query — and so it could not catch the bug
it was recorded from. When adding a case, work out the answer from the data and write
the reasoning in the comment beside it.

## Deferred language work

These are **features**, not open questions — each is a language or IR addition with a
known shape, and none is a boundary of what exists today:

| item | what it would take |
|---|---|
| catalog declarations | a module that declares tables without a `query … = table` binding, so a project can share one schema |
| typed bind parameters | `--param name:type=value` works today (`limit_n : int = 5` is prepended to the file, so the type is checked like any signature), and a wrong type is a compile error. What is missing is a *file* declaring its own parameters, so the same query can be run without the flag |
| name-aligned set operations | `union` requires both sides to have the same row *in the same order*; aligning by name needs a projection per branch |
| richer window frames | `rows …` covers row-counted frames; `range`/`groups` would be two more constructors |

Done since this table was written, and struck from it: `qualify` (a lowering rule —
`where` over a window folds into the clause on four targets), grouping sets (`rollup`
and `cube`), and correlated subqueries (no capability was missing; `markExists` closed
the ergonomic gap).

`decimal` was the one with a correctness argument behind it, and it is in — see
above for what ships and what was decided against. The decisions as they stand:

- **One type everywhere.** `+`, `-`, `*` take two operands of the same `(p,s)` and
  return it; a comparison needs the same type on both sides; a different `(p,s)` is
  a compile error whose message names both parameters and says `toDecimal` is how a
  value moves. There is no widening coercion and no mixed comparison.
- **The declared width is the promise.** `+`/`-`/`*` keep the operand type rather
  than widening the way SQL does: the engine's own arithmetic is exact anyway, and a
  *computed* result type cannot be stated as a signature without type-level
  arithmetic. A product's scale is the engine's, so `toDecimal p s` is how a value
  comes back to the declared one.
- **Division is refused**, because PostgreSQL keeps a decimal quotient exact and
  DuckDB promotes it to `DOUBLE`: no same-type rule holds on both targets.
- **Literals lift.** A numeric literal takes the `(p,s)` its context demands, with
  its digits checked at compile time, the way `1` is already an `int` or a `float`
  depending on context. Columns and expressions never lift.
- **Parameters may be zero-scaled.** `decimal 38 0` is a whole number of units;
  `1 ≤ p ≤ 38` and `0 ≤ s ≤ p`.
- `sum`/`avg` are signatures, not rules: `sum : expr r (decimal p s) -> agg
  (expr r (maybe (decimal 38 s)))`, because the width is a constant in the portable
  family (PostgreSQL's `numeric` is unbounded, which is why the language states the
  portable 38 rather than either engine's best).

The remaining deferred items are unchanged: catalog declarations, typed bind
parameters, name-aligned set operations, `qualify`, grouping sets, lateral or
correlated subqueries, and richer window frames.

See [`CORE.md`](CORE.md) for semantics and [`ROW-TYPES.md`](ROW-TYPES.md) for
row algebra.
