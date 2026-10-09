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
and run to the end of the line.

```haskell
# A definition is `name = expression`, optionally with a type annotation.
users : query { id = int, name = string, age = int, active = bool } =
  table "public" "users"

adults = users
  & where (.age >= 18 && .active)
  & select { id = .id, name = .name }
  & order [asc .name]
  & limit 10
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
left-associative and has the lowest precedence, so a pipeline reads top to
bottom:

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

A shorthand is not built into the compiler. It is two lines in
`prelude.cagara` — one to *declare* the symbol, one to *define* what it means:

```haskell
infixl 1 &^                  # declare: same level as `&`, so a stage
_&^_ = q => n => offset n q    # define: `q &^ n` is `offset n q`
```

The parser reads the declarations to find out what an operator's precedence
is, and the desugared call `_&^_ q n` is an ordinary prelude definition like
any other. So adding a shorthand to the language is those two lines and
nothing else: the formatter, the error messages and the editor grammars all
follow from the declaration.

There is no special form for a stage. `&`, `&+` and `+` are the same kind of
thing — an infix operator with a precedence and an associativity — and one
declaration form covers all of them:

```haskell
infixl 1 &+     # left-associative, precedence 1 (same as `&`)
infixr 21 ??    # right-associative, precedence 21
infixl 3 ?      # the join operators, at their own level
```

What makes a shorthand a *pipeline stage* is only where it sits: an operator
declared at `&`'s precedence and associativity is a stage, and one at any
other level — the joins at 3, or `+` at 17 — is an ordinary operator. That is
why `q &+ {..}` chains like `q &` while `q ? on` does not, and it is the only
reason: nothing has to mark an operator as a stage, so nothing can disagree
with the level.

Precedence is a number from 1 (loosest) to 250. A symbol may be any operator
spelling — `&?`, `~=`, `>>` — and declaring one that already exists changes
its fixity rather than adding a second operator. Any operator you write must
be declared, so a typo is a compile error pointing at the operator rather than
a silently different parse:

```haskell
bad = users &~ 5   # unknown operator `&~`; declare it in `prelude.cagara`
```

Declarations are read from the prelude only, and may not appear in your own
files: an operator's precedence is a property of the language, not of one
module. The two words `infixl` and `infixr` are reserved where a definition
may start.

Note the spelling boundary. A shorthand is `&` followed by operator
punctuation, and an ordinary operator is a run of operator characters, so both
follow from a character class rather than a list of names. Those classes are
`&` plus any of `= ! < > * / % | ? $ ^ ~ + - .`, and `= ! < > * / % | ? $ ^ ~`
on their own. A shorthand built from characters already in the classes is free
— `&^`, `&>>`, `~=` all work the moment they are declared. A character never
used in an operator before needs one line in the lexer, after which every
spelling made of those characters is free too.

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

### Operator precedence

From tightest to loosest. The levels are the ones the operators are *declared*
with in `prelude.cagara`; a larger number binds tighter, and application
(`f x y`) is tighter than any operator.

| Level | Operators | Associativity |
|---|---|---|
| — | application (`f x y`) | left |
| 23 | `>>>` (composition) | right |
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
its declaration says. Arithmetic, comparison, logic and the concatenation
operators are the language's own and are fixed in the compiler; everything
else — the pipeline, the joins, and the three combinators — is declared in the
prelude (see [Shorthands](#shorthands)). The gaps in the numbering leave room
for the operators Cagara has that Haskell does not.

Two associativities differ from Haskell on purpose, both harmless: `&&` and
`||` are `infixl` where Haskell writes `infixr` (both are associative, so the
parse tree is the only difference), and the comparisons are `infixl` where
Haskell writes `infix 4` (non-associative). Cagara's comparisons could be made
non-associative too, but that needs a third declaration keyword, `infix`, and
`a < b < c` is already rejected as a type error.

`/` on `int` truncates toward zero and `%` keeps the dividend's sign in **every**
dialect — the compiler emits a dialect-appropriate expression rather than
relying on whatever `/` means locally.

### Arithmetic is not overloaded across types

`+ - * / %` work on `int` with `int`, or `float` with `float`. Mixing is an
error, and the fix is an explicit cast: `toFloat .user_id`, `toInt .amount`.

### Conditionals

`ifThenElse` lowers to a searched `CASE` expression. Both branches must have the
same type.

```haskell
size = orders & select { id = .id, bucket = ifThenElse (.amount > 100.0) "big" "small" }
```

```sql
SELECT id, CASE WHEN (amount > 100.0) THEN 'big' ELSE 'small' END AS bucket FROM public.orders;
```

### Casts

Casts are named by their **result** type: `toInt`, `toFloat`, `toBool`,
`toString`. There is no implicit conversion between ordinary expressions.

### Identifiers are quoted when needed

A column name that is not a lowercase word, or that collides with a reserved
word, is quoted in the generated SQL using the dialect's quotes:

```sql
SELECT id, amount > 100.0 AND status = 'paid' AS "both" FROM public.orders;
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

