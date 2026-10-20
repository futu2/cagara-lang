# Cagara: one-file reference for code-generating agents

**Version 0.3.1.** This document is self-contained: it assumes no repository access and no SQL
habits. Every block tagged `cagara` is a complete file that compiles with
`cagara file.cagara` and exits 0; `crates/cagara-cli/tests/doc_blocks.rs` compiles all of them in
CI. Fragments and grammar use `text`.

Give this file to an agent as context, then ask for **one** `.cagara` file.

---

## How to use this document

1. Read §0 and §1 first. They contain the rules that account for most first-attempt failures.
2. Copy the shape of §10 examples rather than translating SQL line by line.
3. Use only names from §7. If a function is absent, write a `sql` template (§8.4) or say it is
   unavailable — do not invent a Cagara name.
4. Before returning code, run the checklist in §12 and, if possible, `cagara file.cagara`.
5. Success is `exit 0` and one SQL statement beginning `-- main`. A file with no `main` is a
   library and prints nothing.

---

## 0. Contract: twelve rules that prevent most errors

1. **A file is definitions only.** Each item is `name = expression`, optionally `name : type = expression`.
   There are no statements, no `let`, no `return`, no top-level `;`.
2. **Indentation is structural.** An identifier or `import` in **column 0** starts a new definition.
   Indent every continuation line: record fields, list items, `case` arms, and row-type fields.
3. **Every table has a closed row annotation.**
   `users : query { id = int, name = string } = table "public" "users"`.
4. **`.col` reads the current row. A bare `name` is a definition, never a column.**
   Write `.amount`, not `amount`.
5. **`select` replaces the row, `update` merges into it, `agg` declares a grouped row.**
   `omit "c"` drops one column; `prefix "p_"` / `suffix "_s"` rename every column.
6. **Aggregates live only in `agg`.** Every field there is either `group expr` or an aggregate
   expression (constants are also allowed). Aggregates never nest.
7. **Windows live only in `select` or `update` fields.** Compute one in a stage, then filter or sort
   on the resulting column in a later stage.
8. **Predicates and sort keys are row-phase and non-null.** Handle `maybe` with `coalesce`, `??`,
   `isNull`/`isNotNull`, `isTrue`, `whereTrue`, `eqMaybe`.
9. **Joins require side-qualified columns in the predicate:** `.<x` (left) and `.>x` (right).
   Shared output names keep the left value; outer joins make the nullable side `maybe`.
10. **Set operations require identical rows:** same names, same order, same types.
11. **`[..]` is a fixed key list, not a general list value.** It is valid as `order` keys, a window
    spec's `partition`/`order`, and `inList` members. Nowhere else.
12. **`main` is the entry point and should be a `query`.** A scalar `main = 1` compiles but
    prints no SQL, so make `main` a query when you want output.

If code can be executed, run `cagara file.cagara` before answering. Diagnostics are specific; §11
maps each common one to its fix.

---

## 1. Program shape

A file is a sequence of imports and definitions. Order does not matter; a definition may refer to
one written later. Comments start with `#` and run to end of line.

```cagara
# Optional module imports (see §8.3).
users : query { id = int, name = string, age = int, active = bool } =
  table "public" "users"

adults = users
  & where (.age >= 18 && .active)
  & select { id = .id, label = upper .name }
  & order [asc .label]
  & limit 10

# The definition printed by `cagara file.cagara`.
main = adults
```

### 1.1 Layout rule

The parser ends a definition at the next `ident` or `import` that begins in **column 0**. Therefore:

```text
main = users
  & where (.age > 18)        # OK: indented continuation
```

```text
main = users
& where (.age > 18)          # OK: the line starts with `&`, not an identifier
```

```text
main = users
where (.age > 18)            # WRONG: `where` in column 0 starts a new definition
```

```text
main =
users                        # WRONG: body starts in column 0
```

This applies inside brackets too. Indent record fields, list elements, `case` arms, and type fields:

```cagara
users : query { id = int, name = string, age = int } =
  table "public" "users"

main = users & select {
    id = .id,
    label = upper .name
  }
```

Trailing commas are allowed in records, lists, and row types. A `;` is used **only** between `case`
arms.

### 1.2 Definitions, annotations, overloading

- `name = expr` is a definition. `name : type = expr` annotates it.
- The `=` belongs to the same item as the annotation. Keep `=` on the annotation line or indent the
  continuation:

```cagara
users : query { id = int, age = int } = table "public" "users"

addOne : expr r int -> expr r int =
  x => x + 1

main = users & select { .id, next = addOne .age }
```

- A name defined more than once must have a type signature on **every** definition; that is
  overloading (§4.7). Duplicate unannotated definitions are an error.
- Definitions without signatures are inferred and may be polymorphic. A helper that reads a named
  column must **not** be annotated with an open `query r` row; either omit the signature or write a
  closed row. This fails:

```text
byId : query r -> expr r int -> query r = i => q => where (.id == i) q
# error: `byId` does not match its signature: no column `id`
```

Use this instead:

```cagara
users : query { id = int, name = string } = table "public" "users"

byId = i => q => where (.id == i) q
main = users & byId 3
```

### 1.3 Imports

- `import "schema.cagara" as s` makes names reachable as `s.users`, `s.shout`, and operators as
  `s._+_`.
- `import "schema.cagara"` (no `as`) brings the names into scope **unqualified** in the current
  compiler. Use `as` in generated code: it is unambiguous and stable.
- Paths resolve relative to the importing file. The prelude is imported automatically.

```text
import "schema.cagara" as s
public_users = s.users & select { .id, display_name = .name }
```

Recommended style: always use `as`, and keep table declarations in one `schema.cagara`.

### 1.4 Modules and entry points

`cagara file.cagara` compiles the definition named `main`. `--only name` compiles another definition.
A file with no `main` is a library: it is still checked, but prints nothing and exits 0.

---

## 2. Lexical syntax

### 2.1 Identifiers and names

```text
name        := [A-Za-z_][A-Za-z0-9_]*
operator    := '_' punctuation+ '_'        e.g. _+_, _&?_, _??_
field       := '.' name                    e.g. .id, .created_at
left field  := '.<' name                   e.g. .<user_id   (join predicate only)
right field := '.>' name                   e.g. .>id        (join predicate only)
qualified   := name '.' name               e.g. schema.users  (no space around the dot)
```

Names are case-sensitive. A column name must match `[A-Za-z_][A-Za-z0-9_]*`; quoted column names are
not representable. `import`, `as`, `sql`, `primitive`, and `case` are keywords only where a keyword
is expected; they are legal field names (`.case`, `{ case = int }`). `true` and `false` are boolean
literals, not names.

### 2.2 Literals

| Literal | Type | Notes |
|---|---|---|
| `42` | `int` | 64-bit; limit `9223372036854775807` |
| `1.5` | `float` | digits on both sides of `.` |
| `1.0e3`, `1.5e-2` | `float` | exponent allowed only with a decimal point |
| `"text"` | `string` | must close on the same line |
| `true`, `false` | `bool` | lowercase |

Strings understand `\n`, `\t`, `\\`, `\"`. Any other `\x` is kept literally. There is no `NULL`
literal, no multiline string, and no interpolation.

Negative numbers use unary `-`, which is part of the expression grammar. When a negative number is a
function argument, parenthesize it:

