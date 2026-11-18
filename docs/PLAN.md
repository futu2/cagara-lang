# Development plan

The production query path is the compile-time normalizer and the relational
arena. The compiler architecture and its invariants are documented in
[`ARCHITECTURE.md`](ARCHITECTURE.md); [`CORE.md`](CORE.md) covers the typed core
and the normalizer. This page is the current roadmap: [what is true
now](#current-boundaries), [what is open](#what-is-open), and what the tests are
for. Finished work is recorded at the end under [Historical
notes](#historical-notes); it is history, not a plan.

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
  built. DuckDB and PostgreSQL *do* accept that form (measured), so the derived
  table is a workaround for the AST rather than a portability requirement — and it
  is correct SQL either way. Lifting it means an AST node for a nested join, which
  is sqlglot's to add.
- A filter after an aggregate is `HAVING` — it replaced the earlier rule, which
  wrapped the aggregate's filter in a derived table and put the predicate in an
  outer `WHERE`. A filter over a window is `QUALIFY`
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
  what makes `id : ?a -> ?a` work); anything else must be a constructor the
  language has, and a near miss is reported with a "did you mean". An explicit
  binder for type parameters would let a one-letter typo be reported too, and is
  a language change rather than a check.
- There are no bind parameters. `--param name=value` substitutes a value at
  compile time — the file's definition is replaced and the value appears in the SQL
  as a literal — so what a host receives is a finished statement, not one with
  placeholders. The `$1`…`$n` in a `sql "…"` template are the template's own
  arguments, substituted during lowering, not host parameters. Adding `?`/`:name`
  would be a new output mode, and it would change what `--param` means.
- SQL optimization remains opt-in (`--optimize`) and runs after Cagara lowering, on
  the statement. The SQLite/DuckDB differential (`engines_agree`) runs every shared
  case through both settings, so an optimization that changes an answer is a wrong
  row rather than a surprise in production.
- Execution evidence is layered, and the `VERIFIED` table in
  `crates/cagara-sql/tests/engines.rs` is the executable statement of which is
  which: SQLite, DuckDB, PostgreSQL (PGlite), Trino and MariaDB run the shared
  cases on a real engine; T-SQL, BigQuery, Snowflake and ANSI are translated to
  DuckDB by Python's sqlglot; the rest are text-only spellings. The emulation is a
  second opinion, not a server — it cannot reject a spelling the target rejects and
  cannot judge a type — and [`ENGINE-TESTS.md`](ENGINE-TESTS.md) says how to run
  each one.

## Lists

`list a` is a value type: `arrayAgg` builds one, `[1, 2]` is one, and `<>` concatenates two. The
spelling is the target's — `ARRAY[…]`/`ARRAY_AGG`/`||` on PostgreSQL, `list_value`/`list`/
`list_concat` on DuckDB — and a dialect with no array type refuses the query instead of writing
JSON text, which would change the type silently. A list can be built (`[1, 2]`, `arrayAgg`), combined (`<>`), measured and searched (`listLength`,
`listContains`), compared as a value (`==`, `!=`, `<` — the generic comparisons take `a` on both
sides and a `list a` is an `a`; measured on DuckDB and PostgreSQL, which compare arrays
element-wise) and unpacked (`unnest`). What is missing is the JSON-facing functions,
which would need their own intrinsic per dialect, and the lambda-taking ones
(`listMap`, `listFilter`), which a compile-time lambda cannot supply — see
[What is open](#what-is-open).

## Decimal arithmetic

`decimal p s` is one type everywhere. `+`, `-` and `*` take two operands of the
same `(p,s)` and return it; a comparison needs the same type on both sides, and a
different `(p,s)` is a compile error whose message names both parameters and says
`toDecimal` is how a value moves. There is no widening coercion and no mixed
comparison. The declared width is the promise: `+`/`-`/`*` keep the operand type
rather than widening the way SQL does, because a *computed* result type cannot be
stated as a signature without type-level arithmetic. A product's scale is the
engine's, so `toDecimal p s` is how a value comes back to the declared one.
Division is refused, because PostgreSQL keeps a decimal quotient exact and DuckDB
promotes it to `DOUBLE`: no same-type rule holds on both targets. Numeric literals
lift to the `(p,s)` their context demands; columns and expressions never lift. A
scale of zero is a whole number of units and is allowed: `1 ≤ p ≤ 38` and
`0 ≤ s ≤ p`. `sum`/`avg` are signatures, not rules: `sum : expr r (decimal p s) ->
agg (expr r (maybe (decimal 38 s)))`, because the width is a constant in the
portable family.

The same-type rule is a real limitation, not a formality: a sum can outgrow its
operand type, and `a_decimal_sum_outgrows_its_operand_type` pins the engine
behaviour that makes it one. Widening was attempted and reverted; the blocker is
recorded in [Historical notes](#historical-notes).

## What is open

One item is work still to be done.

**A real SQL Server, and accounts for BigQuery and Snowflake.** Five engines run
the shared cases (SQLite, DuckDB, PostgreSQL through PGlite, Trino, MariaDB);
T-SQL, BigQuery, Snowflake and ANSI are executed only through a translation to
DuckDB, which is a second opinion rather than a server. A SQL Server is the
highest-value next engine, because boolean handling and paging differ from
everything else there and the emulation cannot judge a type; there is no
self-contained archive, and BigQuery and Snowflake need a cloud account. This is
infrastructure, not language work, and it is the highest-value item. The gap is
not hypothetical: `countOf` on a nullable column was wrong for a whole release and
no text check could see it, and the semi-join `NULLS LAST` bug was found by the
text checks only after it had reached MySQL and T-SQL.

Everything else that is open is a *boundary*, not a task, and is listed with what
it would take:

- a nested right-hand join without a derived table — an AST node for a nested join,
  which is sqlglot's to add ([Current boundaries](#current-boundaries));
- JSON-facing list functions, and `listMap`/`listFilter` — an intrinsic per dialect,
  or a new IR node for a lambda-over-list ([Lists](#lists));
- an explicit binder for type parameters, so a one-letter typo is reportable
  ([Current boundaries](#current-boundaries)).

Decimal widening was attempted and reverted; [Decimal
arithmetic](#decimal-arithmetic) says what ships and the blocker is recorded under
[Historical notes](#historical-notes).

What is *not* planned, and why: replacing sqlglot (it is a parser/transpiler and
the crate does not parse; the two weaknesses found — a dialect-agnostic parser and
no parenthesized-join node — are local and covered), and adding an execution path
(sqlx is an orthogonal layer; a compiler that runs queries needs a database to
build against, which is the opposite of what this is).

## Maintenance

What the next reader needs to know before changing something, in the order the
questions come up.

**Where to put a change.** [`ARCHITECTURE.md`](ARCHITECTURE.md) has the crate table
and the two rules that keep it true: a dialect rule goes in `cagara-sql/src/dialect.rs`,
and a rule that needs to look at two stages at once goes in `rewrite.rs`. Everything
else in the backend is one node against the stage built so far.

**The three big files are big for a reason, and it is the same reason.**
`check/infer.rs` is one `impl Checker` with five marked sections — definitions and
schemes, inference, coercions at expectations, primitives, deferred constraints —
and they share the trail, the variable store and the pending-constraint list, so
splitting them would mean handing a dozen fields across a module boundary on every
call. The sections are the seams; a new one is a new marker, not a new file.
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
| `cagara-hir/tests/pipeline_golden.rs` | the erased `Rel`, fault class and diagnostics of every program in the repository, as a snapshot (`UPDATE_SNAPSHOTS=1` to update) |
| `cagara-sql/tests/corpus.rs` (cases in `cagara-hir/tests/corpus`) | the accepted/type/SQL snapshot of the shipped examples (`CAGARA_BLESS=1` to update) |
| `cagara-sql/tests/engines.rs` | wrong *rows* on real SQLite, DuckDB, PostgreSQL (PGlite), Trino and MariaDB; the SQLite/DuckDB differential runs both optimizer settings |
| `cagara-sql/tests/fuzz.rs` | a crash, a panic, or non-determinism over generated programs |
| `cagara-cli/tests/doc*.rs` | the prelude's documentation, and that the manual's code blocks compile |
| `cagara-sql/tests/perf.rs` | a lowering or checking regression in time |

**A recorded expectation is not a derivation.** The differential suite asserted a
wrong row order for a whole round because the expectation had been recorded from the
engine's output instead of reasoned from the query — and so it could not catch the bug
it was recorded from. When adding a case, work out the answer from the data and write
the reasoning in the comment beside it.

## Historical notes

**Everything in this section is finished work.** It is kept because the
measurements are still the evidence behind the current rules; it is not a plan.

The last revision of the deferred-work table ended with "the remaining deferred
items are unchanged: catalog declarations, typed bind parameters, name-aligned set
operations, `qualify`, grouping sets, lateral or correlated subqueries, and richer
window frames." All but two of those are done, and the sections below record how;
correlated subqueries were investigated and closed. The two that are genuinely
absent are not on the plan: **bind parameters** (the emitted statement carries
literals — see the boundary in [Current boundaries](#current-boundaries)) and a
general **`LATERAL` subquery** (`unnest`
lowers to a lateral join internally in `lower.rs`, but the language offers no way
to write one, and `CAGARA.md` §9.1 lists `LATERAL` among the constructs that do not
exist).

### A. The tooling surfaces

The compiler had the engine-backed differential suite; the tools around it had far
less. A boundary here was anything an editor or a script could reach and a test
could not.

| # | item | outcome |
|---|---|---|
| ~~A1~~ | Language server, end to end | **Done.** Four server-level tests driving every advertised capability; found that none of them had ever been reached through the protocol. |
| ~~A2~~ | `references` and `highlights` across an import | **Done.** A real bug: `references` was `highlights` filtered, so it only ever saw the file the request came from — a short list rather than an error. |
| ~~A3~~ | `fmt` as an invariant, not examples | **Done.** Two properties — every shipped program formats, and every one means the same afterwards — plus the awkward shapes. The mutation showed the first version tested the wrong half. |
| ~~A4~~ | `doc`'s `--json` contract | **Done.** One object per name with a `signatures` array, as the markdown has always grouped them. The shape changed. |
| ~~A5~~ | `check` over a tree | **Done.** A real bug: a symlink loop made one six-byte file be checked 41 times (1.6 s against 0.04 s). |

### B. The deferred language work

These were the features with a known shape. Each is now done or settled; the
outcomes record what the work measured.

| # | item | outcome |
|---|---|---|
| ~~B1~~ | **Done, after a revert and a language decision.** Aligning by name makes the checker's row order and the SQL's column order disagree: `&` is reverse application, so the primitive's first parameter is the operand written on the *right*, while `step_set` binds the output to `left`. The SQL keeps the left's order; the type said the right's. The first attempt was reverted because settling that is a language decision rather than a projection in the arena. It found a real bug on the way: `cagara check` accepted the mismatched-order file that `cagara --only` refused — fixed separately. The decision was then made (a set operation answers with its left operand's row) and alignment by name landed; see "The set-operation decision" below. |
| ~~B2~~ | **Done.** A file declares a parameter with a default and `--param` overrides the value, keeping the file's type |
| ~~B3~~ | **Done, with two dialect refusals that measurement forced.** `range` and `groups` are constructors and `Frame` carries the unit; MySQL refuses `groups` (syntax error) and every dialect that emulates `NULLS LAST` refuses `range` (it needs one sort key; the emulation adds a second). **Extended:** the `groups` refusal is now asked of a capability table rather than of the MySQL family, because T-SQL documents `{ ROWS \| RANGE }` too and the family test accepted it — see [Capabilities](#capabilities) |
| ~~B4~~ | **Done.** `catalog "schema" { name = { … } }` lowers to one ordinary table definition per entry, so nothing downstream needed a case for it |

### C. Documentation that stays true

| # | item | outcome |
|---|---|---|
| ~~C1~~ | **Done.** Every ```` ```cagara ```` block in `docs/` compiles or is a listed fragment; the guide's shorthand table is checked against the fixed table *and* against the prelude's bodies; the two claims a reader would act on (paging order, empty `[]`) were measured and corrected; the examples are tested and the fragments are named |
| ~~C2~~ | **Done.** Kept per-commit through Phases A and B: the deferred-work table and `CHANGELOG.md` were kept current at every commit — no commit left a doc describing the previous behaviour |

### Completed backend work

**The `Rel -> Rel` pass.** `crates/cagara-sql/src/rewrite.rs` is a `Rel -> Rel` pass
with no dialect and no SQL in it, applied in `compile` after validation and before
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

What was checked and deliberately left alone: a filter on an inner join already
folded into the outer `WHERE` (`lower_join` hands the sides' `wheres` over), and a
projection that only renames a join operand already folded into it. The one
candidate that stayed is `count(*)` over an aggregate (`agg { c = count } & agg { d
= count }` is `COUNT(*)` over one row per group, which SQL can write as a nested
aggregate only with a window or a derived table — so it keeps its derived table).
None of them was needed for correctness.

**Execution coverage beyond SQLite, DuckDB and PostgreSQL.** Trino and MariaDB
were added as real servers: `tests/trino.sh` drives a single-node Trino 435 over
the REST API, and `tests/mysql.sh` runs MariaDB 11.4 from a tarball. The first
Trino run found two bugs no text check could see — a `date` literal reaching
`DATE_DIFF` as a string, which Trino rejects rather than coerces, and the
`OFFSET`/`LIMIT` order, which its grammar reverses. The MySQL family closed the
same way, with three harness bugs found on the way; one of them (the client
escaping backslashes without `-r`) would have been blamed on the compiler.

The dialects with no server here are covered by text checks and by an emulation.
The text checks are:

| check | what it proves | what it does not |
|---|---|---|
| `every_statement_is_well_formed_sql` | the generator produced well-formed SQL for that target (balanced, no placeholder left, no empty clause) | that the target *accepts* it — sqlglot's parser is dialect-agnostic here, which is measured in the test itself |
| `each_dialect_avoids_what_it_cannot_read` | the dialect pass removed the spellings that target cannot read (`LIMIT` and `NULLS LAST` for T-SQL, `||` and `FULL JOIN` for MySQL, `::` everywhere but PostgreSQL, backticks outside MySQL) | anything about values — only an engine can answer that |
| `each_dialect_uses_the_spelling_it_has` | the right spelling is *present* — `CONCAT(` where `||` is OR, `TOP` and `OFFSET … ROWS FETCH NEXT` for T-SQL paging, a `CASE` for a boolean column — which the absence check cannot see, since a construct that is missing entirely passes an absence | the same: it is text |

The first of those found a real bug: a statement nested inside an expression
(`EXISTS (…)` from a semi/anti join) was never rewritten, so `NULLS LAST` reached
MySQL and T-SQL inside the subquery. That is exactly the class of defect the
engines here could not see, and the reason the text checks were worth adding.

**The emulated dialects.** T-SQL, BigQuery, Snowflake and ANSI are translated to
DuckDB by Python's sqlglot — an independent implementation — and executed there
(`the_emulated_dialects_mean_the_same_thing`, 403 statements). It found two BigQuery
bugs on its first run, both invisible to a text check: a bare `UNION` (that grammar
wants `UNION { ALL | DISTINCT }`) and a doubled `''` quote (it parses as two adjacent
literals). It is a second opinion, not a server: blind to anything both
implementations get wrong, to any type, and it runs on DuckDB's engine. Those limits
are written down in [`ENGINE-TESTS.md`](ENGINE-TESTS.md) and pinned in the test.

**The capability tables are checked too.** The `Cap`/`Support` table, `null_order`
and the paging order are hand-written, and a wrong entry is SQL that looks right —
which is how the two BigQuery bugs got in.
`every_construct_we_emit_is_one_the_engine_reads` compiles one query per conditional
rule and *runs* whatever came out, asserting the implication rather than a recorded
string. It was verified by mutation: adding SQLite to the `QUALIFY` entry makes it
fail, and the first version of it silently passed because it compiled the table
definition instead of the query. The same mutation test is what makes that statement
worth anything.

## Capabilities

A family is a *grammar* grouping, and capabilities do not follow grammar, so the
questions "does this target have X" are answered by one table in
[`crates/cagara-sql/src/dialect.rs`](../crates/cagara-sql/src/dialect.rs) rather than
by `fam(d) != Fam::Mysql`-shaped tests. The table has four answers —
`Native`, `Emulated`, `Unverified`, `Unsupported` — and matches on `Dialect`
exhaustively, so a dialect added to `sqlglot-rust` is a compile error here instead of a
new target that inherits whichever family claimed it.

Two rounds of bugs are why it exists:

- **`GROUPS` on T-SQL.** `supports_groups_frame` was `fam(d) != Fam::Mysql`. T-SQL is
  in a different family and has no `GROUPS` frame either, so `--dialect tsql` emitted
  `GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW` for a server whose frame syntax is
  `{ ROWS | RANGE }`.
- **A check that stopped at a `Select`.** Each capability check wrote its own walk and
  treated `Rel::Select`/`Rel::Agg` as leaves, so an unsupported construct behind a
  later stage was never seen: `agg { k = rollup .k, n = count } & agg { total = count }`
  compiled for SQLite and then failed at the server with `no such function: ROLLUP`.
  The walk now lives in one place (`cagara-sql/src/visit.rs`) and is exhaustive over
  `Rel`, so a node with an expression in it cannot be skipped without a compile error.

Two constructs that used to be refused are now **emulated**, because an equivalent
exists and refusing made the language's surface depend on the target:
`fullJoin` on the MySQL family (a left join unioned with an anti-join of the right
rows that have no match) and `rollup`/`cube` on SQLite and MySQL (one `agg` per
grouping set, `UNION ALL`ed). Both are in `cagara-sql/src/emulate.rs`.

### Completed language decisions

**`qualify`.** Done as a lowering rule rather than a language feature. A filter over
a window folds into `QUALIFY` on the four targets that have the clause (DuckDB,
Snowflake, BigQuery, Teradata) and into a derived table on the rest. No new syntax:
`where` already means "filter the rows", and the pipeline order says whether a window
came before it. Two exclusions, both measured: a `limit`/`offset` cannot fold (SQL runs
`QUALIFY` before `LIMIT`), and the predicate names the window *function*, not the
output alias, because a later projection can drop the alias.

**Grouping sets.** `rollup e` and `cube e` are primitives beside `group`, and the key
they mark is `maybe` in the output row — the type is the whole point, since there is
no cast that makes a subtotal row's key exist. The shape belongs to the stage:
`ROLLUP(a, b)` is one clause over every marked key, so a plain `group` beside them
goes first in the clause, which is what makes it mean "grouped within the subtotals".
SQLite and the MySQL family refuse it, for different reasons — SQLite has no grouping
sets, and MySQL's `WITH ROLLUP` rolls up the whole key list and cannot combine with
`ORDER BY`.

**Correlated subqueries.** Investigated: no capability was missing. Every correlated
pattern the old plan named was reachable with the join spellings, and
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

is `EXISTS` as a value. The one wart at the time — a side-less `.x` in its predicate
being refused with a unification message rather than the join's `.<x` / `.>x` hint —
is closed too: `mark_exists_appends_one_boolean_column` pins that it now gives the
join's message. A general correlated subquery was not needed and would have been a
large change with no evidence behind it.

**Catalog declarations, file parameters, window frames.** `catalog "schema" { … }`
lowers to one ordinary table definition per entry. A file can declare a parameter
with a default (`rows_n : int = 5`) and `--param rows_n=2` overrides the value while
keeping the type. Window frames have all three units (`rows`, `range`, `groups`) and
the five bounds (`unboundedPreceding`, `unboundedFollowing`, `currentRow`,
`preceding n`, `following n`), so "richer window frames" is no longer open.

### The set-operation decision

The old deferred-work table said name-aligned set operations were the one item left.
That is settled: a set operation answers with its **left** operand's row, where
"left" is the operand written first in the source (`a & union b`). That decision is
what the rest needed.

What it fixed: the checker bound the answer's row to `left`, and `left` was the
primitive's first *parameter* — but `&` is reverse application, so that is the operand
written second. `a & union b` therefore had `b`'s column order in its type and `a`'s in
its SQL. The fix is to name the parameters by what they are (`argument`, `piped`) and
have the result *be* the piped variable rather than a third one unified with it:
unifying two unbound row variables binds in whichever direction `unify` takes, and that
is not a decision a type rule should leave to its implementation.

With the order settled, alignment by name landed too: `{ id, name }` and `{ name, id }`
are the same row written differently, so the two sides are matched by name and the right
input gets a projection into the left's order. Requiring the same order made the user
write a `select` on one branch purely to reorder, which says nothing about the query.

The attempt had already found one real bug, fixed separately: `cagara check` accepted a
file that `cagara --only` refused to compile. Making alignment work found a second — the
refusal came from `cagara-sql`'s lowering, which `check` did not run at the time; it
does now.

### The decimal widening attempt

**Attempted and reverted, with the blocker now known.** The type-level arithmetic is
straightforward — three `Ty` variants (`NatAdd`, `NatMin`, `NatMax`) and a `reduce_nat`
that folds them once the operands are numbers; `decimal (atMost 38 (plus (larger p q) 1))
s` reduces to `decimal 11 2` at the use site, and that part worked. Two things blocked
it:

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
  it. What would unblock a future attempt is an ordering fix in the checker: a
  computed parameter must reduce before the row is recorded, which means knowing the
  operand bindings at that point.

The same-type rule is what makes the type honest, and it is what ships. Doing this
properly is worth it only if someone needs SQL's `(p+1, s)` — and then the ordering
problem above is the work, not the arithmetic. Nothing here is scheduled.

One thing the attempt did establish, worth keeping: the widening is *necessary*, not
cosmetic. `DECIMAL(10,2)` holds `99999999.99`, its sum with itself is
`199999999.98`, and casting that back to `DECIMAL(10,2)` is an out-of-range error on
DuckDB. Both engines answer `DECIMAL(11,2)`; a type that says `10 2` would be
describing a value it cannot hold.

### Other closed items

- `join` and `crossJoin`. The join family had six members and neither of the two names a
  SQL reader reaches for first. `join = innerJoin`; `crossJoin s = innerJoin s true`,
  because the join machinery threads a predicate through every stage and `ON TRUE` is the
  same product — measured on five engines, with SQLite giving both forms one query plan.
- `over { … } (rowNumber)` says the function already takes the spec. The old message
  (`expected agg (…), found winspec ?a -> win (…)`) described the overload set rather than
  the mistake. Reported in `infer_app`, where the argument's type is still available.
- `docs/PERFORMANCE.md` no longer implies `nix develop` is needed for the benchmark.

**Checked and not a defect:** field names never need quoting — `.order` as a column and
`{ order = .id }` as an output both reach the SQL as `"order"`, which is what
`LEARN.md` already says. The syntax error I hit came from writing `{ "order" = .id }`,
which the language does not offer and does not need to.

- `monthsBetween`/`yearsBetween`/`hoursBetween`/`minutesBetween`/`secondsBetween`, and the
  `second` field they needed. The calendar ones are the **elapsed** count, written from
  `yearOfDate`/`monthOfYear`/`dayOfMonth` rather than delegated to `DATE_DIFF`, which counts boundaries and
  disagrees with an age (`2024-01-31` to `2024-02-01` is `0` elapsed, `1` crossed).
  Measuring the clock ones found two bugs: the obvious `hour b - hour a` is wrong across
  midnight, and SQLite's `CAGARA_PART` had a `_ =>` catch-all that made *any* newly
  accepted unit lower to the quarter formula.
- `exp`, `ln`, `log10`, `log2`, `logBase`, `cbrt`, `degrees`, `radians` — and deliberately
  no `log`, which means base 10 on DuckDB and SQLite, base `e` on MariaDB, and is refused
  by Trino. `cbrt` is `exp(ln|x|/3)` and `logBase` is `ln x / ln base`, both because
  measurement showed the obvious spelling wrong: `power(abs(x), 1.0/3)` is
  `1.9999999986` on MariaDB, and `sqlglot`'s generator swaps the arguments of
  two-argument `LOG`.
- `workspace/symbol` in the language server: a definition in any open file.
- `cagara fmt` walks a directory with the same walk `cagara check` uses.

- `position`, `ascii`, `chr`, `repeat`, `reverse` and `listAt`. Every spelling was found by
  measurement, and two were not what the documentation suggests: `repeat` is
  `replace(hex(zeroblob(n)), '00', s)` on SQLite and `array_join(repeat(s, n), '')` on Trino
  (whose `repeat` returns an *array*), and `ascii` is `codepoint` with a `varchar(1)` cast on
  Trino, whose type check runs before constant folding. `reverse` and `listAt` are **refused**
  on the targets that cannot express them, with a message naming what is missing.

**`listMap` and `listFilter` cannot be built, and this was measured rather than assumed.**
They need the engine to run a lambda over each element — DuckDB's
`list_transform(xs, x -> ?x + 1)`, PostgreSQL's `ARRAY(SELECT f(x) FROM unnest(xs) x)`. A
Cagara lambda is a *compile-time* value: it beta-reduces away, and a `sql "…"` template
receives its argument as an opaque expression, not as something whose parameter can be
substituted. Measured: a helper `lf : (expr ?r int -> expr ?r bool) -> … = p => xs => xs`
compiles and the lambda is simply dropped — the generated SQL is `SELECT xs AS v`. There
is no way to render `x -> ?x + 1` from it, so both functions would be a lie. Doing it
properly means either a new IR node for a lambda-over-list (and a lowering rule per
target) or a stage that takes an expression over `value` and a list — a language change,
not a prelude addition.

- `textDocument/signatureHelp`: which argument of the innermost call the cursor is in.
  `textDocument/documentColor` stays refused on purpose — the language has no colour type.
- Dependencies updated: `sqlglot-rust` 0.11.2, `logos` 0.16.1, `serde_json` 1.0.154.
  `rowan` stays at 0.17 (0.18 is an alpha). The new sqlglot still does not parse `GROUPS`
  and still swaps two-argument `LOG`'s arguments; it adds `Expr::ArrayIndex`, which cannot
  be used for `listAt` because the generator omits the parentheses an aggregate needs.

- `random`, refused on SQLite: its `random()` is a signed 64-bit integer, and
  `random() / 2^63` landed outside `[0, 1)` in 490 of 1000 measured draws. It is also the
  one prelude function the differential suite cannot check by rows, which its test says.

- A stage given no argument says what it needs. `q = u & where` reported
  `expected query {…}, found expr ?a b`; the check had to go in `unify_inner`'s `Fun`/`Fun`
  arm, where both whole function types still exist — three attempts in `infer_app` and
  `mismatch` never fired, because by then the stage's type is gone. Two measured details
  decided the wording: the parameter is `expr ?a b` with `b` still open (so the printed form
  cannot be matched), and `select`/`agg` have a `record_expr` parameter rather than an
  `expr`. An arity test keeps the prelude's own stage-shaped `_&|_` out.

- `& limit -1` says what it parsed. `-` binds tighter than `&`, so it is `limit - 1` —
  a subtraction from the stage — and the diagnostic was `expected expr a b, found expr a
  int -> query a -> query a`. The check has to run *before* the argument is checked,
  because the failure is raised by a queued constraint one level below the application.
  Narrowed to operands whose type is a function or an `agg`/`win` producer, so `age - 1`
  on a column is untouched.
- `agg { x = sum -1 }` — the same slip one level down, caught at the application rather
  than in `infer_app`'s argument loop: the callee is still `-` there and the operand is the
  reader's node, whereas inside the loop the failure is already against a parameter type.

- A lowering refusal names the stage it is about. `cagara-sql` returned
  `Result<String, String>`, so every refusal reached the user as `file:0:0` — a
  pre-commit hook's output was unusable and an editor would underline the first line.
  `compile_located` carries the location of the innermost stage; `compile` is a wrapper
  that drops it. Two places had to record it, and both were dropping one:
  `rel_peeled` strips a run of `At`/`Where` iteratively (that is what makes a long
  pipeline cost one stack frame instead of one per stage), so `lower_at` is never
  reached for those nodes; and a check *inside* a `lower_*` runs after `rel` restored
  the outer location.
- The language server answers `rename`/`prepareRename`, `foldingRange` and
  `semanticTokens/full`. Rename's materials were already there (`occurrences` is
  cross-file, `references` returns paths); what was missing was the wiring, so the
  server advertised eight capabilities and refused the ninth. Its refusals carry a
  reason, because an empty edit set and "cannot rename this" are different things.
- Every dialect's evidence is a table, not a paragraph: `VERIFIED` in `engines.rs`
  marks each of the 22 accepted dialects `Engine(name)`, `Emulated` or `Text`, and two
  tests keep it honest — the table covers every accepted dialect, claims no engine
  without a harness, and its `Emulated` set is what the emulation test iterates. The
  `range`/`groups` work is why: MariaDB rejects `GROUPS` and refuses a two-key `range`
  frame, and no emulation could have shown either.

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

See [`CORE.md`](CORE.md) for semantics and [`ROW-TYPES.md`](ROW-TYPES.md) for
row algebra.