`a` is `users` without `id`; `b` is `orders` without `id`. The key is a literal
so that the column can be identified when the query is compiled, which also
means a key cannot be passed in as a parameter.

### Rewriting names: `prefix` and `suffix`

`prefix "text"` and `suffix "text"` apply a rewrite to **every** column name:

```haskell
prefixed = users & prefix "u_"               # id -> u_id, name -> u_name, ...
suffixed = users & suffix "_v2"              # id -> id_v2, ...
```

```sql
SELECT id AS u_id, name AS u_name, age AS u_age, active AS u_active FROM public.users;
```

A name rewrite keeps field order and types; only labels change.

Two names colliding is an error. Dropping is `omit`'s job:

```
error: the key map would produce column `name` twice
```

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

But after `agg`, a `where` on the aggregated row becomes `HAVING` in the same
`SELECT`. The predicate names an aggregate output, and the compiler inlines the
aggregate expression behind that name (SQL does not let you use the output name
there):

```haskell
b = orders & agg { n = count } & where (.n > 5)
```

```sql
SELECT COUNT(*) AS n FROM public.orders HAVING (COUNT(*) > 5);
```

After `limit` — or when an `order` or `limit` sits between the `agg` and the
`where` — it has to become an outer query, because SQL cannot filter on a
limited result in the same `SELECT`:

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
non-null `int`. See the next section.

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
| `sum`, `avg`, `min`, `max` | `maybe` |
| `lag`, `lead`, `sumOver`, `avgOver` | `maybe` |
| `count`, `countOf`, `countDistinct`, `countOver` | non-null `int` |
| `isNull`, `isNotNull`, `isTrue` | non-null `bool` |

Handling nulls explicitly:

| Function | Meaning |
|---|---|
| `coalesce default x` | use `default` if `x` is `NULL` |
| `x ?? default` | the same, infix (and it binds tighter than arithmetic) |
| `just x` | mark a non-null value as nullable |
| `nullIf x y` | return `NULL` when `x` equals `y` |
| `isNull x` / `isNotNull x` | test for `NULL`, returning non-null `bool` |
| `isTrue x` | treat a nullable boolean `NULL` as false |
| `whereTrue p` | filter with a nullable predicate, treating `NULL` as false |
| `eqMaybe x y` | compare nullable values, treating two `NULL`s as equal |

The payoff is that the compiler refuses to let a `NULL` slip into a computation
where it would silently poison the result:

```haskell
u : query { id = int } = table "public" "users"
o : query { user_id = int, amount = float } = table "public" "orders"

t = u & leftJoin o (.<id == .>user_id) & select {.id, amt = .amount + 1.0}
```

```
error: field `amount`: type mismatch: expected maybe float, found float;
       only one side is nullable: `coalesce default x` takes a `maybe`, ...
```

The fix is to say what a missing amount means:

```haskell
t = u & leftJoin o (.<id == .>user_id) & select {.id, amt = coalesce 0.0 .amount + 1.0}
```

`countOf` follows the same non-null rule even though SQL's `COUNT(x)` skips
nulls. To count *matched* rows across a left join, sum an indicator over the
null-extended side instead — this also excludes the synthetic row SQL creates
for an unmatched row:

```haskell
matches = users
  & leftJoin orders (.<id == .>user_id)
  & agg { id = group .id, n = coalesce 0 (sum (ifThenElse (isNotNull .user_id) 1 0)) }
```

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
evaluated before rows exist. These are rejected, each with a targeted message:

```haskell
bad1 = orders & where ((rowNumber {order = [desc .amount]}) <= 3)
# `where` cannot filter on a window function; `select` it first, then filter the new column

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
SELECT id, name, active FROM public.users WHERE (id IN (1, 2, 3));
```

This is the one place a bracket is a genuine value list: `inList` takes a list of
literals that become the members of a SQL `IN (...)`, rather than sort keys. The
elements are still homogeneous, so `inList [1, 2.0] .id` is a type error.

---

## 12. Functions, types, and the prelude

`prelude.cagara` is imported into every module automatically. It is ordinary
Cagara built on a handful of `__` primitives from the Rust core — you can read
it, and it is the best reference for what is available.

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
| `f >>> g` | `g` after `f` (composition) | `infixr 23`, tighter than every other operator |
| `x & f` | apply `f` to `x` | `infixl 1`, the pipeline |
| `f $ x` | apply `f` to `x` | `infixr 0`, the loosest operator |

These are Haskell's own fixities: `$` is `infixr 0` and `&` is `infixl 1` from
`Data.Function`, and `>>>` is `Control.Category`'s forward composition, which
in Haskell binds tighter than everything (as `(.)` does). Keeping composition
tight is what makes `trim >>> lower >>> replaceAll "-" ""` read left to right
and `f >>> g $ x` mean `(f >>> g) $ x`. They are declared in `prelude.cagara`
next to their definitions, like every other operator.