```cagara
orders : query { id = int, d = date } = table "public" "orders"

main = orders & select { .id, week_ago = addDays (-7) .d }
```

`addDays -7 .d` parses as subtraction and fails.

### 2.3 Grammar

```text
file        := item*
item        := import | definition
import      := 'import' STRING ('as' IDENT)?
definition  := IDENT (':' type)? '=' expr

type        := ty_app ('->' type)?
ty_app      := IDENT ty_atom* | ty_atom
ty_atom     := IDENT | STRING | '(' type ')' | '{' ty_field (',' ty_field)* ','? ('|' IDENT)? '}'
ty_field    := field_name '=' type

expr        := lambda | binary
lambda      := IDENT '=>' expr
binary      := unary (OP binary)?          -- precedence per §4.2
unary       := '-' unary | application
application := atom+
atom        := IDENT ('.' name)*
             | INT | FLOAT | STRING
             | '.name' | '.<name' | '.>name'
             | '(' expr ')'
             | '{' fields? '}'
             | '[' (expr (',' expr)* ','?)? ']'
             | 'sql' STRING
             | 'primitive' STRING
             | 'case' '{' arm (';' arm)* ';'? '}'
fields      := (IDENT '=' expr | '.' name) (',' ...)* ','?
arm         := expr '=>' expr
```

Application binds tighter than every operator. `f a b` applies left to right. There are no call
parentheses: write `coalesce 0 .x`, never `coalesce(0, .x)`.

---

## 3. Types

### 3.1 Scalar types

| Type | SQL | Notes |
|---|---|---|
| `int` | integer | 64-bit; `/` truncates toward zero; `%` keeps the dividend's sign |
| `float` | floating point | no implicit conversion to/from `int` |
| `string` | text | double-quoted literals |
| `bool` | boolean | only `true`/`false` and comparisons |
| `date` | date | string literals widen to it where a `date` is expected |
| `timestamp` | timestamp | string literals widen to it where a `timestamp` is expected |
| `maybe a` | nullable `a` | explicit nullability; see §6.3 |
| `list a` | fixed list | only key lists and `inList`; no list operations |

There is no `decimal`, `uuid`, `json`, `array`, or user-declared type. `toDecimal` changes the SQL
type while the language type stays `float`.

### 3.2 Row and query types

```text
query { id = int, name = string }          a relation with these columns, in this order
expr r a                                   an expression over row r producing a
agg (expr r a)                             an aggregate expression
win (expr r a)                             a window expression
list (sortkey r)                           sort keys for `order`
winspec r                                  a window specification record
```

Most of these appear only in signatures and diagnostics. An agent normally writes only closed
`query { ... }` annotations, scalar types, and `maybe`.

### 3.3 Inference, widening, and conversion

- `1` is polymorphic: it can satisfy `int` or `float`. `1.5` is always `float`.
- A string literal widens to `date` or `timestamp` only when the expected type is known.
- There are no other implicit conversions. `int` and `float` never mix: use `toFloat`/`toInt`.
- An overloaded name with an unconstrained literal is ambiguous. `fromString "1"` is ambiguous;
  `toInt` is not (destination known). When in doubt, use `toInt`/`toFloat`/`toDate`/`toTimestamp`.
- Widening is driven by the **expected** type. A bare string passed to a function overloaded across
  `date` and `timestamp` stays `string` and fails: `addDays 7 "2024-01-01"` is an error. Wrap it:
  `addDays 7 (toDate "2024-01-01")`.

```cagara
orders : query { id = int, amount = float, created_at = date } = table "public" "orders"

main = orders
  & where (.created_at >= "2024-01-01")
  & select { .id, doubled = .amount * 2.0, n = .id + 1 }
```

---

## 4. Expressions

### 4.1 Application, lambdas, and definitions

- `f x y` applies `f` to `x`, then to `y`.
- `x => body` is a one-parameter lambda; curry for more.
- Functions take their **subject last**: `upper .name`, `substring 1 3 .s`, `coalesce 0 .score`,
  `addDays 7 .d`. This is what makes partial application a reusable stage.
- `q & stage` means `stage q`. The pipeline's right operand is a whole expression.

```cagara
users : query { id = int, name = string } = table "public" "users"

addOne = x => x + 1
clean = trim >>> lower
byName = n => q => where (.name == n) q

main = users & byName "ada" & select { .id, x = addOne .id, e = .name & clean }
```

### 4.2 Operators and precedence

Tighter binds first. Application is tighter than every operator.

| Level | Operators | Associativity | Meaning |
|---|---|---|---|
| — | `f x` | left | application |
| 23 | `>>>` `<<<` | right | composition |
| 21 | `??` | right | null fallback |
| 19 | `*` `/` `%` | left | arithmetic |
| 17 | `+` `-` | left | arithmetic |
| 15 | `<>` | right | string concatenation |
| 13 | `==` `!=` `<` `<=` `>` `>=` | left | comparison |
| 11 | `&&` | left | boolean and |
| 9 | `\|\|` | left | boolean or |
| 3 | `?` `<?` `?>` `<?>` | left | joins |
| 1 | `&` and stage shorthands | left | pipeline/apply |
| 0 | `$` | right | apply (loosest) |

Consequences worth memorizing:

- `??` binds tighter than arithmetic: `.s ?? 0 + 1` is `COALESCE(s, 0) + 1`.
- `&` is looser than every expression operator (only `$` is looser), so
  `q & where (.a > 1) & select {.a}` chains.
- `>>>` is tighter than `&`, so `q & (f >>> g)` composes before applying.
- `<>` is **concatenation**, not inequality.

Arithmetic is not overloaded across types. `+ - * / %` are `int` with `int` or `float` with `float`.
`/` on `int` truncates toward zero; `%` keeps the dividend's sign, in every dialect.

### 4.3 Records

A record is `{ name = expr, ... }`, or `{ .name }` as shorthand for `{ name = .name }`. Records are
used for `select`/`update`/`agg` field lists, window specs, and custom signatures. Field order is
preserved. A field name is an identifier; it may not be quoted.

```cagara
users : query { id = int, name = string } = table "public" "users"

main = users & select { .id, label = upper .name }
```

Fields in one record are independent: a later field cannot read an earlier field's alias. Use a
second `select`/`update` stage instead.

### 4.4 Lists

`[a, b, c]` is a fixed-length key list. It is accepted only where keys or `IN` members are expected:

- `order [asc .a, desc .b]`
- `{ partition = [.user_id], order = [asc .ts] }`
- `inList [1, 2, 3] .id`

Elements must be homogeneous. `[1, 2.0]` is an error. `[]` is accepted where keys are expected.
There is no `map`, `append`, or list-typed variable.

### 4.5 Conditionals

`ifThenElse cond then else` compiles to a searched `CASE`. Both values must have the same type, and
the condition must be a non-null `bool`.

`case { cond => value; ...; true => value }` is the multi-way form. Arms are separated by `;`, not
`,`; the whole expression is braced so it nests inside records and arguments. The final arm must be
literally `true => value`; it is the required default.

```cagara
orders : query { id = int, amount = float, status = string } = table "public" "orders"

main = orders & select {
    .id,
    bucket = case {
      .amount > 1000.0 => "huge";
      .amount > 100.0 => "big";
      true => "small"
    }
  }
```

Rules:

- Conditions are tried top to bottom; the first true wins.
- Every condition is a non-null `bool`; a `maybe bool` needs `isTrue` or `coalesce`.
- Every value has one type; `ifThenElse` has the same rule.
- Arms share a phase: aggregate arms belong in `agg`, window arms in `select`.

### 4.6 Casts

Casts are named by their destination and are explicit:

| Call | From | To |
|---|---|---|
| `toInt x` | `float`, `string` | `int` |
| `toFloat x` | `int`, `string` | `float` |
| `toBool x` | `int`, `float` | `bool` |
| `toString x` | `int`, `float`, `date`, `timestamp` | `string` |
| `toDate x` | `timestamp`, `string` | `date` |
| `toTimestamp x` | `date`, `string` | `timestamp` |
| `toDecimal p s x` | `int`, `float`, `string` | `float` (SQL `DECIMAL(p,s)`) |
| `tryInt x`, `tryFloat x` | `string` | `maybe int` / `maybe float` |
| `tryDecimal p s x` | `string` | `maybe float` |

`try*` returns `maybe`; handle it (`coalesce 0 (tryInt .code)`). SQLite has no `TRY_CAST`; a `try*`
cast is not portable there. `fromString`/`fromInt`/`fromFloat` are alternative spellings of the
`to*` functions, named by source; `fromString` is overloaded and often ambiguous.

### 4.7 Overloading and aliases

A name defined more than once, each definition with a signature, is an overload set. The checker
picks the candidate that fits the use. `+`, `-`, `*`, `/`, `sum`, `toInt`, etc. work this way.

```cagara
double : expr r int -> expr r int = x => x + x
double : expr r float -> expr r float = x => x + x

users : query { age = int, score = float } = table "public" "users"
main = users & select { a = double .age, b = double .score }
```

An alias is a definition whose body is another name; it carries overloading with it:

```cagara
s = sum
u = upper
```

Each use re-selects the overload, so `s .amount` and `s .id` may pick different candidates.

### 4.8 Composition and pipeline values

A stage is any function from a query to a query. It can be named, composed, or written inline:

```cagara
users : query { id = int, age = int } = table "public" "users"

public_ids = select {.id} >>> where (.id > 1)
adults = users & (q => where (.age > 18) q)
main = users & public_ids
```

`f >>> g` applies `f` first, then `g`; `f <<< g` applies `g` first. A composed stage binds tighter
than `&`, so no parentheses are needed around `f >>> g` in `q & f >>> g`; use parentheses when the
composition contains a pipeline.

---

## 5. Query stages and their expression constructors

Every stage takes the query **last**, so `q & stage` and `stage q` are the same. The table lists the
signature, the output row, and the constraints. The last three rows (`inList`, `asc`/`desc`, `group`)
are expression constructors used *inside* stages, not query stages themselves.

| Stage | Signature | Output row | Key rules |
|---|---|---|---|
| `table "s" "t"` | `string -> string -> query r` | declared row | closed annotation mandatory |
| `where p` | `expr r bool -> query r -> query r` | same as input | `p` row-phase, non-null `bool` |
| `select f` | `expr r (row s) -> query r -> query s` | exactly `f`'s fields | fields row/win/const; no aggregates |
| `update f` | `expr r (row s) -> query r -> query (merge r s)` | input overlaid by `f` | right-wins; new names append |
| `agg f` | `agg (expr r (row s)) -> query r -> query s` | exactly `f`'s fields | each field `group` or aggregate/const |
| `order ks` | `list (sortkey r) -> query r -> query r` | same as input | row-phase keys; NULLs last |
| `limit n` | `expr r int -> query r -> query r` | same as input | non-negative literal/constant |
| `offset n` | `expr r int -> query r -> query r` | same as input | non-negative literal/constant |
| `distinct` | `query r -> query r` | same as input | deduplicates the whole row |
| `omit "c"` | `symbol k -> query r -> query (removeKey k r)` | input without `c` | key is a literal |
| `prefix "p"` | `symbol k -> query r -> query (keyMap ... r)` | every name prefixed | key is a literal |
| `suffix "s"` | `symbol k -> query r -> query (keyMap ... r)` | every name suffixed | key is a literal |
| `innerJoin q p` | `query s -> expr (join r s) bool -> query r -> query (merge r s)` | left then right-only | predicate uses `.<`/`.>` |
| `leftJoin q p` | same, result right side nullable | left then right-only | right columns `maybe` |
| `rightJoin q p` | same, result left side nullable | left then right-only | left columns `maybe` |
| `fullJoin q p` | same, both sides nullable | left then right-only | both sides `maybe` |
| `semiJoin q p` | `query s -> expr (join r s) bool -> query r -> query r` | left only | rows with a match |
| `antiJoin q p` | same | left only | rows without a match |
| `union q2` | `query r -> query r -> query r` | left row | deduplicates |
| `unionAll q2` | same | left row | keeps duplicates |
| `intersect q2` | same | left row | deduplicates |
| `except q2` | same | left row | deduplicates |
| `inList [..] e` | `list a -> expr r a -> expr r bool` | `bool` | members homogeneous |
| `asc e`, `desc e` | `expr r a -> sortkey r` | sort key | only inside `order`/spec |
| `group e` | `expr r a -> agg (expr r a)` | aggregate key | only inside `agg` |

### 5.1 `table`

```cagara
users : query { id = int, name = string, deleted_at = maybe timestamp } =
  table "public" "users"

main = users
```

Two string literals: schema and table. The annotation is not checked against a live database; it is
the source of truth. A table without a closed annotation fails with “the columns ... are unknown”.

### 5.2 `where`

```cagara
users : query { id = int, age = int, active = bool } = table "public" "users"

main = users & where (.age >= 18 && .active)
```

`where` sees the row produced by the previous stage. A predicate may not **construct** an aggregate or
a window, and it must be non-null. A `maybe bool` needs `whereTrue` or `isTrue`: after `agg`, `.n` is
an ordinary `int` column, so `where (.n > 5)` is legal.

```cagara
t : query { id = int, flag = maybe bool } = table "public" "t"

main = t & whereTrue .flag
```

### 5.3 `select` and `update`

`select` **replaces** the row with exactly the listed fields. `update` **merges** over the row:
existing names keep their position and take the new value; new names append; unmentioned columns
pass through.

```cagara
users : query { id = int, name = string, age = int } = table "public" "users"

picked = users & select { id = .id, label = upper .name }     # id, label
merged = users & update { age = .age + 1, display = .name }   # id, name, age, display
```

`select {.a, .b}` is shorthand for `{a = .a, b = .b}`. To rename, write `new = .old`. To drop a
column, use `omit`; `select` is a whitelist that fails closed when the schema grows.

### 5.4 `agg`

`agg` declares the output row. Each field is either a group key (`group expr`) or an aggregate
expression. Constants are allowed. If no field is grouped, the result is one row for the whole input.

```cagara
orders : query { id = int, user_id = int, amount = float, status = string } =
  table "public" "orders"

main = orders
  & where (.status == "paid")
  & agg { user_id = group .user_id, revenue = sum .amount, n = count }
  & where (.n >= 5)
  & order [desc (coalesce 0.0 .revenue)]
```

Rules:

- Ungrouped bare columns are rejected: `agg { n = count, name = .name }` fails.
- Aggregates do not nest: `sum (sum .amount)` fails. Use an earlier `agg`.
- Aggregates belong only in `agg`; `select { x = sum .amount }` fails.
- A later `agg` may aggregate the previous `agg`'s output; that is a separate stage, not nesting.
- `sum`, `avg`, `min`, `max` return `maybe`. `count`, `countOf`, `countDistinct` return `int`.
- A filter after `agg` is an outer `WHERE` over a derived table; there is no `HAVING` keyword.
- A window may read an aggregate's output in a later `select`; a window field in `agg` is rejected.

Correct grouped report:

```cagara
orders : query { id = int, user_id = int, amount = float } = table "public" "orders"

main = orders & agg {
    user_id = group .user_id,
    total = coalesce 0.0 (sum .amount),
    n = count
  }
```

### 5.5 `order`, `limit`, `offset`, `distinct`

```cagara
orders : query { id = int, amount = float, created_at = date } = table "public" "orders"

main = orders
  & order [desc .amount, asc .id]
  & limit 20
  & offset 40
  & distinct
```

- `order` takes a list of `asc`/`desc` keys; keys are expressions over the current row.
- NULLs sort **last** in both directions; this is fixed by the language.
- `limit` and `offset` take an integer literal or a definition that is an integer constant. They do
  not order by themselves.
- `distinct` deduplicates the whole row; it is the only deduplicating stage besides `union`,
  `intersect`, and `except`.
- `offset` has no shorthand; `&^` is **intersect**, not offset.

### 5.6 `omit`, `prefix`, `suffix`

```cagara
users : query { id = int, name = string, password_hash = string } =
  table "public" "users"

no_secret = users & omit "password_hash"
prefixed = users & prefix "u_"
suffixed = users & suffix "_v2"

main = no_secret & select { .id, .name }
```

- `omit` drops exactly one column; chain for more.
- `prefix`/`suffix` rename every column; later stages use the new names.
- At a direct use the argument is a string literal. A helper may take the label as a parameter
  (`addPrefix = p => q => q & prefix p`, used as `t & addPrefix "x_"`), because the parameter is
  bound to a literal at the call site. A `string`-typed definition or computed expression is **not**
  a `symbol` and is rejected.
- `select` (whitelist) and `omit` (blacklist) have opposite behaviour when the schema changes; pick
  deliberately.

### 5.7 Joins

The predicate **must** name the side of every column: `.<x` is the left input, `.>x` the right.
Plain `.x` is an error inside a join predicate.

```cagara
users : query { id = int, name = string } = table "public" "users"
orders : query { id = int, user_id = int, amount = float } = table "public" "orders"

main = users
  & innerJoin orders (.<id == .>user_id)
  & select { .id, .name, .amount }
```

| Named | Operator | Keeps |
|---|---|---|
| `innerJoin q p` | `q ? p` | matching rows only |
| `leftJoin q p` | `q <? p` | all left rows |
| `rightJoin q p` | `q ?> p` | all right rows |
| `fullJoin q p` | `q <?> p` | all rows from both |
| `semiJoin q p` | — | left rows with a match (left columns only) |
| `antiJoin q p` | — | left rows with no match (left columns only) |

Output columns: left input's columns, then the right input's columns that the left does not have. On
a shared name the **left** value wins. To keep both, rename one side **before** joining:

```cagara
users : query { id = int, name = string } = table "public" "users"
orders : query { id = int, user_id = int, amount = float } = table "public" "orders"

main = orders
  & select { order_id = .id, .user_id, .amount }
  & innerJoin users (.<user_id == .>id)
  & select { .order_id, .name, .amount }
```

After a left/right/full join, the nullable side's columns become `maybe` (never doubly so). The join
predicate still sees the non-null input types. `.<` and `.>` are valid **only** inside the predicate;
after the join refer to `.x`, which follows the left-wins rule.

Filtering the nullable side is a semantic choice. `where (isNotNull .amount)` drops the unmatched
rows; omit it to keep them. A comparison such as `where (.amount > 0.0)` is a type error until the
value is handled (`coalesce`, `isNull`). Do not use `countOf` on the nullable side to count matches;
the nullable argument is rejected. Sum an indicator instead:

```cagara
users : query { id = int } = table "public" "users"
orders : query { user_id = int, amount = float } = table "public" "orders"

main = users
  & leftJoin orders (.<id == .>user_id)
  & agg {
      id = group .id,
      matched = coalesce 0 (sum (ifThenElse (isNotNull .user_id) 1 0))
    }
```

A join input may be any query; parenthesize a pipeline on the right:

```cagara
users : query { id = int } = table "public" "users"
orders : query { user_id = int, amount = float } = table "public" "orders"

main = users
  & leftJoin (orders & where (.amount > 10.0)) (.<id == .>user_id)
  & select { .id, .amount }
```

### 5.8 Set operations

Both operands must have the same row: same names, same order, same types. The result keeps the left
operand's columns and order.

```cagara
a : query { id = int, name = string } = table "public" "a"
b : query { id = int, name = string } = table "public" "b"

main = a &| b
```

| Function | Operator | Duplicates |
|---|---|---|
| `union a b` | `a &\| b` | removed |
| `unionAll a b` | `a &! b` | preserved |
| `intersect a b` | `a &^ b` | removed |
| `except a b` | `a &~ b` | removed |

Only `except` and `intersect` make operand order observable. There is no `INTERSECT ALL` or
`EXCEPT ALL`.

### 5.9 Stage shorthands

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

The table is complete. There is no shorthand for `offset`, `distinct`, `omit`, `prefix`, or `suffix`.
`&^` is intersect, not offset. Long names are clearer for a first attempt; shorthands are optional.

---

## 6. Semantics: rows, phases, nulls

### 6.1 Row changes

| Stage | Output row |
|---|---|
| `where`, `order`, `limit`, `offset`, `distinct` | input row unchanged |
| `select f` | exactly the fields of `f`, in order |
| `update f` | input row with `f` overlaid: existing positions kept, new names appended |
| `agg f` | exactly the fields of `f` |
| `omit "c"` | input row minus `c`, order kept |
| `prefix "p"` / `suffix "s"` | all names rewritten, order and types kept |
| `innerJoin` | left columns, then right-only columns |
| outer join | same, nullable side's types wrapped in `maybe` |
| `semiJoin`, `antiJoin` | left row only |
| set operation | left operand's row |

A name introduced by `select`/`update` is visible to every **later** stage, not to fields in the same
record. A dropped name is invisible later and is reported with “no column”.

### 6.2 Phases

| Construct | Allowed phase |
|---|---|
| `where` predicate | Row |
| `select`/`update` field | Row, Win, or Const |
| `agg` field | Agg or Const (a `group` field is Row) |
| `order` key | Row only |
| window `partition`/`order` key | Row only (scalar expressions allowed; no aggregates/windows) |
| join predicate | Row over the two join inputs |
| `limit`/`offset` | Const int |
| set operands | queries with equal rows |

The practical rule: **if a value is an aggregate or a window, give it its own stage**. A filter on an
aggregate follows `agg`; a filter on a window follows a `select` that names it.

### 6.3 Nullability

`maybe a` means a value that may be SQL `NULL`. A type variable in a signature means non-null, so
operators require non-null operands.

Where `maybe` comes from:

| Source | Result |
|---|---|
| column declared `maybe` | `maybe` |
| nullable side of left/right/full join | that side's columns `maybe` |
| `sum`, `avg`, `min`, `max` | `maybe` (empty/all-null input) |
| `lag`, `lead`, `sumOver`, `avgOver` | `maybe` |
| `count`, `countOf`, `countDistinct`, `countOver` | non-null `int` |
| `isNull`, `isNotNull`, `isTrue` | non-null `bool` |
| `try*` casts | `maybe` |

Handling:

| Call | Meaning |
|---|---|
| `coalesce default x` | use `default` if `x` is NULL |
| `x ?? default` | the same, infix |
| `just x` | treat non-null `x` as nullable |
| `nullIf x y` | NULL when `x == y` |
| `isNull x`, `isNotNull x` | tests, non-null `bool` |
| `isTrue x` | nullable bool, NULL becomes false |
| `whereTrue p` | filter with a nullable predicate |
| `eqMaybe x y` | nullable equality; two NULLs are equal |

Examples:

```cagara
users : query { id = int, score = maybe int } = table "public" "users"
orders : query { user_id = int, amount = maybe float } = table "public" "orders"

main = users
  & leftJoin orders (.<id == .>user_id)
  & agg {
      id = group .id,
      total = coalesce 0.0 (sum (coalesce 0.0 .amount)),
      n = coalesce 0 (sum (ifThenElse (isNotNull .user_id) 1 0))
    }
```

`countOf .maybe_column` is rejected because its argument must be non-null. To count matches across a
left join, sum an indicator over the nullable side, as above.

A nullable value also cannot be a sort key or a group key. Wrap it first:

```cagara
users : query { id = int, score = maybe int } = table "public" "users"

main = users
  & order [asc (coalesce 0 .score)]
  & agg { bucket = group (coalesce 0 .score), n = count }
```

Conditional aggregation after an outer join is the common reporting shape; every nullable argument
gets a default. A nullable string compared to a literal must be coalesced to a string first:

```cagara
accounts : query { id = int, name = string } = table "public" "accounts"
tx : query { account_id = int, amount = float, kind = string } =
  table "public" "tx"

main = accounts
  & leftJoin tx (.<id == .>account_id)
  & agg {
      id = group .id,
      name = group .name,
      deposits = coalesce 0.0 (sum (ifThenElse (coalesce "" .kind == "deposit") (coalesce 0.0 .amount) 0.0)),
      withdrawals = coalesce 0.0 (sum (ifThenElse (coalesce "" .kind == "withdrawal") (coalesce 0.0 .amount) 0.0)),
      n = coalesce 0 (sum (ifThenElse (isNotNull .account_id) 1 0))
    }
```

---

## 7. Prelude catalog (exact names and argument order)

The prelude is imported automatically. **Only these names exist** (plus the stage names in §5). If a
function is missing, write a `sql` template (§8.4) or report it unavailable.

Notation: signatures are exact for the shown overloads; `expr r a` means an expression over the
current row. A name with several lines is overloaded.

### 7.1 Combinators and pipeline

```text
_&_    : a -> (a -> b) -> b          x & f      = f x
_$_    : (a -> b) -> a -> b          f $ x      = f x
_>>>_  : (a -> b) -> (b -> c) -> a -> c
_<<<_  : (b -> c) -> (a -> b) -> a -> c
id     : a -> a
```

### 7.2 Comparison, logic, arithmetic, strings

```text
==  !=  <  <=  >  >=   : expr r a -> expr r a -> expr r bool
&&  ||                 : expr r bool -> expr r bool -> expr r bool
not                    : expr r bool -> expr r bool
negate                 : expr r int|float -> same

+ - *                  : expr r int -> expr r int -> expr r int        (also float overloads)
/                      : expr r int -> expr r int -> expr r int        (truncates toward zero)
/                      : expr r float -> expr r float -> expr r float
%                      : expr r int -> expr r int -> expr r int        (sign of dividend)
<>                     : expr r string -> expr r string -> expr r string
concat                 : expr r string -> expr r string -> expr r string

upper lower trim ltrim rtrim : expr r string -> expr r string
length                 : expr r string -> expr r int
substring              : expr r int -> expr r int -> expr r string -> expr r string
left right             : expr r int -> expr r string -> expr r string
lpad rpad              : expr r int -> expr r string -> expr r string -> expr r string
replaceAll             : expr r string -> expr r string -> expr r string -> expr r string
strpos                 : expr r string -> expr r string -> expr r int
contains               : expr r string -> expr r string -> expr r bool
startsWith endsWith    : expr r string -> expr r string -> expr r bool
like ilike             : expr r string -> expr r string -> expr r bool
```

Argument order is subject-last, which is the reverse of the SQL spelling:

| Call | Meaning | Emitted |
|---|---|---|
| `substring start len s` | `len` chars from `start` | `SUBSTRING(s, start, len)` |
| `left n s`, `right n s` | first/last `n` chars | `LEFT(s, n)` |
| `lpad n fill s` | pad left to width `n` | `LPAD(s, n, fill)` |
| `replaceAll from to s` | replace every `from` | `REPLACE(s, from, to)` |
| `strpos sub s` | 1-based position, 0 if absent | `STRPOS(s, sub)` |
| `contains sub s` | plain substring test | `STRPOS(s, sub) > 0` |
| `like pattern s` | SQL `LIKE` | `s LIKE pattern` |
| `concat a b` / `a <> b` | concatenation | `a \|\| b` |

Positions and lengths are 1-based. `contains`/`startsWith`/`endsWith` treat `%` and `_` literally;
`like` does not. `length` counts characters, including trailing spaces.

```cagara
orders : query { id = int, email = string, name = string } = table "public" "orders"

main = orders & select {
    .id,
    slug = replaceAll " " "-" (trim .name),
    local = left (strpos "@" .email - 1) .email,
    domain = lower (substring (strpos "@" .email + 1) 99 .email),
    has_at = contains "@" .email
  }
```

### 7.3 Casts

```text
toInt        : expr r float|string -> expr r int
toFloat      : expr r int|string -> expr r float
toBool       : expr r int|float -> expr r bool
toString     : expr r int|float|date|timestamp -> expr r string
toDate       : expr r timestamp|string -> expr r date
toTimestamp  : expr r date|string -> expr r timestamp
toDecimal    : expr r int -> expr r int -> expr r string|int|float -> expr r float
tryInt       : expr r string -> expr r (maybe int)
tryFloat     : expr r string -> expr r (maybe float)
tryDecimal   : expr r int -> expr r int -> expr r string -> expr r (maybe float)
fromString   : expr r string -> expr r int|float|date     (overloaded; may be ambiguous)
fromInt      : expr r int -> expr r float
fromFloat    : expr r float -> expr r int
```

`toDecimal 18 2 .amount` emits `CAST(amount AS DECIMAL(18, 2))`; the language type stays `float`.

### 7.4 Dates and timestamps

