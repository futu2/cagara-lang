# A short Cagara guide

Cagara describes a query as a typed value. A table declaration gives the
compiler the table's name and row schema; it does not connect to a database
or inspect a live catalog.

```haskell
users : query { id = int, name = string, age = int, active = bool } =
  table "public" "users"

orders : query { id = int, user_id = int, amount = float, status = string } =
  table "public" "orders"
```

Column expressions start with a dot. `where` filters rows, `select` chooses
and computes columns, `order` sorts, and `limit` caps the result. The `&`
operator applies each stage to the query on its left:

```haskell
adults = users
  & where (.age >= 18 && .active)
  & select { id = .id, label = upper .name }
  & order [asc .label]
  & limit 20
```

Aggregation uses `group` for output columns that identify each group. Other
fields must be aggregate expressions:

```haskell
by_user = orders
  & agg {
      user_id = group .user_id,
      revenue = coalesce 0.0 (sum .amount),
      orders = count
    }
```

`sum`, `avg`, `min`, and `max` can return `maybe` because SQL can produce
`NULL`. Use `coalesce default value` (or `value ?? default`) before using a
nullable result as an ordinary value. See the [nullability reference](NULLABILITY.md)
for outer joins and nullable aggregate inputs.

Join predicates name their input sides explicitly. After a left join, columns
from the right side may be absent and have `maybe` types:

```haskell
user_orders = users
  & leftJoin orders (.<id == .>user_id)
  & select {
      id = .id,
      amount = coalesce 0.0 .amount
    }
```

`.<id` and `.>user_id` are only used in the join predicate. Outside it, `.id`
refers to the joined output row; when both inputs have the same name, the left
column wins. Give a column a new name with `update` before joining if both
values are needed.

`select` chooses the columns of the output row. A field `{.name}` is short for
`{name = .name}`, and it mixes with computed fields:

```haskell
picked = users & select {.id, label = upper .name}
```

`select` replaces the whole row; `update` merges over it. A name the input
already has keeps its position and takes the new expression, a name it does
not have is appended, and everything else passes through:

```haskell
greeting = users & update { age = .age + 1, display_name = .name }
```

So `update` renames or recomputes a column, while `select` chooses the
columns to publish:

```haskell
public_users = schema.users
  & select {.id, display_name = .name}
```

`agg` and windows do not nest: an `agg` field is one aggregate expression. A
window is an ordinary expression, so it may take part in a computation — but
it is computed per row, so it cannot go in a `where`, a sort key, or a join
predicate. Compute it in its own stage and filter the new column later:

```haskell
ranked = orders & select { id = .id, rn = rowNumber { order = [desc .amount] } }
top = ranked & where (.rn <= 3)
```

The long stage names are the easiest form to learn first. Cagara also has
shorthands: `&?` for `where`, `&=` for `select`, `&*` for `agg`, `&.` for
`order`, and `&-` for `limit`. Join operators include `?` (inner), `<?`
(left), `?>` (right), and `<?>` (full). For example, `q &? .active` means
`q & where .active`.

Compile a file to SQL with `cagara file.cagara`; add `--dialect postgres` to
target a SQL dialect, `--only by_user` to compile one definition, or
`--types` to print inferred types. `cagara fmt file.cagara` formats a source
file. The [examples](../examples) show imports, windows, diagnostics, and
complete query pipelines.

Imports are relative to the importing file, and the prelude is available
automatically. Use an alias to make a module's exported definitions explicit:

```haskell
import "schema.cagara" as schema

public_users = schema.users & select {.id, .name}
```