# A Cagara guide

Cagara is a small typed functional language that describes a SQL query as a
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
- [13. Writing your own SQL with `sql` templates](#13-writing-your-own-sql-with-sql-templates)
- [14. Modules](#14-modules)
- [15. What the compiler emits](#15-what-the-compiler-emits)
- [16. Command line](#16-command-line)
- [17. Errors you will meet](#17-errors-you-will-meet)
- [18. Style guide](#18-style-guide)
- [19. Where to go next](#19-where-to-go-next)

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

From tightest to loosest:

| Level | Operators | Associativity |
|---|---|---|
| 1 | application (`f x y`) | left |
| 2 | `??` | right |
| 3 | `*` `/` `%` | left |
| 4 | `+` `-` | left |
| 5 | `<>` (string concatenation) | right |
| 6 | `==` `!=` `<` `<=` `>` `>=` | non-associative |
| 7 | `&&` | right |
| 8 | `\|\|` | right |
| 9 | `&` and the stage/join shorthands | left |

Two consequences worth remembering:

```haskell
# ?? binds tighter than arithmetic, so this is COALESCE(s, 0) + 1
a = t & select { x = .s ?? 0 + 1 }

# <> is right-associative, like the :: of list languages
b = t & select { l = .first <> " " <> .last }
```

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

Every row is fully known: a field list names every output column. There is no
"select all except" operation, because that would make the row type depend on
the table's runtime shape. To drop a column, name the ones you keep.

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

But after `agg` or `limit`, a `where` becomes an outer query, because SQL cannot
filter on an aggregate or on a limited result in the same `SELECT`:

```haskell
b = orders & agg { n = count } & where (.n > 5)
c = orders & select {.id} & limit 5 & where (.id > 1)
```

```sql
SELECT n FROM (SELECT COUNT(*) AS n FROM public.orders) AS t1 WHERE (n > 5);
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
| `isNull x` / `isNotNull x` | test for `NULL`, returning non-null `bool` |
| `isTrue x` | treat a nullable boolean `NULL` as false |

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
  & inner orders (.<id == .>user_id)
  & select { id = .id, amount = .amount }
```

| Function | Shorthand | Keeps |
|---|---|---|
| `inner q on` | `?` | only matching rows |
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
  & inner users (.<user_id == .>id)
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

bad3 = orders & inner x (.<id == (.>id + rowNumber {}))
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
everyone = users & union vips
both     = users & intersect vips
rest     = users & except vips
```

The result keeps the columns and order of the left input. (The emitted SQL lists
the operands in the order it evaluates them safely; the observable result
follows the left query.)

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

| Name | Meaning |
|---|---|
| `f >>> g` | `g` after `f` (composition) |
| `x & f` | apply `f` to `x` |
| `f $ x` | apply `f` to `x` |

Composition with `>>>` is why subject-last matters:

```haskell
clean = users & select {.id, e = .email & trim >>> lower}
```

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

**Nulls:** `coalesce` `??` `just` `isNull` `isNotNull` `isTrue`

**Casts:** `toInt` `toFloat` `toBool` `toString`

**Conditionals:** `ifThenElse` (alias `caseWhen`)

Positions are 1-based, as in SQL. `addWeeks` and `addQuarters` are defined in
Cagara as 7 days and 3 months, so only `DAY` and `MONTH` need dialect support.
String literals widen to `date` and `timestamp`, so comparisons read naturally:

```haskell
recent = orders & where (.created_at >= "2024-01-01")
```

---

## 13. Writing your own SQL with `sql` templates

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

## 14. Modules

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

## 15. What the compiler emits

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

**A derived table appears** after aggregation, after a window, and after
`limit`/`offset`, because SQL must finish those before another stage can see
their results:

```haskell
t = orders & agg { user_id = group .user_id, total = sum .amount } & where (.total > 100.0)
```

```sql
SELECT user_id, total
FROM (SELECT user_id, SUM(amount) AS total FROM public.orders GROUP BY user_id) AS t1
WHERE (total > 100.0);
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
simplification, pushdown). It is opt-in, and it preserves filters outside
window, `LIMIT`, and aggregate boundaries.

---

## 16. Command line

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

## 17. Errors you will meet

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

## 18. Style guide

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

## 19. Where to go next

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