```text
currentDate      : expr r date
currentTimestamp : expr r timestamp
now              : expr r timestamp
yesterday        : expr r date

toDate           : expr r timestamp|string -> expr r date
toTimestamp      : expr r date|string -> expr r timestamp

addDays addWeeks addMonths addQuarters addYears
                 : expr r int -> expr r date -> expr r date
                 : expr r int -> expr r timestamp -> expr r timestamp
addHours addMinutes addSeconds
                 : expr r int -> expr r timestamp -> expr r timestamp

truncYear truncQuarter truncMonth truncWeek
                 : expr r date|timestamp -> same
truncDay truncHour truncMinute
                 : expr r timestamp -> expr r timestamp

year quarter month day dayOfWeek dayOfYear
                 : expr r date|timestamp -> expr r int
hour minute      : expr r timestamp -> expr r int
daysBetween      : expr r date -> expr r date -> expr r int
```

- `date` has no hour; `hour`/`truncHour`/`addHours` require `timestamp`. Convert with `toTimestamp`.
- `daysBetween start end` is positive when `end` is later; use `toDate` on a timestamp first.
- `dayOfWeek` is 0 for Sunday; `truncWeek` starts on Monday (ISO).
- A string literal widens to `date`/`timestamp` where the expected type is known.

```cagara
orders : query { id = int, created_at = timestamp, due = date } = table "public" "orders"

main = orders & select {
    .id,
    month_start = truncMonth .created_at,
    next_month = addMonths 1 .created_at,
    overdue = .due < currentDate,
    age_days = daysBetween .due currentDate,
    hour = hour (toTimestamp .due)
  }
```

### 7.5 Nulls

```text
coalesce   : expr r a -> expr r (maybe a) -> expr r a
??         : expr r (maybe a) -> expr r a -> expr r a
just       : expr r a -> expr r (maybe a)
nullIf     : expr r a -> expr r a -> expr r (maybe a)
isNull     : expr r (maybe a) -> expr r bool
isNotNull  : expr r (maybe a) -> expr r bool
isTrue     : expr r (maybe bool) -> expr r bool
whereTrue  : expr r (maybe bool) -> query r -> query r
eqMaybe    : expr r (maybe a) -> expr r (maybe a) -> expr r bool
```

`coalesce` takes the default **first**; `??` takes the value first: `coalesce 0 .score` and
`.score ?? 0` mean the same. `coalesce` requires a `maybe` input; do not wrap a non-null value in it.

### 7.6 Aggregates (only in `agg`)

```text
count          : agg (expr r int)
countOf        : expr r a -> agg (expr r int)
countDistinct  : expr r a -> agg (expr r int)

sum            : expr r int -> agg (expr r (maybe int))
sum            : expr r float -> agg (expr r (maybe float))
avg            : expr r int|float -> agg (expr r (maybe float))
min max        : expr r a -> agg (expr r (maybe a))
```

`count` is a value, not a call: write `n = count`, not `count ()`. `sum`/`avg`/`min`/`max` return
`maybe`; `coalesce 0 (sum .x)` makes a number. `countOf`/`countDistinct` require non-null input.

### 7.7 Windows (only in `select`/`update` fields)

A window specification is a record:

```text
{ partition = [expr, ...], order = [asc expr, desc expr, ...], frame = frame }
```

All fields are optional; `{}` is valid. `partition`/`order` keys are row-phase: scalar expressions
such as `truncMonth .ts` are fine, but aggregates and windows are not.

```text
rowNumber rank denseRank : winspec r -> win (expr r int)
lag lead                 : winspec r -> expr r a -> win (expr r (maybe a))
sumOver                  : winspec r -> expr r int -> win (expr r (maybe int))
sumOver                  : winspec r -> expr r float -> win (expr r (maybe float))
avgOver                  : winspec r -> expr r int|float -> win (expr r (maybe float))
countOver                : winspec r -> win (expr r int)

rows                     : bound -> bound -> frame
unboundedPreceding       : bound
unboundedFollowing       : bound
currentRow               : bound
preceding following      : expr r int -> bound
wholePartition           : frame
runningFrame             : frame
```

```cagara
orders : query { id = int, user_id = int, amount = float, ts = timestamp } =
  table "public" "orders"

main = orders & select {
    .id,
    rn = rowNumber {partition = [.user_id], order = [desc .ts]},
    running = sumOver {partition = [.user_id], order = [asc .ts], frame = runningFrame} .amount
  }
```

A frame is not a spec: `lag wholePartition .amount` is wrong; write
`lag {frame = wholePartition} .amount`. A spec is a record, not a list: `rowNumber [asc .id]` is
wrong; write `rowNumber {order = [asc .id]}`.

---

## 8. Escape hatches and modules

### 8.1 Precedence cannot be declared

`infixl`/`infixr` are not part of source. Operator precedence is fixed by the compiler; a line that
looks like a declaration is just a definition missing `=`.

### 8.2 Unknown operators

Every operator you write must be in the fixed table (§4.2). A typo such as `&>>` is an
“unknown operator” error rather than a different parse. You cannot add operators from source.

### 8.3 Modules

```text
# schema.cagara
users : query { id = int, name = string, password_hash = string } =
  table "public" "users"
```

```text
# public.cagara
import "schema.cagara" as s

public_users = s.users & select { .id, display_name = .name }
main = public_users
```

- `as` is optional: with it, names are qualified; without it, names are unqualified.
- Imports are relative to the importing file. Cycles and duplicate definitions are errors.
- Imported operators are reachable as `s._+_`.

### 8.4 Custom SQL templates

When the prelude lacks a function, define a `sql` template. **A type signature is mandatory**: it
gives the template its arity (number of `->` before the result) and its phase (`expr`/`agg`/`win`).

```cagara
users : query { id = int, name = string, amount = float } = table "public" "users"

myUpper : expr r string -> expr r string = sql "UPPER($1)"
mySum : expr r float -> agg (expr r (maybe float)) = sql "SUM($1)"

report = users & agg { total = coalesce 0.0 (mySum .amount) }

main = report
```

Placeholders are `$1` ... `$n`, 1-based, standing alone (not inside a name or a string literal), with
no gaps. The SQL body itself is not type checked. Avoid dialect-specific spellings: templates are
parsed and rewritten for the target dialect. Prefer naming a `CAGARA_*` intrinsic only in the
prelude, not in user code.

### 8.5 Internal forms (do not use)

`primitive "..."`, `mapValue`, and `merge` are type-level/internal. `merge` has no relational
lowering; using it is an internal error. `# no prelude` on the first line disables the prelude and
requires redefining `table` with `primitive "__table"`; do not use it unless the task asks.

### 8.6 Tooling

```text
cagara file.cagara [--dialect NAME] [--only DEF] [--pretty] [--optimize] [--types]
cagara -                         # read source from stdin
cagara fmt [--check] <files...>  # format in place; `-` for stdin
```

- `--dialect` is an output concern only; it never changes Cagara semantics. Default `ansi`; common
  targets include `postgres`, `mysql`, `sqlite`, `duckdb`, `tsql`, `bigquery`, `snowflake`, `trino`,
  `spark`. SQLite and DuckDB are executed in CI; other dialects use the ANSI/Postgres fallback for
  some intrinsics.
- `--only DEF` compiles one definition instead of `main`.
- `--types` prints inferred `name : type` lines; useful for checking a row after a stage, but do not
  depend on the printed field order.
- `--pretty` formats the SQL; `--optimize` runs the SQL optimizer.
- `fmt` is a formatter, not a validity fixer: an unparseable file is left unchanged.

---

## 9. SQL → Cagara translation table

Use this to map a request, not to write SQL.