Composition with `>>>` is why subject-last matters:

```haskell
clean = users & select {.id, e = .email & trim >>> lower}
```

Because `&` is the pipeline it is the loosest operator, so its right operand is
a whole expression — the same as in Haskell, where `&` is `infixl 1` and `==` is
`infix 4`. Compare after applying by parenthesising:

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

### The prelude at a glance

**Comparison and logic:** `==` `!=` `<` `<=` `>` `>=` `&&` `||` `not`

**Arithmetic:** `+` `-` `*` `/` `%` `negate` (int and float), `<>` / `concat`

**Strings:** `upper` `lower` `trim` `ltrim` `rtrim` `length` `like` `ilike`
`substring start len s` `left n s` `right n s` `replaceAll from to s`
`strpos sub s` `contains sub s` `startsWith prefix s` `endsWith suffix s`

**Dates and timestamps:** `currentDate` `now` `toDate` `toTimestamp` `toString`
`addDays` `addWeeks` `addMonths` `addQuarters` `addYears` `addHours`
`addMinutes` `addSeconds`; `truncYear` `truncQuarter` `truncMonth` `truncWeek`
`truncDay` `truncHour` `truncMinute`; `year` `quarter` `month` `day`
`dayOfWeek` `dayOfYear` `hour` `minute`; `daysBetween start end`

**Nulls:** `coalesce` `??` `just` `nullIf` `isNull` `isNotNull` `isTrue`
`whereTrue` `eqMaybe`

**Casts:** `toInt` `toFloat` `toBool` `toString`

**Conditionals:** `ifThenElse` (alias `caseWhen`)

Positions are 1-based, as in SQL. `addWeeks` and `addQuarters` are defined in
Cagara as 7 days and 3 months, so only `DAY` and `MONTH` need dialect support.
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
| `currentDate` (no argument) | — | — | `date` |
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

Every date and string operation above calls a `CAGARA_*` intrinsic rather than
spelling ANSI SQL directly, so that
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

Use an alias to make a module's definitions explicit: **`as` is not optional**,
and imported names are reachable only through the alias:

```haskell
import "sch.cagara" as s
x = users & select {.id}     # error: unknown name `users`
```

Relative paths resolve against the importing file, so a module can be imported
from anywhere. Cycles and duplicate definitions are reported.

Modules are also how you share table declarations:

```haskell
# schema.cagara
users : query { id = int, name = string, password_hash = string } = table "public" "users"

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

**A filter over an aggregate is `HAVING`.** The predicate is written against the
aggregate outputs, and the aggregate expression is inlined into the clause, so
the grouped `SELECT` needs no wrapper:

```haskell
t = orders & agg { user_id = group .user_id, total = coalesce 0.0 (sum .amount) } & where (.total > 100.0)
```

```sql
SELECT user_id, COALESCE(SUM(amount), 0.0) AS total
FROM public.orders
GROUP BY user_id
HAVING (COALESCE(SUM(amount), 0.0) > 100.0);
```

**A derived table appears** after a window, after `limit`/`offset`, and after an
aggregate when a later stage still has to read the grouped rows — a second
`agg`, or a filter that runs after an intervening `order` or `limit`:

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

**Join inputs are inlined** when they only project or filter a table. A filter
on a preserved side moves to `WHERE`; a filter on a left join's right side moves
into the `ON` clause.

**Repeated relational subtrees become CTEs** automatically.

`--optimize` additionally runs sqlglot's optimizer (constant folding, boolean
simplification, pushdown). It is opt-in, and it preserves filters outside window
and `LIMIT` boundaries and keeps an aggregate filter in `HAVING`.

---

## 17. Command line

```
cagara <file.cagara> [--dialect NAME] [--only DEF] [--pretty] [--optimize] [--types]
cagara fmt [--check] <files...>
cagara lsp
```

| Flag | Effect |
|---|---|
| `--dialect NAME` | target a SQL dialect (`postgres`, `mysql`, `sqlite`, `duckdb`, `tsql`, `bigquery`, `snowflake`, `trino`, `spark`, ...) |
| `--only DEF` | compile just the named query definition |
| `--types` | print inferred types instead of SQL |
| `--pretty` | pretty-print the generated SQL |
| `--optimize` | run the SQL optimizer |

Without `--only`, every query definition in the file is compiled. Diagnostics
are printed as `file:line:col` with the offending source line underlined, and
the process exits non-zero when there are errors.

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

The language server, `cagara lsp`, provides diagnostics, hover with inferred
types, go-to-definition, find-references, document symbols, and completion
(including column completion after `.`, `.<`, and `.>`). Editor integrations are
in [`editors/vscode`](../editors/vscode) and [`editors/nvim`](../editors/nvim).

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
| `` `where` cannot filter on a window function `` | `select` the window, then filter the new column |
| `type mismatch: expected int, found float` | convert explicitly with `toFloat` / `toInt` |
| `only one side is nullable` | wrap with `coalesce`, or use `just` |
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
