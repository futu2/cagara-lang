# A Cagara guide

Cagara is a small typed query language that describes a SQL query as a
**typed value**. You build a query by starting from a table and passing it
through a pipeline of stages. Because the whole query is a value, the compiler
can check it — unknown columns, ungrouped fields, `NULL` misuse, and misplaced
aggregates are all reported before any SQL reaches a database.

This guide is meant to be read start to finish. Every example is compiled
output you can reproduce with the `cagara` binary.

- [1. The shape of a program](#1-the-shape-of-a-program)
- [2. Tables: declarations, not connections](#2-tables-declarations-not-connections)
- [3. Pipelines and the `&` operator](#3-pipelines-and-the--operator)
- [4. Columns and expressions](#4-columns-and-expressions)
- [5. Choosing columns: `select` and `update`](#5-choosing-columns-select-and-update)
- [6. Filtering, sorting, and paging](#6-filtering-sorting-and-paging)
- [7. Aggregation](#7-aggregation)
- [8. Nulls](#8-nulls)
- [9. Joins](#9-joins)
- [10. Window functions](#10-window-functions)
- [11. Set operations and `distinct`](#11-set-operations-and-distinct)
- [12. Functions, types, and the prelude](#12-functions-types-and-the-prelude)
- [13. String and date functions in practice](#13-string-and-date-functions-in-practice)
- [14. Writing your own SQL with `sql` templates](#14-writing-your-own-sql-with-sql-templates)
- [15. Modules](#15-modules)
- [16. What the compiler emits](#16-what-the-compiler-emits)
- [17. Command line](#17-command-line)
- [18. Errors you will meet](#18-errors-you-will-meet)
- [19. Style guide](#19-style-guide)
- [20. Where to go next](#20-where-to-go-next)

---

## 1. The shape of a program

A Cagara file is a sequence of top-level definitions. Comments start with `#`
and run to the end of the line. A `##` line documents the definition below it
and a `#!` block documents the file; `cagara doc` turns them into a reference,
and `cagara doc --prelude` prints the prelude's own (see §17).

```haskell
# A definition is `name = expression`, optionally with a type annotation.
users : query { id = int, name = string, age = int, active = bool } =
  table "public" "users"

adults = users
  & where (.age >= 18 && .active)
  & select { id = .id, name = .name }
  & order [asc .name]
  & limit 10

# `main` is the definition printed by `cagara file.cagara`.
main = adults
```

Three ideas carry most of the language:

1. **A query is a value.** `users` is not a connection or a handle; it is a
   description of a relation. `adults` is another one, derived from it.
2. **A pipeline is built with `&`.** Each stage takes the query on its left and
   produces a new query.
3. **The compiler knows your columns.** Every row has a fixed set of named
   columns with types, and the pipeline carries that record from stage to stage.

There is no `SELECT` keyword, no `FROM`, and no statement structure. Those come
out the other end as SQL.

The definition named `main` is the file's entry point. The other definitions
are helpers that build values for it; use `--only NAME` when you want to
inspect one of them directly. A file without `main` is still checked, but it
prints no SQL because it is a library.

Definitions start in column 0; every continuation line is indented. A line that
starts with an *identifier* in column 0 begins a new definition, so an
unindented continuation that starts with a name is reported as a layout error; a
line that starts with an operator (`& where …`) is a continuation and is
accepted. A top-level `;` is an optional item terminator, useful when generated
code cannot guarantee indentation. The formatter leaves a `;`-terminated item
exactly as written — terminator included — rather than re-laying it out.

### Column references start with a dot

`.name` means "the column `name` of the input row". This small syntax does a lot
of work: it makes column references visually distinct from function names and
lets the compiler tell them apart unambiguously.

---

## 2. Tables: declarations, not connections

A table declaration gives the compiler the table's name and row schema. It does
**not** connect to a database or inspect a live catalog. Nothing here is
verified against a real server; your annotation is the source of truth.

```haskell
users : query { id = int, name = string, age = int, active = bool } =
  table "public" "users"

orders : query { id = int, user_id = int, amount = float, status = string, created_at = date } =
  table "public" "orders"
```

`table "schema" "name"` takes two string literals. The annotation is mandatory:
without a row type, the compiler could not check anything downstream.

The scalar types are:

| Type | SQL equivalent | Notes |
|---|---|---|
| `int` | integer | 64-bit; `/` truncates toward zero |
| `float` | floating point / decimal | no implicit conversion to or from `int` |
| `string` | character text | literals in double quotes |
| `bool` | boolean | `true` / `false` comparisons produce it |
| `date` | date | string literals widen to it |
| `timestamp` | timestamp | string literals widen to it |

There is also `maybe a`, a value that may be SQL `NULL`; see
[section 8](#8-nulls).

There are no user-declared types, and a name in a type position that is not one
of the constructors above is not "an unknown type" — it is a **type parameter**,
the same mechanism as the `a` in `id : a -> a`. The definition becomes
polymorphic in it and every use instantiates it afresh, so a column written
`amount = decimal` can be used as a `string` in one query and as an `int` in
another, and neither is rejected. A typo in a column type is therefore silent;
use the names in the table verbatim, and treat a type that has to mean something
specific as one of those six plus `maybe`/`list`.

A column may be declared nullable:

```haskell
users : query { id = int, deleted_at = maybe timestamp } = table "public" "users"
```

### Literals

| Literal | Type |
|---|---|
| `42` | `int` |
| `1.5` | `float` |
| `"hello"` | `string` |
| `true` / `false` | `bool` |

A string ends at the end of its line, so a missing closing quote is reported
near where you made the mistake instead of swallowing the rest of the file.

Numeric literals are *polymorphic when their context calls for it*, exactly like
Haskell's: `1.5` is a `float`, but `1` can be used where a `float` is expected.
This is the **only** implicit conversion. Values never convert implicitly:

```haskell
bad = orders & select { x = .amount + .user_id }   # int and float: type error
```

---

## 3. Pipelines and the `&` operator

`&` applies the stage on its right to the query on its left. It is
left-associative and sits just above `$` — the loosest operator — so a pipeline
reads top to bottom:

```haskell
revenue = orders
  & where (.status == "paid")
  & select { user_id = .user_id, amount = .amount }
  & order [desc .amount]
  & limit 10
```

Each stage has a type. `where` takes a query and a predicate and returns a
query; `select` takes a query and a record of fields and returns a query.
`&` just applies one to the other, so `q & where p` means `where p q`. The
subject comes last — the same convention as the whole prelude, and the reason
partial application works so well (see [section 12](#12-functions-types-and-the-prelude)).

### Shorthands

Every common stage has a one-token shorthand at the same precedence as `&`:

| Shorthand | Long form |
|---|---|
| `&?` | `where` |
| `&=` | `select` |
| `&+` | `update` |
| `&*` | `agg` |
| `&.` | `order` |
| `&-` | `limit` |
| `&\|` | `union` |
| `&!` | `unionAll` |
| `&^` | `intersect` |
| `&~` | `except` |

This table is the whole set: there is no shorthand for `offset`, `distinct`,
`omit`, `prefix` or `suffix`. The symbols are **mnemonic rather than initial** —
`?` for a filter, `=` for replacing the row, `*` for grouping, and the four set
operations taking the arithmetic-looking `|`, `!`, `^`, `~` — so a symbol cannot
be inferred from the stage's name and this table is the only place they are all
listed. That matters because an incomplete table is how a wrong entry hides:
`&^` was documented as `offset` for several releases, and a reader copying it got
a type error.

These are equivalent pairs:

```haskell
a = users &? (.age > 1) &+ {age = .age + 1} &= {.id, .age} &. [asc .age] &- 2
b = users & where (.age > 1) & update {age = .age + 1} & select {.id, .age} & order [asc .age] & limit 2
```

Both produce:

```sql
SELECT id, age + 1 AS age FROM public.users
WHERE (age > 1) ORDER BY age + 1 NULLS LAST LIMIT 2;
```

Note that `&=` and `&+` are not interchangeable. `&=` **replaces** the row with
the fields you list; `&+` **merges** over it, keeping every column you did not
mention:

```haskell
keeps = users &+ {age = .age + 1}   # id, name, age (new), active
drops = users &= {age = .age + 1}   # only age
```

Stage order follows from that: `&+` cannot update a column a previous `&=` has
already dropped. See
[section 5](#5-choosing-columns-select-and-update).

The long names are the easiest to learn first, and they are what the compiler
prints in error messages. Use the shorthands once a pipeline is long enough that
the punctuation reads better.

#### Where the shorthands come from

A shorthand is one line in `prelude.cagara`, which says what it *means*:

```haskell
_&=_ : query r -> expr r (row s) -> query s = q => fields => select fields q
```

The *symbol* is in the compiler's operator table, which is where every
operator's precedence lives. Spelling and meaning are two different facts and
live in two different places: precedence has to be known before a file is
read, so it cannot come from source, while what `&=` does is an ordinary
definition like any other.

There is no special form for a stage. `&`, `&+` and `+` are the same kind of
thing — an infix operator with a precedence and an associativity — and the
table covers all of them:

```
&     left  1     the pipeline
&+    left  1     a stage shorthand, at the pipeline's own level
?     left  3     the join operators, at their own level
+     left  17    arithmetic
```

The shorthand list is not arbitrary: `&` is the pipe, and a symbol after it
names the stage it stands for. `&^` in particular means **intersect**, not
`offset` — the set operations take the symbols `&|`, `&!`, `&^` and `&~`, and
`offset` has no shorthand. Passing a number to `&^` is a type error, because it
combines two queries.

What makes a shorthand a *pipeline stage* is only where it sits: an operator
at `&`'s precedence and associativity is a stage, and one at any other level —
the joins at 3, or `+` at 17 — is an ordinary operator. That is why `q &+ {..}`
chains like `q &` while `q ? on` does not, and it is the only reason: nothing
has to mark an operator as a stage, so nothing can disagree with the level.

A symbol may be any operator spelling — `&?`, `~=`, `>>`. Every operator you
*write* has to be in the table, so a typo is a compile error pointing at the
operator rather than a silently different parse:

```haskell
bad = users &>> 5   # unknown operator `&>>`
```

`&>>` is deliberately not in the table. The example used to use `&~`, which
*is* — it is `except` — so the snippet demonstrated a type error while claiming
to show an unknown operator.

`infixl` and `infixr` are not part of the language: precedence is a property of
the language, so it is fixed in the compiler and no module can declare or
re-fix an operator. There is no `infixl 1 &^` line to write. Defining `_&^_`
gives `&^` a meaning; the table gives it its place in the grammar.

Note the spelling boundary. A shorthand is `&` followed by operator
punctuation, and an ordinary operator is a run of operator characters, so both
follow from a character class rather than a list of names. Those classes are
`&` plus any of `= ! < > * / % | ? $ ^ ~ + - .`, and `= ! < > * / % | ? $ ^ ~`
on their own. A shorthand built from characters already in the classes needs
one line in the table; a character never used in an operator before needs one
line in the lexer as well, after which every spelling made of those characters
is free too.

---

## 4. Columns and expressions

Inside a stage, `.name` refers to the input row's `name` column. Expressions
combine columns, literals, and functions with the usual operators.

```haskell
t = orders & select {
  id = .id,
  gross = .amount * 1.2,
  label = upper .name,
  is_big = .amount > 100.0,
  both = .amount > 100.0 && .status == "paid"
}
```

### Local bindings

`let name = value in body` names an intermediate value inside an expression.
It is lexical, monomorphic, and inlined at compile time — no SQL stage is added:

```haskell
t = orders & select {
  id = .id,
  total = let base = .amount * 1.2 in base + 1.0,
  doubled = let x = .id in x + x
}
```

The binding cannot refer to itself (there is no `let rec`). Use a top-level
definition instead when the helper must be polymorphic or shared between
definitions; top-level definitions are generalized, local bindings are not.

### Operator precedence

From tightest to loosest. The levels are fixed by the compiler; a larger number
binds tighter, and application (`f x y`) is tighter than any operator. A source
file cannot declare an operator or change one's level.

| Level | Operators | Associativity |
|---|---|---|
| — | application (`f x y`) | left |
| 23 | `>>>` `<<<` (composition) | right |
| 21 | `??` | right |
| 19 | `*` `/` `%` | left |
| 17 | `+` `-` | left |
| 15 | `<>` (string concatenation) | right |
| 13 | `==` `!=` `<` `<=` `>` `>=` | left |
| 11 | `&&` | left |
| 9 | `\|\|` | left |
| 3 | `?` `<?` `?>` `<?>` (joins) | left |
| 1 | `&` and the stage shorthands | left |
| 0 | `$` | right |

Two consequences worth remembering:

```haskell
# ?? binds tighter than arithmetic, so this is COALESCE(s, 0) + 1
a = t & select { x = .s ?? 0 + 1 }

# <> is right-associative, like the :: of list languages
b = t & select { l = .first <> " " <> .last }
```

This table is a summary, not the definition: an operator's precedence is what
the compiler's fixed table says. Every operator is fixed there — arithmetic,
comparison, logic and concatenation, the pipeline, the joins, and all four
combinators (`&`, `$`, `>>>`, `<<<`) alike — and the prelude supplies their
meanings (see
[Shorthands](#shorthands)). The gaps in the numbering leave room for the
operators Cagara has that Haskell does not. The words `infixl` and `infixr`
below are Haskell's names for associativity, used only to compare the two
languages; they are not Cagara syntax.

Two associativities differ from Haskell on purpose, both harmless: `&&` and
`||` are `infixl` where Haskell writes `infixr` (both are associative, so the
parse tree is the only difference), and the comparisons are `infixl` where
Haskell writes `infix 4` (non-associative). Cagara's comparisons could be made
non-associative too, but that needs a third entry form in the table, and
`a < b < c` is already rejected as a type error.

`/` on `int` truncates toward zero and `%` keeps the dividend's sign in **every**
dialect — the compiler emits a dialect-appropriate expression rather than
relying on whatever `/` means locally.

### Arithmetic is not overloaded across types

`+ - * /` work on `int` with `int`, or `float` with `float`. Mixing is an
error, and the fix is an explicit cast: `toFloat .user_id`, `toInt .amount`.
`%` is the exception: the prelude defines it for `int` only, so there is no
float remainder — convert with `toInt` first, or write the remainder you need as
a `sql` template.

### Conditionals

`ifThenElse` lowers to a searched `CASE` expression. Both branches must have the
same type.

```haskell
size = orders & select { id = .id, bucket = ifThenElse (.amount > 100.0) "big" "small" }
```

```sql
SELECT id, CASE WHEN (amount > 100.0) THEN 'big' ELSE 'small' END AS bucket FROM public.orders;
```

When there are more than two cases, `case` writes the chain without nesting:

```haskell
size = orders & select {
  id = .id,
  bucket = case {
    .amount > 1000.0 => "huge";
    .amount > 100.0 => "big";
    true => "small"
  }
}
```

```sql
SELECT id, CASE WHEN (amount > 1000.0) THEN 'huge'
                WHEN (amount > 100.0) THEN 'big'
                ELSE 'small' END AS bucket FROM public.orders;
```

Arms are separated by `;` (not `,`, which a record uses) and the whole thing is
braced, so a `case` nests inside a record, an argument list, or another `case`
without ambiguity. Each arm is `condition => value`, using the same `=>` a
lambda does. The conditions are tried top to bottom and the first true one
wins.

The last arm is a condition like any other; writing `true => value` is the
default, and it is **required**. That is what makes the expression total: SQL's
`CASE` with no matching branch is `NULL`, but a `case` takes its type from its
arms, so without a default it would claim a non-null value it can fail to
produce. `true` is the only condition that needs no column, so
`case { true => 7 }` is legal and is just `7`.

The conditions and the values are all expressions over the same row, and every
value must have one type — the same rules `ifThenElse`'s three arguments obey:

```haskell
# A condition must be a bool, and must not be nullable: a NULL cannot choose
# an arm. Use `coalesce` or `isNotNull` first, as in `where`.
bad  = orders & select { b = case { .amount => 1; true => 0 } }
bad2 = people & select { b = case { .score > 10 => 1; true => 0 } }   # .score is maybe
good = people & select { b = case { coalesce 0 .score > 10 => 1; true => 0 } }
```

Because the arms share one phase, a `case` whose arms are aggregates is an
aggregate and belongs in `agg` where `sum` belongs:

```haskell
q = orders & agg { k = group .user_id, b = case { count > 3 => coalesce 0.0 (sum .amount); true => 0.0 } }
```

### Casts

Casts are named by their **result** type: `toInt`, `toFloat`, `toBool`,
`toString`. There is no implicit conversion between ordinary expressions.

A name is **overloaded by its source**, so `toInt` accepts either a `float`
(a narrowing) or a `string` (a parse) — the same destination reached from
different places. Each `to*` also has a `from*` spelling that names the source
instead, which reads better where the column's type is the surprising part:
`fromString .man_grade` says the value is text being read as a number. They are
the same function.

`toDecimal precision scale x` gives a value an exact SQL type —
`toDecimal 18 2 .amount` is `CAST(amount AS DECIMAL(18, 2))`. The language type
stays `float` (there is no separate decimal scalar), but the SQL type is exact,
which matters for money: binary floating point cannot represent it.

`toInt` truncates a `float` toward zero in every dialect, and `toFloat` produces
a 64-bit double; the backend casts to the matching SQL width (`BIGINT`,
`DOUBLE PRECISION`/`DOUBLE`/`FLOAT`/`FLOAT64`) rather than a 32-bit
`INT`/`FLOAT`. `toString` also accepts a `bool` and returns `"true"`/`"false"`.

A cast that cannot be performed **fails the query**. Where the value may not be
convertible — a `string` column holding a code, say — use the `try*` forms
instead: `tryInt`, `tryFloat`, `tryDecimal`. They return `maybe`, because the
NULL has to be handled somewhere:

```haskell
q = t & select { n = coalesce 0 (tryInt .code) }
```

This is not available on SQLite, which has no `TRY_CAST` and casts
unparseable text to `0` rather than failing; see
[SQL dialect support](SQL-DIALECTS.md).

### Identifiers are quoted when needed

A column name that is not a lowercase word, or that collides with a reserved
word, is quoted in the generated SQL using the dialect's quotes:

```sql
SELECT id, (amount > 100.0) AND (status = 'paid') AS "both" FROM public.orders;
```

That is automatic; you never quote a name yourself.

---

## 5. Choosing columns: `select` and `update`

These two stages are the ones people mix up, so it is worth being precise.

**`select` replaces the whole row.** The output row is exactly the fields you
list, in the order you list them.

```haskell
picked = users & select { id = .id, label = upper .name }
```

```sql
SELECT id, UPPER(name) AS label FROM public.users;
```

**`update` merges over the row.** A name the input already has keeps its
position and takes the new expression; a name it does not have is appended;
everything else passes through untouched.

```haskell
greeting = users & update { age = .age + 1, display_name = .name }
```

```sql
SELECT id, name, age + 1 AS age, active, name AS display_name FROM public.users;
```

Note `age` stayed in third position (it existed) and `display_name` was appended.
So `update` is how you rename or recompute a column, and `select` is how you
decide which columns to publish.

```haskell
# Drop password_hash by naming what is published; give name its public label.
public_users = schema.users & select { .id, display_name = .name }
```

The pipeline shorthand for `update` is `&+`, so the two definitions below are
the same query:

```haskell
long_form  = users & update { name = upper .name } & select {.id, .name}
short_form = users &+ { name = upper .name } &= {.id, .name}
```

```sql
SELECT id, UPPER(name) AS name FROM public.users;
```

`&+` is easy to confuse with `&=`, so remember that the symbol points at the
operation: `&+` adds to the row, `&=` writes the row.

### The `{.name}` shorthand

A field written `{.name}` is short for `{name = .name}`. It mixes freely with
computed fields:

```haskell
t = users & select {.id, label = upper .name}
```

Use this to select existing columns; use `name = expr` to compute or rename.

### Fields are static

Every row is fully known: a field list names every output column, and the
compiler knows the resulting row before any SQL is emitted. `omit` drops one
column, while `prefix` and `suffix` rewrite names through type-level row maps.

### Dropping a column: `omit`

`omit "k"` removes one column and leaves every other column exactly where it
was:

```haskell
public_users = schema.users & omit "password_hash"
```

```sql
SELECT id, name, age, active FROM public.users;
```

This is the one thing `select` cannot express, and the difference is worth
knowing. `select` is a whitelist, so it fails *closed*: a column added to
`users` tomorrow is silently not published. `omit` is a blacklist and fails
*open*: the new column passes through. Pick whichever behaviour you want,
deliberately.

Because `omit` knows the row it produces, later stages see the column is gone:

```haskell
ok  = users & omit "age" & where (.active)
bad = users & omit "age" & where (.age > 1)   # error: no column `age`
```

And it composes like any other transformation, because the row it produces is
computed from the query it is applied to:

```haskell
no_id = omit "id"      # one definition...
a = users & no_id      # ...used on different tables
b = orders & no_id
```

`a` is `users` without `id`; `b` is `orders` without `id`. The key must be a
*literal* where the stage is applied — the column has to be known when the query
is checked — so a computed name is refused:

```
error: `omit` needs a literal column name, such as `omit "password_hash"`: the column must be known when the query is checked
```

It may still reach the stage through a helper's parameter, because the label is
bound at the call site: `no_id = k => q => omit k q` then `users & no_id "id"`.

### Rewriting names: `prefix` and `suffix`

`prefix "text"` and `suffix "text"` apply a rewrite to **every** column name:

```haskell
prefixed = users & prefix "u_"               # id -> u_id, name -> u_name, ...
suffixed = users & suffix "_v2"              # id -> id_v2, ...
```

```sql
SELECT id AS u_id, name AS u_name, age AS u_age, active AS u_active FROM public.users;
```

A name rewrite keeps field order and types; only labels change, and the rewrite
is injective (`suffix "_2"` takes `a_2` to `a_2_2`), so a rewrite cannot make two
columns collide. Removing a column is `omit`'s job; the error it raises for a
column that is not there is the one to expect from this family of stages
(`no column 'salary'; available: id, name, age`).

There is no `pick`, `mapKeys`, or `Labels` type. Row maps keep names in the
row algebra while preserving ordinary type unification. See the
[row type reference](ROW-TYPES.md) for the rules.

| You want | Write |
|---|---|
| keep these columns | `select {.a, .b}` |
| drop one column | `omit "b"` |
| add a prefix or suffix to every key | `prefix "u_"` / `suffix "_v2"` |
| recompute a column in place | `update {a = .a + 1}` |
| keep both sides of a join | rename one side *before* the join |

One trap is worth stating here, because the two stages look similar:
`update {new = .old}` **appends** `new` and keeps `old` in place, so it adds a
column rather than moving one. Use `prefix` or `suffix` when changing every
column name. Section
[5](#5-choosing-columns-select-and-update) covers `update` in full.

**Keeping both sides of a join.** Since a shared name resolves to the left
column, rename on one side *before* joining. `.<x` / `.>x` belong to the join
predicate only, so `.>id` is **not** available after the join:

```haskell
order_names = orders
  & select { .user_id, .amount, order_id = .id }
  & innerJoin users (.<user_id == .>id)
  & select { order_id = .order_id, name = .name, amount = .amount }
```

See [section 9](#9-joins) for the join output rules this follows from.

---

## 6. Filtering, sorting, and paging

`where` keeps rows for which the predicate is true. It takes a `bool`
expression.

```haskell
adults = users & where (.age >= 18 && .active)
```

`order` sorts. Sort keys are written with `asc` or `desc`, and take an
expression rather than just a column:

```haskell
ranked = orders & order [desc .amount, asc .id]
```

**NULLs sort last in both directions.** This is a deliberate choice that makes
the two directions stable with respect to each other, and the compiler emits
whatever each dialect needs to achieve it (`NULLS LAST`, or a leading
`CASE WHEN x IS NULL` key where the dialect lacks it).

`limit` and `offset` cap and skip rows:

```haskell
page2 = orders & order [asc .id] & limit 20 & offset 20
```

`asc` / `desc` take an expression, not another sort key: `asc (desc .x)` is a
type error.

### How `where` positions affect the SQL

`where` fuses into the query below it when that is safe:

```haskell
a = orders & where (.status == "paid") & select {.id, .amount} & where (.amount > 10.0)
```

```sql
SELECT id, amount FROM public.orders WHERE (status = 'paid') AND (amount > 10.0);
```

A `where` after `agg` is a boundary too. The predicate names an aggregate
output, so the aggregate is lowered into a subquery and the filter sits outside
it:

```haskell
b = orders & agg { n = count } & where (.n > 5)
```

```sql
SELECT n FROM (SELECT COUNT(*) AS n FROM public.orders) AS t1 WHERE (n > 5);
```

Folding that filter into a `HAVING` on the aggregate is a query-shape
optimization this compiler does not currently perform. See the aggregate-filter
note under *Current boundaries* in [`PLAN.md`](PLAN.md) for why it is absent.

A `limit` is also a boundary, and so is an `order` or `limit` sitting between
the `agg` and the `where`: SQL cannot filter on a limited result in the same
`SELECT`.

```haskell
c = orders & select {.id} & limit 5 & where (.id > 1)
```

```sql
SELECT id FROM (SELECT id FROM public.orders LIMIT 5) AS t1 WHERE (id > 1);
```

You never write the subquery yourself — but knowing when one appears explains
the SQL you get.

---

## 7. Aggregation

`agg` reduces the input to groups. Fields marked `group` identify each group;
every other field must be an aggregate expression. There is no implicit
grouping: the compiler will not guess.

```haskell
by_user = orders
  & agg {
      user_id = group .user_id,
      revenue = sum .amount,
      orders = count
    }
```

```sql
SELECT user_id, SUM(amount) AS revenue, COUNT(*) AS orders FROM public.orders GROUP BY user_id;
```

If **no** field is grouped, the result is one row for the whole input — a global
aggregate:

```haskell
totals = orders & agg { total = sum .amount, n = count }
```

```sql
SELECT SUM(amount) AS total, COUNT(*) AS n FROM public.orders;
```

### Ungrouped columns are rejected

This is the error the type system exists to catch:

```haskell
bad = users & agg { n = count, name = .name }
```

```
error: field `name` uses a column that is not grouped; wrap it in `group` or aggregate it
```

Adding `name = group .name` fixes it.

### Aggregates do not nest

An `agg` field is **one** aggregate expression. `sum (sum .age)` is rejected:

```
error: this argument contains an aggregate; aggregates cannot nest
       (aggregate in an earlier `agg` stage)
```

To combine aggregates, use an earlier `agg` stage and then compute on its
output. Aggregates also belong only in `agg`; putting one in `select` is its own
error, with its own message:

```
error: field `s` is an aggregate; aggregates belong in `agg`, not `select`
```

### Aggregates return `maybe`

`sum`, `avg`, `min`, and `max` return `maybe` because SQL produces `NULL` for an
all-`NULL` or empty input. `count`, `countOf`, and `countDistinct` return a
non-null `int`. The filtered forms follow the same rule: `sumFilter`,
`avgFilter`, `minFilter`, and `maxFilter` are `maybe`, while `countFilter`,
`countOfFilter`, and `countDistinctFilter` are non-null `int`. See the next
section.

### Nullable aggregate inputs

A column declared `maybe` is not accepted by `sum`, `avg`, `min`, `max`,
`countOf`, or `countDistinct`: those signatures say non-null. The explicit
spelling is a `*Maybe` helper, which passes the column straight to the SQL
aggregate because SQL already skips `NULL`:

```haskell
q = t & agg {
  total = coalesce 0 (sumMaybe .amount),
  average = avgMaybe .amount,
  n = countOfMaybe .code
}
```

This is not the same as coalescing first. Over `[NULL, 5]`, `avgMaybe` is `5`,
while `avg (coalesce 0 .amount)` is `2.5`; `minMaybe` over `[NULL, 5]` is `5`,
while the coalesced form is `0`. Use `coalesce` when a missing value should
count as a default; use `*Maybe` when SQL's null-skipping meaning is what you
want. The value forms still return `maybe` for an all-`NULL` or empty input;
the count forms return non-null `int`. There are no `*MaybeFilter` forms yet —
filter the rows with `where` first, or combine the predicate and null test.

### Filtered aggregates

A predicate on an aggregate's **input rows** is written with the `*Filter`
functions. They are the portable spelling of SQL's
`SUM(x) FILTER (WHERE p)`: the filter changes which rows reach each aggregate,
not which groups are kept.

```haskell
sales = orders & agg {
  user_id = group .user_id,
  paid = sumFilter (.status == "paid") .amount,
  refunded = sumFilter (.status == "refunded") .amount,
  n_paid = countFilter (.status == "paid")
}
```

```sql
SELECT user_id,
       SUM(CASE WHEN (status = 'paid') THEN amount END) AS paid,
       SUM(CASE WHEN (status = 'refunded') THEN amount END) AS refunded,
       COUNT(CASE WHEN (status = 'paid') THEN 1 END) AS n_paid
FROM public.orders
GROUP BY user_id;
```

The predicate is a non-null `bool` over the input row and cannot contain an
aggregate or window. It is evaluated before grouping, so it may name a column
that is not a group key (`status` above). The `CASE` has no `ELSE`: a group with
no matching rows is `NULL` for `sumFilter`, `avgFilter`, `minFilter`, and
`maxFilter`, and `0` for the count forms — the same results `FILTER` gives. Do
not add `ELSE 0` yourself; for `avg` it changes the denominator, and for the
other value aggregates it changes an empty group from `NULL` to `0`.

When **every** aggregate in the stage shares one predicate, filter before
`agg` instead:

```haskell
paid = orders & where (.status == "paid") & agg { user_id = group .user_id, total = sum .amount }
```

That filters the rows once. The `*Filter` forms exist for the case where
different aggregates in one stage need different predicates.

---

## 8. Nulls

Nullability is a real part of the type system, not an afterthought. `maybe a` is
the type of a value that may be SQL `NULL`, and — importantly — **a type
variable in a signature means a non-null type**, so `expr r a` and
`expr r (maybe a)` are different types. Operators require non-null arguments.

Where nullability comes from:

| Source | Result |
|---|---|
| A column declared `maybe int` | reads as nullable |
| The possibly-absent side of a left, right, or full join | its columns become `maybe` |
| `sum`, `avg`, `min`, `max` and their `*Filter` forms | `maybe` |
| `sumMaybe`, `avgMaybe`, `minMaybe`, `maxMaybe` | `maybe` |
| `lag`, `lead`, `sumOver`, `avgOver` | `maybe` |
| `count`, `countOf`, `countDistinct`, `countOver`; `countFilter`, `countOfFilter`, `countDistinctFilter` | non-null `int` |
| `countOfMaybe`, `countDistinctMaybe` | non-null `int` |
| `isNull`, `isNotNull`, `isTrue` | non-null `bool` |

Handling nulls explicitly:

| Function | Meaning |
|---|---|
| `coalesce default x` | use `default` if `x` is `NULL` |
| `x ?? default` | the same, infix (and it binds tighter than arithmetic) |
| `nothing` | SQL `NULL` at any nullable type |
| `just x` | mark a non-null value as nullable |
| `nullIf x y` | return `NULL` when `x` equals `y` |
| `isNull x` / `isNotNull x` | test for `NULL`, returning non-null `bool` |
| `isTrue x` | treat a nullable boolean `NULL` as false |
| `whereTrue p` | filter with a nullable predicate, treating `NULL` as false |
| `eqMaybe x y` | compare nullable values, treating two `NULL`s as equal |

`nothing` has type `maybe a`; a surrounding use fixes `a` (`coalesce 0 nothing`
is `int`). A bare `x = nothing` projection remains `maybe a` until another use
constrains it. `just x` is the opposite direction: it keeps `x` and only changes
its type to `maybe`.

The payoff is that the compiler refuses to let a `NULL` slip into a computation
where it would silently poison the result:

```haskell
u : query { id = int } = table "public" "users"
o : query { user_id = int, amount = float } = table "public" "orders"

t = u & leftJoin o (.<id == .>user_id) & select {.id, amt = .amount + 1.0}
```

```
error: field `amt`: field `amount`: type mismatch: expected float, found maybe float;
       only one side is nullable: `coalesce default x` takes a `maybe`, `just x` makes one
```

The fix is to say what a missing amount means:

```haskell
t = u & leftJoin o (.<id == .>user_id) & select {.id, amt = coalesce 0.0 .amount + 1.0}
```

`countOf` follows the same non-null rule even though SQL's `COUNT(x)` skips
nulls. Use `countOfMaybe` for that meaning: it counts matched rows across a
left join, because the synthetic null-extended row has a `NULL` key:

```haskell
matches = users
  & leftJoin orders (.<id == .>user_id)
  & agg { id = group .id, n = countOfMaybe .user_id }
```

The same count without a nullable helper sums an indicator:

```haskell
matches = users
  & leftJoin orders (.<id == .>user_id)
  & agg { id = group .id, n = coalesce 0 (sum (ifThenElse (isNotNull .user_id) 1 0)) }
```

The `*Maybe` value aggregates (`sumMaybe`, `avgMaybe`, `minMaybe`, `maxMaybe`)
similarly preserve SQL's null-skipping semantics instead of treating a missing
value as zero.

For a full treatment, see the [nullability reference](NULLABILITY.md).

---

## 9. Joins

A join takes another query and a predicate. **The predicate must name which
input each column comes from**: `.<x` is the left input, `.>x` is the right.

```haskell
user_orders = users
  & innerJoin orders (.<id == .>user_id)
  & select { id = .id, amount = .amount }
```

| Function | Shorthand | Keeps |
|---|---|---|
| `innerJoin q on` | `?` | only matching rows |
| `leftJoin q on` | `<?` | all left rows |
| `rightJoin q on` | `?>` | all right rows |
| `fullJoin q on` | `<?>` | all rows from both |
| `semiJoin q on` | — | left rows with a match (left columns only) |
| `antiJoin q on` | — | left rows with no match (left columns only) |

```haskell
# The same join written with the shorthand.
user_orders = users & orders ? .<id == .>user_id
```

Using a plain `.x` in a join predicate is a specific, helpful error:

```
error: join predicates must say which input a column comes from:
       `.<x` (left) or `.>x` (right)
```

And `.<x` / `.>x` are **only** valid inside a join predicate. Outside it,
`.id` refers to the joined output row:

```
error: `.<x` and `.>x` refer to the inputs of a join
       and can only be used in a join predicate
```

### Output columns of a join

The output has the left input's columns, then the right input's columns that the
left does not already have. **On a shared name, the left column wins.** This
keeps the common case (join on a key, keep the left key) simple.

When you need both values, rename before joining — `update` is the tool:

```haskell
order_names = orders
  & select { .user_id, .amount, order_id = .id }
  & innerJoin users (.<user_id == .>id)
  & select { order_id = .order_id, name = .name, amount = .amount }
```

### Nullability after an outer join

After a left join, the right side's columns are `maybe` (never doubly so), and
the join predicate itself still sees the plain, non-null types. This is what
makes the errors in [section 8](#8-nulls) appear exactly where the null could
originate.

---

## 10. Window functions

A window function computes across a set of rows related to the current one. In
Cagara **a window is an ordinary expression** — it has a value, so it composes,
and you can use it in arithmetic and in `select`/`update` fields.

The spec is a record:

```haskell
spec = { partition = [.user_id], order = [desc .created_at] }
```

| Field | Meaning |
|---|---|
| `partition` | list of expressions; rows with equal values form one window |
| `order` | list of `asc`/`desc` keys, ordering rows within the window |
| `frame` | a frame such as `wholePartition` or `runningFrame` |

All three are optional; `{}` is a valid spec.

> **`[..]` is a fixed-length key list, not a Haskell list.** Despite the
> spelling, `[a, b]` is not a cons cell or a linked list with a `[]`/`(:)`
> algebra. There is no `map`, no concatenation, and no list-typed variable you
> can build up. It is a *syntax for writing a fixed list of sort keys or
> partition keys*, and it is only meaningful where keys are expected: as the
> argument of `order`, or in a spec's `partition` and `order` fields. A list of
> column expressions is inferred as a `sortkey` row, which is why putting one
> anywhere else is a type error:

```haskell
bad  = users & select {.id, ks = [asc .age]}   # field `ks` of `select` must be a
                                               # column expression or constant, found list
bad2 = users & where ([1, 2] == [1, 2])        # type mismatch: expected expr a b, found list int
bad3 = users & select {.id, rn = rowNumber [asc .age]}
                                               # expected winspec a, found list (sortkey ...)
```

> Note the last one: a spec is a **record**, not a list. `rowNumber [asc .age]`
> is rejected; write `rowNumber {order = [asc .age]}`. Elements are homogeneous,
> so `[1, 2.0]` is a type error rather than a promoted list. An empty `[]` is
> accepted wherever keys are expected.

| Function | Returns |
|---|---|
| `rowNumber spec` | `int` |
| `rank spec`, `denseRank spec` | `int` |
| `lag spec expr`, `lead spec expr` | `maybe` (NULL at the edge) |
| `sumOver spec expr`, `avgOver spec expr` | `maybe` |
| `countOver spec` | `int` |

```haskell
ranked = orders
  & select {.id, rn = rowNumber {partition = [.user_id], order = [desc .amount]}}
```

```sql
SELECT id, ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY amount DESC NULLS LAST) AS rn
FROM public.orders;
```

A running total uses a frame:

```haskell
running = orders
  & select {
      .id,
      total = sumOver {partition = [.user_id], order = [asc .created_at], frame = runningFrame} .amount
    }
```

```sql
SELECT id,
       SUM(amount) OVER (PARTITION BY user_id ORDER BY created_at NULLS LAST
                         ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS total
FROM public.orders;
```

Frames are built from `rows` and the bounds `unboundedPreceding`,
`unboundedFollowing`, `currentRow`, `preceding n`, `following n`. Two named
frames cover the common cases: `wholePartition` and `runningFrame`.

Note that a frame is **not** a spec. `lag wholePartition .amount` is a type
error (`expected winspec a, found frame`); wrap it:
`lag {frame = wholePartition} .amount`.

### Windows may not go where a value cannot

Because a window is computed per row, it cannot appear in a place that must be
normalized before rows exist. These are rejected, each with a targeted message:

```haskell
bad1 = orders & where ((rowNumber {order = [desc .amount]}) <= 3)
# this argument contains a window function; windows cannot nest
# (compute it in an earlier `select` stage)

bad2 = orders & order [rowNumber {}]
# sort and partition keys must be plain column expressions;
# compute aggregates or windows in an earlier stage

bad3 = orders & innerJoin x (.<id == (.>id + rowNumber {}))
# join predicates cannot contain aggregates or window functions
```

Note that the error is about *building* a window in that position, not about
reading a column that a window produced. Once `rowNumber` has been selected into
a plain column, filtering on that column is perfectly normal — which is what the
next snippet does. The distinction is the whole point: a window is not a value
until a stage has computed it.

The pattern is always the same: compute the window in its own stage, then use
the new column in the next one.

```haskell
ranked = orders & select {.id, rn = rowNumber {order = [desc .amount]}}
top3   = ranked & where (.rn <= 3)
```

Compare `bad1` above with `top3` here. They look almost identical, but `.rn` in
`top3` is an ordinary `int` column of `ranked`, computed by an earlier stage,
whereas `bad1` asks `where` to build a window itself. Only the second is legal:

```sql
SELECT id, rn
FROM (SELECT id, ROW_NUMBER() OVER (ORDER BY amount DESC NULLS LAST) AS rn
      FROM public.orders) AS t1
WHERE (rn <= 3);
```

Windows nest just as little as aggregates — `rowNumber` inside `rowNumber` is
rejected, with the advice to compute the inner one in an earlier stage. But
unlike aggregates, a window may be *composed*: `rowNumber spec + 1` is fine.

---

## 11. Set operations and `distinct`

Set operations combine two queries that have **the same row type**:

```haskell
everyone = users &| vips        # union
both     = users &^ vips        # intersect
rest     = users &~ vips        # except
allUsers = users &! vips        # unionAll
```

Each one also has a named function, which is exactly equivalent:

```haskell
everyone = union users vips
both     = intersect users vips
rest     = except users vips
allUsers = unionAll users vips
```

The result keeps the columns and order of the left input, and *both spellings
agree*: `users &~ vips` is `users EXCEPT vips`, the same query as
`except users vips`. Only `except` and `intersect` can tell the two operands
apart — `union` and `unionAll` are commutative, so their order is not
observable.

`union` removes duplicate rows, while `unionAll` preserves them.

`distinct` removes duplicate rows:

```haskell
names = users & select {.name} & distinct
```

`distinct` is a **lowering barrier**: a projection, aggregate, or window that
follows it sees the deduplicated rows rather than folding into the `DISTINCT`,
and `ORDER BY` is always emitted outside it. This matters because SQL would
otherwise change which rows are considered duplicates.

Membership tests use `inList` (also available as `in`):

```haskell
t = users & where (inList [1, 2, 3] .id)
```

```sql
SELECT id, name, age, active FROM public.users WHERE (id IN (1, 2, 3));
```

This is the one place a bracket is a genuine value list: `inList` takes a list of
literals that become the members of a SQL `IN (...)`, rather than sort keys. The
elements are still homogeneous, so `inList [1, 2.0] .id` is a type error.

---

## 12. Functions, types, and the prelude

`prelude.cagara` is imported into every module automatically. It is ordinary
Cagara — you can read it, and it is the best reference for what is available.
It is built on the `sql "..."` and `primitive "..."` **body forms**, which are
the whole of what the Rust core provides:

```haskell
table : string -> string -> query r = primitive "__table"
upper : expr r string -> expr r string = sql "UPPER($1)"
```

A body form is syntax, not a name, so `primitive "__table"` needs no import —
`__table` is the argument the form reads, in the same way `UPPER($1)` is the
text `sql` reads. Nothing about it is in scope.

### Turning the prelude off

A module that wants none of it puts this on the **first line**:

```haskell
# no prelude
table : string -> string -> query r = primitive "__table"
t : query { a = int } = table "s" "t"
```

The implicit import goes; the grammar does not. `&`, `where` and `+` are the
compiler's, so they still parse, while `_&_` and `where` are simply names the
module no longer has — an unknown name, like any other. Write your own, or
import a module that has. The marker must be the whole first line: a leading
space, trailing text, or a later line is an ordinary comment.

Operator *precedence* is not part of this. It is fixed in the compiler, so
turning the prelude off changes which names exist and never how an expression
groups.

### Functions and partial application

Functions are written with `=>`:

```haskell
addOne = x => x + 1
add = a => b => a + b
```

**Arguments come last-in-the-signature, subject-last in usage.** This makes
partial application a reusable transformation:

```haskell
coalesce 0          # a function: turn a maybe int into an int
addDays 7           # a function: shift a date a week later
contains "@"        # a function: test a string
upper >>> trim      # a function: uppercase then trim
```

The combinators:

| Name | Meaning | Fixity |
|---|---|---|
| `f >>> g` | `g` after `f` (composition) | right at level 23, tighter than every other operator |
| `x & f` | apply `f` to `x` | left at level 1, the pipeline |
| `f $ x` | apply `f` to `x` | right at level 0, the loosest operator |

These are Haskell's own fixities: `$` is `infixr 0` and `&` is `infixl 1` from
`Data.Function`, and `>>>` is `Control.Category`'s forward composition, which
in Haskell binds tighter than everything (as `(.)` does). Keeping composition
tight is what makes `trim >>> lower >>> replaceAll "-" ""` read left to right
and `f >>> g $ x` mean `(f >>> g) $ x`. The fixed table gives them these
levels; `prelude.cagara` defines what they mean, like every other operator.

Composition with `>>>` is why subject-last matters:

```haskell
clean = users & select {.id, e = .email & trim >>> lower}
```

Because `&` is the pipeline it sits at level 1, one above `$`, so its right
operand is a whole expression — the same as in Haskell, where `&` is `infixl 1`
and `==` is `infix 4`. Compare after applying by parenthesising:

```haskell
# `.a & toInt` is the applied value; without the parens `&` would swallow the
# comparison into its right operand.
is_five = users & select { ok = (.a & toInt) == 5 }
```

A function name can be bound to another definition, and calling that alias is
the same as calling what it names:

```haskell
up = upper                    # an alias for a template
myCase = caseWhen             # `caseWhen` is itself an alias of `ifThenElse`
s = sum                       # an alias of an overload set
q = users & select { x = up .name }
```

An alias carries overloading with it: `s = sum` still chooses per use, so
`s .amount` and `s .user_id` pick the `float` and `int` candidates respectively,
exactly as `sum` would. Chains work too (`g = up`). A definition whose whole
body is another name has no body of its own to run, so it is the *target* that
provides one — which is why `up`, `s` and `myCase` can be called at all.

### Overloading

A name defined more than once, each with a type signature, is an **overload
set**. The compiler picks the definition whose signature fits each use:

```haskell
_+_ : expr r int -> expr r int -> expr r int
_+_ : expr r float -> expr r float -> expr r float
```

This is how `+` works on both numeric types while still refusing to mix them.

A helper whose overloads stay open keeps them as *holes* in its type, and every
use fills them in — so one helper can be used at several types in the same
query:

```haskell
twice = x => x + x
t = users & select { a = twice .age, b = twice .amount }
```

### Types

A query's type is written `query { ... }`, and `--types` prints it:

```
users  : query { id = int, name = string, age = int, active = bool }
adults : query { id = int, label = string }
```

Expression types are written `expr r a` — an expression over row `r` producing
`a` — plus `agg (expr r a)` for an aggregate and `win (expr r a)` for a window.
You mostly will not write these yourself, but they appear in signatures and in
error messages.

A helper that reads a named column can still have a signature: name the fields
it needs and use `| r` for the rest of the row. A bare `query r` says nothing
about the row's fields, so it cannot be used for a column-reading helper:

```haskell
byId : expr { id = int | r } int -> query { id = int | r } -> query { id = int | r } =
  i => q => where (.id == i) q
```

### The prelude at a glance

**Comparison and logic:** `==` `!=` `<` `<=` `>` `>=` `&&` `||` `not`

**Arithmetic:** `+` `-` `*` `/` `negate` (int and float), `%` (int only), `<>` / `concat`

**Strings:** `upper` `lower` `trim` `ltrim` `rtrim` `length` `like` `ilike`
`substring start len s` `left n s` `right n s` `lpad n fill s` `rpad n fill s`
`replaceAll from to s` `strpos sub s` `contains sub s` `startsWith prefix s`
`endsWith suffix s`

**Dates and timestamps:** `currentDate` `yesterday` `now` `toDate` `toTimestamp` `toString`
`addDays` `addWeeks` `addMonths` `addQuarters` `addYears` `addHours`
`addMinutes` `addSeconds`; `truncYear` `truncQuarter` `truncMonth` `truncWeek`
`truncDay` `truncHour` `truncMinute`; `year` `quarter` `month` `day`
`dayOfWeek` `dayOfYear` `hour` `minute`; `daysBetween start end`

**Math:** `abs` `round` `roundTo digits x` `floor` `ceil` `sqrt`
`power exponent base` `sign` `greatest a b` `least a b`

**Aggregates:** `count` `countOf` `countDistinct` `sum` `avg` `min` `max`;
`sumFilter` `avgFilter` `minFilter` `maxFilter` `countFilter` `countOfFilter`
`countDistinctFilter`; nullable inputs use `sumMaybe` `avgMaybe` `minMaybe`
`maxMaybe` `countOfMaybe` `countDistinctMaybe`

**Nulls:** `nothing` `coalesce` `??` `just` `nullIf` `isNull` `isNotNull` `isTrue`
`whereTrue` `eqMaybe`

**Casts:** `toInt` `toFloat` `toBool` `toString` `toDecimal` `tryInt` `tryFloat` `tryDate` `tryTimestamp` `tryDecimal` (`from*` spellings: `fromString`, `fromInt`, `fromFloat`)

**Windows:** `rowNumber` `rank` `denseRank` `lag` `lead` `sumOver` `avgOver`
`countOver` `firstValue` `lastValue` `ntile spec n` `percentRank` `cumeDist`

**Conditionals:** `ifThenElse` (alias `caseWhen`), `case { cond => value; ...; true => default }`

Positions are 1-based, as in SQL. `addWeeks` and `addQuarters` are defined in
Cagara as 7 days and 3 months, so the date units the prelude spells are `DAY`,
`MONTH` and `YEAR` (with `HOUR`/`MINUTE`/`SECOND` for timestamps), plus
`WEEK`/`QUARTER` for the `trunc*` helpers; every one of them is a `CAGARA_*`
intrinsic, not raw SQL.
String literals widen to `date` and `timestamp`, so comparisons read naturally:

```haskell
recent = orders & where (.created_at >= "2024-01-01")
```

The next section is a working reference for the string and date halves of that
list: argument order, which types each function accepts, and the traps the
signatures alone do not tell you about.

---

## 13. String and date functions in practice

The lists in [section 12](#12-functions-types-and-the-prelude) say what exists.
This section says how to *use* it. All of these examples share one table:

```haskell
orders : query { id = int, email = string, name = string, amount = float, status = string, created_at = timestamp, due = date } =
  table "public" "orders"
```

### Strings

Every string function takes the string it operates on **last**, which is what
makes partial application work. The argument order is therefore:

| Call | Meaning | Emitted |
|---|---|---|
| `upper s`, `lower s` | case conversion | `UPPER(s)` |
| `trim s`, `ltrim s`, `rtrim s` | strip whitespace | `TRIM(s)` |
| `length s` | number of characters, as `int` | `LENGTH(s)` |
| `substring start len s` | `len` characters from `start` | `SUBSTRING(s, start, len)` |
| `left n s`, `right n s` | first / last `n` characters | `LEFT(s, n)` |
| `lpad n fill s`, `rpad n fill s` | pad to width `n` with copies of `fill` | `LPAD(s, n, fill)` |
| `replaceAll from to s` | every occurrence of `from` | `REPLACE(s, from, to)` |
| `strpos sub s` | 1-based position of `sub`, `0` if absent | `STRPOS(s, sub)` |
| `contains sub s` | is `sub` in `s`? | `STRPOS(s, sub) > 0` |
| `startsWith prefix s`, `endsWith suffix s` | prefix / suffix test | `LEFT` / `RIGHT` comparison |
| `like pattern s`, `ilike pattern s` | `LIKE` / case-insensitive `LIKE` | `s LIKE pattern` |
| `concat a b` / `a <> b` | concatenation | `a \|\| b` |

The order is the reverse of the SQL spelling, deliberately: `substring 1 3 .s`
reads "three characters from position 1 of `.s`", and because the subject is
last, `substring 1 3` is itself a reusable function.

```haskell
clean = orders & select {
  .id,
  slug = replaceAll " " "-" (trim .name),
  initial = left 1 .name,
  tail = right 3 .name,
  n = length .name,
  local = left (strpos "@" .email - 1) .email,
  domain = lower (substring (strpos "@" .email + 1) 99 .email)
}
```

```sql
SELECT id, REPLACE(TRIM(name), ' ', '-') AS slug, LEFT(name, 1) AS initial,
       RIGHT(name, 3) AS tail, LENGTH(name) AS n,
       LEFT(email, (STRPOS(email, '@') - 1)) AS local,
       LOWER(SUBSTRING(email, (STRPOS(email, '@') + 1), 99)) AS domain
FROM public.orders;
```

Splitting an email on `@` is the canonical example because it shows the pattern
you will reuse constantly: `strpos` gives a 1-based position, arithmetic on it is
ordinary `int` arithmetic, and `substring` is a normal function that takes the
result. There is no dedicated `split`; a long `len` is the usual way to say "to
the end of the string".

#### `contains` is not `like`

These two are easy to reach for interchangeably, and they are not the same:

```haskell
a = orders & select { .id, has_at = contains "@" .email, plain = like "a%b" .name }
```

```sql
SELECT id, STRPOS(email, '@') > 0 AS has_at, name LIKE 'a%b' AS plain FROM public.orders;
```

`like` takes a **pattern**, where `%` matches any run of characters and `_`
matches exactly one. `contains`, `startsWith`, and `endsWith` are plain substring
tests built on `strpos`, so `%` and `_` are literal characters there — which is
what you want when testing for text a user typed:

```haskell
# Finds a literal percent sign; like "%100%%" would need escaping.
a = orders & where (contains "100%" .status)
```

Note the argument order on `like` too: **the pattern comes first** —
`like "paid%" .status` — because `.status` is the subject. And `strpos` returns
`0` for "not found", never `NULL` and never a negative number, so on a miss
`strpos "@" .email - 1` is `-1` rather than a null; guard with `contains` first
when a miss is possible.

#### Positions and counts are 1-based

`substring`, `left`, `right`, and `strpos` all count from 1, as in SQL. A `start`
of `0` or a negative `len` is passed through to the engine rather than clamped,
so the prelude does not hide the dialect's behaviour there — keep positions and
lengths positive. `length` counts characters, and trailing spaces count, since it
is a character count rather than a trimmed one.

### Dates and timestamps

Two types are involved, and the prelude is strict about which function accepts
which. A `date` is a calendar day; a `timestamp` is a point in time. String
literals widen to *either*, depending on what the surrounding context expects.

| Function | `date` | `timestamp` | Result |
|---|---|---|---|
| `currentDate`, `yesterday` (no argument) | — | — | `date` |
| `now` / `currentTimestamp` (no argument) | — | — | `timestamp` |
| `year`, `quarter`, `month`, `day`, `dayOfWeek`, `dayOfYear` | ✓ | ✓ | `int` |
| `hour`, `minute` | ✗ | ✓ | `int` |
| `truncYear`, `truncQuarter`, `truncMonth`, `truncWeek` | ✓ | ✓ | same type in |
| `truncDay`, `truncHour`, `truncMinute` | ✗ | ✓ | `timestamp` |
| `addDays`, `addWeeks`, `addMonths`, `addQuarters`, `addYears` | ✓ | ✓ | same type in |
| `addHours`, `addMinutes`, `addSeconds` | ✗ | ✓ | `timestamp` |
| `daysBetween start end` | ✓ | — | `int` |
| `toDate` / `toTimestamp` | converts | converts | see below |
| `toString` | ✓ | ✓ | `string` |

The asymmetry is the point: a calendar day has no hour, and adding two hours to a
`date` has no answer that is still a `date`. Asking anyway is a type error that
names the fix:

```haskell
bad = orders & select {.id, h = hour .due}
```

```
error: field `due`: type mismatch: expected timestamp, found date
```

Convert first with `toTimestamp` when you genuinely want the time:

```haskell
t = orders & select {.id, h = hour (toTimestamp .due)}
```

#### Widening: what a string literal can and cannot become

A string literal widens to `date` or `timestamp` where one is expected, which is
why comparisons read naturally:

```haskell
recent = orders & where (.created_at >= "2024-01-01" && .created_at < "2025-01-01")
```

But widening is driven by the **expected type**, and a bare literal passed to an
overloaded function gives the checker nothing to go on. That split explains two
results that look inconsistent until you see the rule:

```haskell
ok   = orders & select { x = daysBetween "2024-01-01" "2024-02-01" }  # both widen to date
ok2  = orders & select { x = .due > "2024-01-01" }                    # widens for the comparison
bad  = orders & select { x = addDays 7 "2024-01-01" }                 # error: string, not date
bad2 = orders & select { x = year "2024-01-01" }                      # error: string, not date
```

`daysBetween` has a single overload whose parameter is `date`, so the literal
widens. `addDays` and `year` are each overloaded across `date` and `timestamp`,
so a bare literal is ambiguous and stays a `string`. The fix is to say which one
you mean, with `toDate` / `toTimestamp`:

```haskell
ok = orders & select { x = addDays 7 (toDate "2024-01-01") }
```

```sql
SELECT CAST((DATE '2024-01-01' + 7 * INTERVAL '1' DAY) AS DATE) AS x FROM public.orders;
```

#### `toDate` / `toTimestamp` / `toString` / `toBool`

Conversions are explicit, in both directions:

| Call | From | To |
|---|---|---|
| `toDate x` | `timestamp` or `string` | `date` |
| `toTimestamp x` | `date` or `string` | `timestamp` |
| `toString x` | `int`, `float`, `date`, or `timestamp` | `string` |
| `toBool x` | `int` or `float` | `bool` |

```haskell
t = orders & select {
  .id,
  day = toDate .created_at,
  midnight = toTimestamp .due,
  stamped = toTimestamp "2024-01-01 08:30:00",
  label = toString .due,
  ts_label = toString .created_at
}
```

```sql
SELECT id, CAST(created_at AS DATE) AS day, CAST(due AS TIMESTAMP) AS midnight,
       TIMESTAMP '2024-01-01 08:30:00' AS stamped,
       CAST(due AS TEXT) AS label, CAST(created_at AS TEXT) AS ts_label
FROM public.orders;
```

Note that `toString` on a `date` and on a `timestamp` both render the value's
text form directly; wrapping one in the other first is only about *which* form
you want, not about what the cast accepts. If you want the date part of a
timestamp as text, `toString (toDate .created_at)` still says that explicitly.

#### Adding, truncating, and bucketing

`add*` functions shift a value by a signed count — negative goes back — and
`trunc*` functions return the start of the containing period:

```haskell
t = orders & select {
  .id,
  next_month = addMonths 1 .created_at,
  back_30 = addDays (-30) .created_at,
  month_start = truncMonth .created_at,
  week_start = truncWeek .created_at,
  q = quarter .created_at,
  dow = dayOfWeek .created_at,
  overdue = .due < currentDate,
  age_days = daysBetween .due currentDate
}
```

```sql
SELECT id, CAST((created_at + 1 * INTERVAL '1' MONTH) AS TIMESTAMP) AS next_month,
       CAST((created_at + (-30) * INTERVAL '1' DAY) AS TIMESTAMP) AS back_30,
       CAST(DATE_TRUNC('MONTH', created_at) AS TIMESTAMP) AS month_start,
       CAST(DATE_TRUNC('WEEK', created_at) AS TIMESTAMP) AS week_start,
       CAST(EXTRACT(QUARTER FROM created_at) AS INT) AS q,
       CAST(EXTRACT(DOW FROM created_at) AS INT) AS dow,
       due < CURRENT_DATE AS overdue, (CURRENT_DATE - due) AS age_days
FROM public.orders;
```

Three details worth remembering:

- **`dayOfWeek` is 0 for Sunday**, matching Postgres' `DOW` rather than ISO.
  **`truncWeek` starts the week on Monday**, matching ISO. Those two conventions
  differ from each other, so check which one a given report needs.
- **`daysBetween start end`** is positive when `end` is later, and its argument
  order follows subject-last like everything else, so `daysBetween .created_at`
  is a function of the end date. It takes `date`, not `timestamp`; use `toDate`
  on a timestamp first.
- **`addWeeks` and `addQuarters` are defined in Cagara** as 7 days and 3 months,
  not as dialect primitives — visible in the generated SQL as the
  `INTERVAL '1' DAY` / `MONTH` spellings. `addHours`, `addMinutes`, and
  `addSeconds` exist only for `timestamp`.

Truncation is what makes date bucketing work, because the result is still a date
you can group and sort by:

```haskell
monthly = orders & agg {
  month_start = group (truncMonth .created_at),
  orders = count,
  revenue = sum .amount
}
```

```sql
SELECT CAST(DATE_TRUNC('MONTH', created_at) AS TIMESTAMP) AS month_start,
       COUNT(*) AS orders, SUM(amount) AS revenue
FROM public.orders GROUP BY CAST(DATE_TRUNC('MONTH', created_at) AS TIMESTAMP);
```

Use `group` on the truncated expression rather than grouping the raw timestamp:
`group .created_at` would give one row per instant, not one per month.

### Partial application and composition

Because the subject is last, these functions are all reusable transformations,
which is the idiomatic way to name a cleaning step once:

```haskell
normalize = trim >>> lower              # a function of a string
clean = orders & select {.id, email = .email & normalize}
half = substring 1 2                    # a function of a string
week_ago = addDays (-7)                 # a function of a date
```

Only `clean` is a query, so it is the only one that emits SQL:

```sql
SELECT id, LOWER(TRIM(email)) AS email FROM public.orders;
```

### Naming a reusable stage

Because a stage is just a function from a query to a query, a definition that
is *any* such function can be used as a stage. The body may apply a single
stage, or thread the query through several:

```haskell
no_id  = omit "id"                        # point-free: the body is the stage
big    = q => where (.age > 18) q         # a lambda naming its parameter
adults = q => where (.name != "") (where (.age > 18) q)   # stages in sequence

a = users & no_id
b = users & big
```

A stage's own parameters come **before** the query, following subject-last, so
a helper may be partly configured at the use:

```haskell
byAge = n => q => where (.age > n) q
grown = users & byAge 21
```

An inline lambda works the same way, wherever a stage is expected:

```haskell
users & (q => where (.age > 18) q)
```

Because a stage is any function from a query to a query, stages also **compose**
with `>>>` (and its mirror `<<<`) like any other function:

```haskell
public_ids = select {.id} >>> where (.id > 1)
t = users & public_ids
u = users & (select {.id} >>> where (.id > 1) >>> select {.id})   # chains
```

`>>>` binds tighter than everything, so a composed stage is one operand of `&`.
`f >>> g` applies `f` first, then `g`; `f <<< g` applies `g` first. A
composition may be given a name and used as a stage, composed again, or passed
to a helper — it is an ordinary value, so nothing special applies to it.

### Why these are not plain SQL

Every date operation above, and the string operations whose spelling differs
across engines (`length`, `left`/`right`, `strpos`, `contains`, `startsWith`,
`endsWith`), call a `CAGARA_*` intrinsic rather than spelling ANSI SQL directly —
`upper`, `lower`, `trim`, `ltrim`, `rtrim`, `lpad`, `rpad`, `substring`,
`replaceAll`, `like` and `ilike` are ordinary SQL templates — so that
[`crates/cagara-sql/src/intrinsics.rs`](../crates/cagara-sql/src/intrinsics.rs)
can emit the right thing per dialect. The same expression therefore lowers
differently depending on `--dialect`:

```haskell
t = orders & select {.id, y = year .created_at, d = dayOfWeek .created_at}
```

```sql
-- ansi / postgres
CAST(EXTRACT(YEAR FROM created_at) AS INT), CAST(EXTRACT(DOW FROM created_at) AS INT)

-- mysql: DAYOFWEEK is 1-based from Sunday, so 1 is subtracted to keep 0 = Sunday
EXTRACT(YEAR FROM created_at), (DAYOFWEEK(created_at) - 1)
```

Note that MySQL adjustment specifically: it exists so that `dayOfWeek` means the
same thing on every engine. This is the reason to check the
[dialect reference](SQL-DIALECTS.md) before trusting a lightly tested engine —
the *semantics* are supposed to be constant across dialects, and where a dialect
needed adjusting, that document says so. `ilike` is another example: SQLite has
no `ILIKE`, so it lowers to `LOWER(s) LIKE LOWER(pattern)`.

---

## 14. Writing your own SQL with `sql` templates

When the prelude does not have what you need, a `sql` template drops down to
SQL text with `$1`, `$2`, ... placeholders:

```haskell
myUpper : expr r string -> expr r string = sql "UPPER($1)"
```

**The type signature is required.** It gives the template its arity (from the
arrows) and its phase (from the result head, `expr` / `agg` / `win`):

```
error: `bad` needs a type signature: a `sql` template takes its arity and phase
       from it, e.g. `bad : expr r string -> expr r string = sql "UPPER($1)"`
```

Placeholders are validated at the definition: they must be exactly `$1` through
`$n`, each standing alone. `$0`, a gap such as `$1` and `$3`, a bare `$`, and a
`$n` glued to a name are all reported there rather than reaching the backend.

Templates are parsed and rewritten for the target dialect, so dialect-specific
spellings do not survive intact. For that reason the prelude's date and string
operations call `CAGARA_*` intrinsics that the backend spells per dialect; an
unrecognized intrinsic is a compile error rather than SQL passed through. See
[SQL dialect support](SQL-DIALECTS.md) for the full explanation.

---

## 15. Modules

Imports are relative to the importing file. The prelude is always available.

```haskell
import "schema.cagara" as schema

public_users = schema.users & select {.id, .name}
```

`as` is optional and decides how the module's definitions are reached. With an
alias they stay behind it, and the bare name is not in scope:

```haskell
import "sch.cagara" as s
x = users & select {.id}     # error: unknown name `users`
y = s.users & select {.id}   # the aliased spelling
```

Without one, the module's definitions join the importing module's scope
directly, so `import "sch.cagara"` also lets you write `users`. That is
convenient and easy to misread: a later import with a colliding name shadows an
earlier one, and the module's own definitions shadow both. Prefer the alias when
a module contributes more than one name.

Relative paths resolve against the importing file, so a module can be imported
from anywhere. Cycles and duplicate definitions are reported.

Modules are also how you share table declarations:

```haskell
# schema.cagara
users : query { id = int, name = string, age = int, active = bool, password_hash = string } =
  table "public" "users"

# public.cagara
import "schema.cagara" as s
public_users = s.users & select { .id, display_name = .name }
```

---

## 16. What the compiler emits

You do not write subqueries, but the compiler does, and knowing when helps you
read the output.

**Stages fuse** into one `SELECT` when that is safe. Filters, projections, and
sorts on the same source become clauses of one query:

```haskell
t = orders & where (.status == "paid") & select {.id, .amount} & order [asc .id]
```

```sql
SELECT id, amount FROM public.orders WHERE (status = 'paid') ORDER BY id NULLS LAST;
```

**A filter over an aggregate wraps a derived table.** The predicate is written
against the aggregate outputs, so the grouped `SELECT` is lowered into a
subquery and the filter sits outside it:

```haskell
t = orders & agg { user_id = group .user_id, total = coalesce 0.0 (sum .amount) } & where (.total > 100.0)
```

```sql
SELECT user_id, total
FROM (SELECT user_id, COALESCE(SUM(amount), 0.0) AS total
      FROM public.orders GROUP BY user_id) AS t1
WHERE (total > 100.0);
```

Folding this into a `HAVING` on the aggregate is a query-shape optimization this
compiler does not currently perform; see [`PLAN.md`](PLAN.md).

**A derived table appears** after a window, after `limit`/`offset`, after a
filter over an aggregate, and after an aggregate when a later stage still has to
read the grouped rows — a second `agg`, or a filter that runs after an
intervening `order` or `limit`:

```haskell
u = orders & select {.id, rn = rowNumber { order = [asc .id] }} & where (.rn <= 3)
v = orders & agg { u = group .user_id, n = count } & order [desc .n] & where (.n > 5)
```

```sql
SELECT id, rn FROM (SELECT id, ROW_NUMBER() OVER (ORDER BY id NULLS LAST) AS rn FROM public.orders) AS t1 WHERE (rn <= 3);
SELECT u, n FROM (SELECT user_id AS u, COUNT(*) AS n FROM public.orders GROUP BY user_id) AS t1 WHERE (n > 5) ORDER BY n DESC NULLS LAST;
```

**Order is preserved across derived tables.** SQL does not guarantee a derived
table's order, so when a stage is wrapped the `ORDER BY` moves to the outer
query (the inner one keeps it only if a `LIMIT` needs it). A sort key that is
not an output column is carried out as a hidden column.

**Join inputs are inlined** when they only project or filter a table, and only
for `innerJoin`: a filter under an inner join is hoisted into the query's `WHERE`
(`... INNER JOIN public.orders AS t2 ON t1.id = t2.user_id WHERE (t2.status =
'paid')`). An outer join's inputs are never inlined — the nullable side keeps a
derived table (`LEFT JOIN (SELECT … WHERE …) AS t2 ON …`), because moving that
filter into `ON` or into the outer `WHERE` would change which rows are
null-extended.

**Repeated relational subtrees become CTEs** automatically.

`--optimize` additionally runs sqlglot's optimizer (constant folding, boolean
simplification, pushdown). It is opt-in, and it preserves filters outside window
and `LIMIT` boundaries.

---

## 17. Command line

```
cagara <file.cagara> [--dialect NAME] [--only DEF] [--pretty] [--optimize] [--types]
cagara fmt [--check] <files...>
cagara doc [<files...|->] [--json] | --prelude | --check
cagara lsp
```

| Flag | Effect |
|---|---|
| `--dialect NAME` | target a SQL dialect (`postgres`, `mysql`, `sqlite`, `duckdb`, `tsql`, `bigquery`, `snowflake`, `trino`, `spark`, ...) |
| `--only DEF` | compile the named definition instead of `main` |
| `--types` | print inferred types instead of SQL |
| `--pretty` | pretty-print the generated SQL |
| `--optimize` | run the SQL optimizer |

A file is compiled for its **entry point**, the definition named `main`:
that is the one statement printed, so helpers do not bury the answer, and a
module of table declarations does not dump its whole schema. `main` must be a
query; a scalar definition named `main` is an error rather than a silent
no-output success. `--only DEF` reads a different definition of any type, which
is how a helper is inspected.

A file with no `main` is a library: it prints nothing and exits 0, because
compiling it succeeded. It is still checked as a whole — a broken helper in a
library is still a broken file, and is reported with a non-zero exit even
though nothing was printed.

Diagnostics are printed as `file:line:col` with the offending source line
underlined, and the process exits non-zero when there are errors.

Dialect support varies in *depth*, and the distinction matters. Some dialects
have Cagara's date, string, and integer-arithmetic operations rewritten and are
executed against real engines in CI (SQLite, DuckDB); others can be targeted but
their intrinsic spellings use the ANSI/Postgres fallback. Comparing `int`
division is instructive:

```sql
-- ansi / postgres / sqlite
(amount / 2)
-- mysql: exact integer division, and NULLs-last emulated with a CASE key
CAST(((amount - (amount % 2)) / 2) AS SIGNED)
-- duckdb
DIVIDE(amount, 2)
-- bigquery
DIV(amount, 2)
```

Check the [dialect reference](SQL-DIALECTS.md) before trusting an untested
engine, especially around dates, string functions, integer division, and null
ordering.

`cagara fmt` formats source in place (`-` reads stdin and writes stdout).
`--check` reports whether files are already formatted without rewriting them,
and an unparseable file is left alone.

`cagara doc` prints the documentation a file carries. Given a file whose
definitions are documented like this:

    #! The reporting file.
    ## Adults only, with `name` relabelled for display.
    adults = users & where (.age >= 18) & select { id = .id, label = upper .name }

`cagara doc q.cagara` prints:

    # q.cagara

    The reporting file.

    ## `adults`

    ```text
    adults : query { id = int, label = string }
    ```

    Adults only, with `name` relabelled for display.

The signature is the inferred one, so documenting a definition cannot disagree
with the checker, and an undocumented one still shows its type. A file's
definitions are grouped by name, so an overload set is one section with one
explanation and every signature.

`cagara doc --check` lists the top-level definitions that have no `##` block and
exits **0** — a missing doc comment is not a compile error, and this is the
report that says what is left to write rather than a gate that stops a build.
`--json` prints the same content for a tool, `--prelude` documents the built-in
prelude, and `-` reads stdin.

The language server, `cagara lsp`, provides diagnostics, hover with inferred
types, go-to-definition, find-references, document symbols, and completion
(including column completion after `.`, `.<`, and `.>`). Hovering a column shows
the expression type it has there — `expr r string`, or `expr (join r s) int` for
a side-qualified one — because that is the shape a parameter of type `expr r a`
accepts; `r` stands for the input row of the enclosing stage. Editor
integrations are in [`editors/vscode`](../editors/vscode) and
[`editors/nvim`](../editors/nvim).

---

## 18. Errors you will meet

Cagara's diagnostics are a feature: they name the column, the fix, or both. The
common ones, with the fix:

| Message | Meaning and fix |
|---|---|
| `no column \`x\`; available: ...` | typo or wrong stage; the list is the actual row |
| `uses a column that is not grouped` | add `group` or aggregate it |
| `aggregates cannot nest` | use an earlier `agg` stage |
| `is an aggregate; aggregates belong in \`agg\`, not \`select\`` | move it into `agg` |
| `this argument contains a window function; windows cannot nest` | `select` the window, then filter the new column |
| `type mismatch: expected int, found float` | convert explicitly with `toFloat` / `toInt` |
| `only one side is nullable` | wrap with `coalesce`, or use `just` |
| `expected a non-null value, found a \`maybe\`` | handle it; for an aggregate input use the `*Maybe` helper |
| `a line starting with an identifier in column 0` | indent the continuation, or terminate the previous item with `;` |
| `\`main\` must be a query` | make `main` a query, or omit it for a library |
| `helper does not match its signature` | use a field-tail row: `query { id = int \| r }` |
| `join predicates must say which input a column comes from` | use `.<x` / `.>x` |
| `` `.<x` and `.>x` ... only be used in a join predicate `` | refer to `.x` on the joined row instead |
| `sort and partition keys must be plain column expressions` | compute it in an earlier stage |
| `integer literal is too large` | the limit is `9223372036854775807` |
| `needs a type signature` | a `sql` template needs an annotation |

Two habits prevent most of the rest: give every table an accurate annotation,
and when something will not type-check, ask *which phase* the value belongs to
(row, aggregate, or window) — almost every phase error is answered by moving
the computation one stage earlier.

---

## 19. Style guide

- Put one stage per line in a pipeline. `cagara fmt` will do this for you.
- Prefer `select` when you are deciding what to publish and `update` when you
  are recomputing or renaming in place.
- Keep table declarations in their own module (`schema.cagara`) and import them.
  An annotation written once cannot drift out of sync with itself.
- Name intermediate stages. A window that is filtered later deserves its own
  definition (`ranked`, then `top3`), for the same reason the compiler requires
  it.
- Reach for the `&?` / `&=` / `&+` shorthands only once a pipeline is long enough
  that the punctuation is clearer than the words. Spelling out `update` is worth
  it when the row merge is the point of the stage.
- Annotate helper functions' types when they are not obvious; signatures are
  what make overloading work predictably.

---

## 20. Where to go next

- [`examples/report.cagara`](../examples/report.cagara) — filtering, aggregation,
  windows, joins, nulls, and shorthands in one file; compiled and executed
  against SQLite and DuckDB by the test suite.
- [`examples/public.cagara`](../examples/public.cagara) and
  [`schema.cagara`](../examples/schema.cagara) — modules.
- [`examples/errors.cagara`](../examples/errors.cagara) — a file of
  deliberate mistakes; run it to see the diagnostics.
- [Nullability reference](NULLABILITY.md) — outer joins and nullable aggregates
  in depth.
- [SQL dialect support](SQL-DIALECTS.md) — what each target guarantees, and why
  the date and string intrinsics are spelled in Rust.
- [`prelude.cagara`](../prelude.cagara) — the whole standard library, in
  Cagara, with comments.
- [`docs/PLAN.md`](PLAN.md) — design and architecture, including the type
  system and the lowering rules.