| SQL | Cagara |
|---|---|
| `SELECT a, b FROM t` | `t & select {.a, .b}` |
| `SELECT * FROM t` | `t` (or `t & select {.a, .b, ...}`) |
| `SELECT a AS x FROM t` | `t & select { x = .a }` |
| `SELECT DISTINCT a FROM t` | `t & select {.a} & distinct` |
| `WHERE p` | `t & where p` (before `select` if `p` uses source columns) |
| `GROUP BY k` + aggregates | `t & agg { k = group .k, ... = sum .x }` |
| `HAVING p` | `... & agg { ... } & where p` |
| `ORDER BY x DESC` | `... & order [desc .x]` |
| `LIMIT n OFFSET m` | `... & limit n & offset m` |
| `INNER JOIN u ON a = b` | `t & innerJoin u (.<a == .>b)` |
| `LEFT JOIN u ON a = b` | `t & leftJoin u (.<a == .>b)` |
| `RIGHT JOIN`, `FULL JOIN` | `rightJoin`, `fullJoin` |
| `WHERE EXISTS (...)` | `t & semiJoin u (.<a == .>b)` |
| `WHERE NOT EXISTS (...)` | `t & antiJoin u (.<a == .>b)` |
| `UNION` / `UNION ALL` | `a &\| b` / `a &! b` |
| `INTERSECT` / `EXCEPT` | `a &^ b` / `a &~ b` |
| `COUNT(*)` | `count` inside `agg` |
| `COUNT(x)` | `countOf .x` |
| `COUNT(DISTINCT x)` | `countDistinct .x` |
| `SUM(x)` | `sum .x` (result `maybe`) |
| `AVG(x)` | `avg .x` (result `maybe float`) |
| `MIN(x)` / `MAX(x)` | `min .x` / `max .x` (result `maybe`) |
| `COALESCE(x, 0)` | `coalesce 0 .x` or `.x ?? 0` |
| `CASE WHEN c THEN a ELSE b END` | `ifThenElse c a b` |
| `CASE WHEN ... WHEN ... ELSE ... END` | `case { c1 => v1; c2 => v2; true => v3 }` |
| `CAST(x AS INT)` | `toInt .x` |
| `x IS NULL` / `x IS NOT NULL` | `isNull .x` / `isNotNull .x` |
| `x IN (1, 2, 3)` | `inList [1, 2, 3] .x` |
| `x BETWEEN a AND b` | `.x >= a && .x <= b` |
| `x LIKE 'p%'` | `like "p%" .x` |
| `x \|\| y` | `.x <> .y` |
| `ROW_NUMBER() OVER (...)` | `rowNumber {partition = [.k], order = [desc .t]}` |
| `SUM(x) OVER (...)` | `sumOver {partition = [.k], order = [asc .t]} .x` |
| `LAG(x) OVER (...)` | `lag {partition = [.k], order = [asc .t]} .x` |
| `WITH q AS (...)` | define `q = ...`; reuse `q` (the compiler emits CTEs for repeated subtrees) |

### 9.1 Constructs that do not exist

If a request needs one of these, say so or write a `sql` template; do not guess a spelling.

- **SQL statement syntax:** `SELECT`, `FROM`, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY` and the
  other SQL clauses as keywords. Cagara's lowercase `where`, `order`, `limit`, `offset`, `select`,
  and `agg` are functions/stages, not clauses.
- **Operators:** words `AND`, `OR`, `NOT`; `=` for equality (use `==`); `<>` for inequality (it is
  concatenation); `\|\|` for concatenation (it is boolean or); `!`; `^` as power.
- **Values:** `NULL`, uppercase `TRUE`/`FALSE`, `INTERVAL`, `DATE '...'`, single-quoted strings.
- **Casts:** `CAST(x AS t)`; use `toInt`/`toFloat`/`toString`/`toDate`/`toTimestamp`.
- **Predicates:** infix `BETWEEN`, `IS NULL`, `LIKE`, `IN (SELECT ...)`, `EXISTS`. Use two
  comparisons, `isNull`, `like`, `semiJoin`, `antiJoin`.
- **Subqueries:** no `FROM (SELECT ...)`, no correlated subqueries, no `LATERAL`. Compose named query
  definitions instead; a join input may be a query.
- **Aggregation:** `HAVING` (use `where` after `agg`), `FILTER (WHERE ...)` (use `ifThenElse` inside
  the aggregate), `GROUPING SETS`, `DISTINCT ON`, `QUALIFY`.
- **Functions not in §7:** math (`abs`, `round`, `floor`, `ceil`, `sqrt`, `power`, `random`), more
  aggregates (`median`, `stddev`, `string_agg`, `array_agg`, `bool_and`), more windows
  (`first_value`, `last_value`, `ntile`, `percent_rank`, `cume_dist`), `split`, `regexp_*`.
  These may be added as `sql` templates, but only with a signature.
- **Language features:** user types, record values as data, arrays/maps, `let`, `if` statements,
  loops, exceptions, parameters, multiple statements, DDL/DML (`INSERT`, `UPDATE`, `DELETE`,
  `CREATE`), `infixl`/`infixr`, user-defined operators.
- **Escaping:** string interpolation, raw multiline strings, backslash line continuation.

---

## 10. Complete examples

Each block is a whole file. Copy the shape, not just the tokens.

### 10.1 Filter, compute, sort, page

```cagara
users : query { id = int, name = string, age = int, active = bool } =
  table "public" "users"

adults = users
  & where (.age >= 18 && .active)
  & select { id = .id, label = upper .name, next_age = .age + 1 }
  & order [asc .label]
  & limit 10
  & offset 20

main = adults
```

### 10.2 Grouped report with a filter after aggregation

```cagara
orders : query { id = int, user_id = int, amount = float, status = string } =
  table "public" "orders"

paid = orders & where (.status == "paid")

report = paid
  & agg {
      user_id = group .user_id,
      revenue = coalesce 0.0 (sum .amount),
      n = count
    }
  & where (.n >= 5)
  & order [desc .revenue]

main = report
```

### 10.3 Left join, null handling, and matched-row count

```cagara
users : query { id = int, name = string } = table "public" "users"
orders : query { id = int, user_id = int, amount = float } = table "public" "orders"

report = users
  & leftJoin orders (.<id == .>user_id)
  & agg {
      id = group .id,
      name = group .name,
      total = coalesce 0.0 (sum (coalesce 0.0 .amount)),
      matched = coalesce 0 (sum (ifThenElse (isNotNull .user_id) 1 0))
    }
  & order [desc .total]

main = report
```

### 10.4 Top N per group with a window

```cagara
orders : query { id = int, user_id = int, amount = float, ts = timestamp } =
  table "public" "orders"

ranked = orders
  & select {
      .id,
      .user_id,
      .amount,
      rn = rowNumber {partition = [.user_id], order = [desc .amount]}
    }
  & where (.rn <= 3)

main = ranked & select { .id, .user_id, .amount }
```

### 10.5 Conditional aggregation and date bucketing

```cagara
orders : query { id = int, amount = float, status = string, ts = timestamp } =
  table "public" "orders"

monthly = orders
  & agg {
      month = group (truncMonth .ts),
      paid = coalesce 0.0 (sum (ifThenElse (.status == "paid") .amount 0.0)),
      total = coalesce 0.0 (sum .amount),
      n = count
    }
  & order [asc .month]

main = monthly
```

### 10.6 Set operations and deduplication

```cagara
users : query { id = int, name = string } = table "public" "users"
vips : query { id = int, name = string } = table "public" "vips"

everyone = users &! vips
only_vips = users &^ vips
names = users & select { .name } & distinct

main = names
```

### 10.7 A reusable stage and a custom SQL function

```cagara
orders : query { id = int, user_id = int, amount = float, status = string } =
  table "public" "orders"

paidOnly = q => where (.status == "paid") q
shout : expr r string -> expr r string = sql "UPPER($1)"

report = orders
  & paidOnly
  & agg {
      user_id = group .user_id,
      total = coalesce 0.0 (sum .amount),
      n = count
    }
  & order [desc .n]

main = report
```

### 10.8 A self-join with renamed sides

```cagara
employees : query { id = int, name = string, manager_id = int } =
  table "public" "employees"

boss : query { id = int, name = string } = table "public" "employees"

boss_named = boss & select { boss_id = .id, boss_name = .name }

main = employees
  & leftJoin boss_named (.<manager_id == .>boss_id)
  & select { .id, .name, .boss_name }
```

Rename the right side **before** joining whenever both sides have a column of the same name. A shared
name in the join output resolves to the left column, so `select { boss_name = .name }` after the join
would pick the employee's name, not the boss's.

### 10.9 Module layout

```text
# schema.cagara
users : query { id = int, name = string, password_hash = string } =
  table "public" "users"
```

```text
# public.cagara
import "schema.cagara" as s

public_users = s.users & select { .id, display_name = .name }
main = public_users
```

---

## 11. Diagnostics → fixes

The compiler names the column and often the fix. Match the wording, then apply the rule.

| Compiler message (abridged) | Cause | Fix |
|---|---|---|
| `no column \`x\`; available: ...` | typo, or the column was dropped/renamed | use an available name; check the current stage's row |
| `uses a column that is not grouped; wrap it in \`group\` or aggregate it` | bare column in `agg` | `x = group .x` or aggregate it |
| `this argument contains an aggregate; aggregates cannot nest` | `sum (sum .x)` | aggregate in an earlier `agg` stage |
| `field \`x\` is an aggregate; aggregates belong in \`agg\`, not \`select\`` | aggregate in `select`/`update` | move it into `agg` |
| `expected a non-null value, found a \`maybe\`` | operator/aggregate/predicate on nullable | `coalesce`, `??`, `isNull`, `isTrue`, `whereTrue` |
| `only one side is nullable: \`coalesce default x\` takes a \`maybe\`` | `coalesce`/`just` mismatch | `coalesce` only on `maybe`; `just` to add maybe |
| `join predicates must say which input a column comes from` | plain `.x` in a join predicate | `.<x` / `.>x` |
| `\`.<x\` and \`.>x\` ... only be used in a join predicate` | side field after the join | use `.x` on the output row |
| `sort and partition keys must be plain column expressions` | aggregate/window in `order` or spec | compute it in an earlier stage |
| `this argument contains a window function; windows cannot nest` | window where a value is needed | `select` it first, then filter/sort |
| `field \`x\` is a window function; compute it in a \`select\` stage after \`agg\`` | window in `agg` | compute after `agg` in `select` |
| `limit needs a non-negative count, got -1` | negative limit | use a non-negative literal |
| `select needs at least one field` / `agg needs at least one field` | empty record | list at least one field |
| `field \`a\` appears twice` | duplicate field name | rename one |
| `a \`case\` must end with a default arm, written \`true => value\`` | no default | append `true => value` |
| `unknown name \`null\`` | SQL NULL literal | use `just`/`coalesce`/`nullIf` |
| `integer literal is too large (the limit is 9223372036854775807)` | overflow | use a smaller literal |
| `needs a type signature: a \`sql\` template...` | template without signature | add `name : expr r a -> expr r b` |
| `template uses placeholders up to $2 but its signature has 1 ...` | placeholder/arity mismatch | number placeholders `$1..$n` exactly |
| `unknown operator \`&>>\`` | operator not in table | use a listed operator |
| `the columns of table ... are unknown; give its definition a closed type` | table without annotation | add `query { ... }` |
| `ambiguous use of ...` | overloaded name with unconstrained literal | use `toInt`/`toFloat`/`toDate`/`toTimestamp` |
| `type mismatch: expected int, found float` | int/float mix | `toInt` / `toFloat` |
| `cannot apply a value of type int to an argument` | juxtaposition of non-functions (e.g. `1e3`) | use a correct literal/function |
| `field \`x\` of \`select\` must be a column expression or constant, found list int` | list as a value | lists are only keys/`inList` |
| `\`byId\` does not match its signature: no column \`id\`` | open `query r` annotation on a column-reading helper | omit the signature or use a closed row |
| `unknown name \`_&_\`` | prelude disabled | remove `# no prelude` or define your own |

Two habits prevent the rest: give every table an accurate closed annotation, and ask **which phase**
(row, aggregate, window) each value belongs to. Most phase errors are fixed by moving the computation
one stage earlier.

---

## 12. Preflight checklist

Run this in order before returning a file.

1. **Layout:** every definition starts in column 0; every continuation line is indented; no stray
   top-level `;`.
2. **Tables:** each `table` definition has a closed `query { ... }` annotation.
3. **Entry point:** `main` exists and is a query (or the file is intentionally a library).
4. **Columns:** every `.x` is a column of the row at that point; no bare column names.
5. **Row changes:** `select` (replace) vs `update` (merge) vs `omit` (drop) chosen deliberately.
6. **Aggregation:** aggregates only in `agg`; every non-`group` field is an aggregate or constant;
   no nested aggregates; `sum`/`avg`/`min`/`max` wrapped in `coalesce` when a non-null is needed.
7. **Windows:** only in `select`/`update` fields; filter/sort on the named column in a later stage.
8. **Nulls:** predicates and operators see non-null values; every `maybe` is handled.
9. **Joins:** predicate uses `.<` and `.>`; both columns exist on the correct sides; output
   name/shared-column rule understood.
10. **Sets:** operands have identical rows (names, order, types).
11. **Lists:** only sort keys, partition keys, or `inList`; homogeneous.
12. **Conditionals:** `case` ends with `true => value`; arms share a type and phase; conditions are
    non-null `bool`.
13. **Names:** every function is in §7; no invented functions or SQL keywords.
14. **Compile:** `cagara file.cagara` exits 0 and prints exactly one `-- main` statement.

---

## 13. Suggested prompt skeleton for an orchestrator

```text
You are writing Cagara 0.3.1. Read CAGARA.md in full before answering.
Write one complete .cagara file, not a fragment and not SQL.
Use only names and forms listed in CAGARA.md.
Define every table with a closed `query { ... }` annotation.
Use `.column` for columns; `select` replaces the row, `update` merges, `agg` groups.
Handle every `maybe` explicitly.
Finish with `main = <query>` and run the §12 checklist.

Schema (authoritative):
<paste table definitions>

Task:
<one paragraph>

Return only the file contents in one `cagara` fenced block.
If you can run `cagara`, do so and fix until exit 0.
```

For a repo-integrated agent, copy this file to `AGENTS.md` at the project root (or reference it from
one), so the contract is loaded automatically. For a hosted model, paste §0, §5, §7, §11, and §12 as
the system context; the full file is still the safer choice.
